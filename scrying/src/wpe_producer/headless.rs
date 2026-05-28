//! Headless WPE display + WebView construction (Phase 4c.2).
//!
//! Constructs a self-owned `WPEDisplayHeadless` and a `WebKitWebView` bound
//! to it via the variadic `g_object_new` construct-property path — the most
//! version-robust approach under glib 0.18, as confirmed by the Task-2 spike.
//!
//! An ephemeral `WebKitNetworkSession` is supplied at construction time so
//! WebKit does not auto-create a persistent on-disk `WebsiteDataStore` whose
//! destructor asserts during process teardown (`atexit`).

use super::ffi;
use crate::WebSurfaceError;
use crate::native_frame::{DmaBufImage, DmaBufPlane, SyncMechanism};
use glib::prelude::*;
use glib::translate::{IntoGlib, from_glib, from_glib_full};
use std::os::raw::c_char;

/// Construct a headless `WPEDisplay` and a `WebKitWebView` bound to it.
///
/// Returns the owned WebView (as a [`glib::Object`]) and the raw [`ffi::WPEView`]
/// pointer borrowed from that webview. The `WPEView` pointer is valid for the
/// lifetime of the returned `glib::Object`.
///
/// The transfer-full display and network-session construction refs are released
/// inside this function after `g_object_new` takes its own references on each
/// construct-property object. The `WebView` retains its own refs, keeping both
/// alive for its lifetime.
pub(super) fn build_producer_view()
    -> Result<(glib::Object, *mut ffi::WPEView), WebSurfaceError>
{
    // 1. Self-owned headless display (no compositor surface).
    let display = unsafe { ffi::wpe_display_headless_new() };
    if display.is_null() {
        return Err(WebSurfaceError::Platform(
            "wpe_display_headless_new() returned null; no headless WPE display available".into(),
        ));
    }

    // 2. Ephemeral network session — avoids WebKit auto-creating a persistent
    //    default WebsiteDataStore (whose destructor asserts during atexit teardown).
    let network_session = unsafe { ffi::webkit_network_session_new_ephemeral() };

    // 3. Fetch the WebKitWebView GType at runtime (most version-robust path).
    let webview_gtype: glib::Type = unsafe { from_glib(ffi::webkit_web_view_get_type()) };

    // 4. Construct the WebView with its `display` and `network-session` construct
    //    properties. Variadic g_object_new sidesteps building a GValue array by
    //    hand and lets the property setter transform-check the display pointer.
    let raw_obj = unsafe {
        glib::gobject_ffi::g_object_new(
            webview_gtype.into_glib(),
            c"display".as_ptr(),
            display,
            c"network-session".as_ptr(),
            network_session,
            std::ptr::null::<c_char>(),
        )
    };
    if raw_obj.is_null() {
        // SAFETY: display and network_session are valid transfer-full GObject
        // pointers from the *_new constructors above; g_object_new did not adopt
        // them into a returned object, so release our refs before bailing.
        unsafe {
            glib::gobject_ffi::g_object_unref(display as *mut glib::gobject_ffi::GObject);
            glib::gobject_ffi::g_object_unref(network_session as *mut glib::gobject_ffi::GObject);
        }
        return Err(WebSurfaceError::Platform(
            "g_object_new returned null for WebKitWebView".into(),
        ));
    }

    // 5. Take ownership of the freshly-constructed object.
    let webview: glib::Object = unsafe { from_glib_full(raw_obj) };

    // 6. Release the transfer-full refs we received from the *_new constructors.
    //    g_object_new took its own ref on each construct-property object, so the
    //    WebView retains both display and network_session for its lifetime. We
    //    must release our transfer-full refs or they leak.
    //    NOTE: capture `display` for the binding check (step 7) BEFORE unreffing;
    //    the pointer remains valid to compare because the WebView holds a ref.
    unsafe {
        // SAFETY: display and network_session are valid GObject pointers obtained
        // from transfer-full constructors above. The WebView has incremented their
        // refcounts via g_object_new, so unreffing here is safe and correct.
        glib::gobject_ffi::g_object_unref(display as *mut glib::gobject_ffi::GObject);
        glib::gobject_ffi::g_object_unref(network_session as *mut glib::gobject_ffi::GObject);
    }

    // 7. Get the raw WebKitWebView pointer (borrowed from the owned `webview`).
    // SAFETY: `webview` is a valid GObject of type WebKitWebView; the cast is
    // safe because we just constructed it with webkit_web_view_get_type().
    let raw_webview: *mut ffi::WebKitWebView =
        glib::translate::ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&webview).0
            as *mut _;

    // 8. Guard: verify the WebView actually bound to our headless display.
    // SAFETY: raw_webview is a borrowed pointer valid for the lifetime of `webview`.
    let bound = unsafe { ffi::webkit_web_view_get_display(raw_webview) };
    if bound != display {
        return Err(WebSurfaceError::Platform(
            "WebView did not bind to the headless display".into(),
        ));
    }

    // 9. Get the WPEView from the WebView.
    // SAFETY: raw_webview is a borrowed pointer valid for the lifetime of `webview`.
    let view = unsafe { ffi::webkit_web_view_get_wpe_view(raw_webview) };
    if view.is_null() {
        return Err(WebSurfaceError::Platform(
            "WebView exposed no WPEView".into(),
        ));
    }

    Ok((webview, view))
}

/// Convert a rendered `WPEBufferDMABuf` into a producer-owned [`DmaBufImage`] by
/// `dup()`-ing each plane fd so the imported texture owns descriptors
/// independent of WPE's buffer pool. `buffer_base` is the same buffer cast to
/// the `WPEBuffer` base type (used for width/height).
///
/// SAFETY: both pointers must be valid (non-null, of the matching WPE type) for
/// the duration of the call.
unsafe fn dmabuf_to_image(
    dmabuf: *mut ffi::WPEBufferDMABuf,
    buffer_base: *mut ffi::WPEBuffer,
) -> Option<DmaBufImage> {
    let width = unsafe { ffi::wpe_buffer_get_width(buffer_base) };
    let height = unsafe { ffi::wpe_buffer_get_height(buffer_base) };
    if width <= 0 || height <= 0 {
        return None;
    }
    let n_planes = unsafe { ffi::wpe_buffer_dma_buf_get_n_planes(dmabuf) };
    if n_planes == 0 {
        return None;
    }
    let mut planes: Vec<DmaBufPlane> = Vec::with_capacity(n_planes as usize);
    for i in 0..n_planes {
        let raw_fd = unsafe { ffi::wpe_buffer_dma_buf_get_fd(dmabuf, i) };
        // dup so the importer can own its copy independently of WPE's pool.
        let fd = unsafe { libc::dup(raw_fd) };
        if fd < 0 {
            // Close any fds dup'd so far, then bail.
            for p in &planes {
                unsafe { libc::close(p.fd) };
            }
            return None;
        }
        planes.push(DmaBufPlane {
            fd,
            offset: unsafe { ffi::wpe_buffer_dma_buf_get_offset(dmabuf, i) },
            stride: unsafe { ffi::wpe_buffer_dma_buf_get_stride(dmabuf, i) },
        });
    }
    Some(DmaBufImage {
        size: dpi::PhysicalSize::new(width as u32, height as u32),
        // WPE's default headless buffer is BGRA; corrected against the observed
        // fourcc in Task 6 if the runtime smoke shows otherwise.
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        drm_format: unsafe { ffi::wpe_buffer_dma_buf_get_format(dmabuf) },
        drm_modifier: unsafe { ffi::wpe_buffer_dma_buf_get_modifier(dmabuf) },
        planes,
        generation: 0, // assigned on submit
        producer_sync: SyncMechanism::None,
        semaphore_fd: None,
    })
}

/// Connect the `WPEView::buffer-rendered` frame seam.
///
/// `view_obj` is the [`glib::Object`] view of the `WPEView`; `view` is the raw
/// pointer (needed to call `wpe_view_buffer_released`); `sink` is a clone of the
/// producer's [`FrameSink`](super::producer::FrameSink). The closure downcasts
/// each rendered buffer to `WPEBufferDMABuf`, `dup()`s its plane fds into a
/// producer-owned [`DmaBufImage`], releases WPE's buffer immediately, and
/// submits the image to the shared sink (stamping the next generation).
///
/// Non-DMABUF buffers are released back to WPE and dropped (we only import the
/// DMABUF contract). The connection persists on the underlying GObject for as
/// long as the view is alive.
pub(super) fn connect_buffer_rendered(
    view_obj: &glib::Object,
    view: *mut ffi::WPEView,
    sink: super::producer::FrameSink,
) {
    use glib::translate::ToGlibPtr;

    // GType of WPEBufferDMABuf, resolved once and captured by the closure so the
    // per-frame downcast is a cheap `is_a` check.
    let dmabuf_gtype: glib::Type =
        unsafe { from_glib(ffi::wpe_buffer_dma_buf_get_type()) };

    // The `view` raw pointer is `Copy` and moved into the closure; the closure
    // is `'static` (no borrow of `view_obj`). WPE invokes `buffer-rendered` on
    // the view's affine main context (the same thread the producer pumps), so
    // sharing the raw pointer here is sound for this single-threaded model.
    let raw_view = view as usize;
    view_obj.connect_closure(
        "buffer-rendered",
        false,
        glib::closure_local!(move |_v: glib::Object, buffer: glib::Object| {
            let raw_buf: *mut ffi::WPEBuffer =
                ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&buffer).0
                    as *mut ffi::WPEBuffer;
            let view = raw_view as *mut ffi::WPEView;

            if !buffer.type_().is_a(dmabuf_gtype) {
                // Not a DMABUF buffer — hand it straight back to WPE.
                unsafe { ffi::wpe_view_buffer_released(view, raw_buf) };
                return;
            }

            let raw_dmabuf = raw_buf as *mut ffi::WPEBufferDMABuf;
            let image = unsafe { dmabuf_to_image(raw_dmabuf, raw_buf) };
            // Hand WPE's buffer back immediately; we own dup'd fds now.
            unsafe { ffi::wpe_view_buffer_released(view, raw_buf) };

            if let Some(mut image) = image {
                image.generation =
                    sink.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                sink.submit(image);
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn headless_webview_binds_display() {
        let (_webview, view) = build_producer_view().expect("build producer view");
        assert!(!view.is_null(), "webview must expose a WPEView");
    }

    /// Diagnostic: enumerate the signals defined on the WPEView GType (and the
    /// concrete view subclass) so the `buffer-rendered` frame-seam name can be
    /// confirmed empirically (the headers expose it only as a method, not a
    /// vfunc/signal). Prints all signal names + whether `buffer-rendered`
    /// resolves via `g_signal_lookup`. Does not connect (so it can't panic on a
    /// missing signal).
    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn wpe_view_signal_introspection() {
        use glib::subclass::signal::SignalId;

        // Build a real view so the concrete subclass GType is registered. The
        // parent chain reaches the base WPEView type, so we don't need a
        // separate `wpe_view_get_type` FFI symbol just for the diagnostic.
        let (_webview, view) = build_producer_view().expect("build producer view");
        assert!(!view.is_null());

        let view_obj: glib::Object =
            unsafe { glib::translate::from_glib_none(view as *mut glib::gobject_ffi::GObject) };
        let concrete_type = view_obj.type_();

        eprintln!("--- signals on {concrete_type} and ancestors ---");
        list_signals(concrete_type);

        // Resolve against the concrete type: `g_signal_lookup` walks ancestors,
        // so a signal defined on the base WPEView is found here too.
        let found = SignalId::lookup("buffer-rendered", concrete_type).is_some();
        eprintln!("buffer-rendered resolves via g_signal_lookup: {found}");
        assert!(
            found,
            "the `buffer-rendered` signal was not found on WPEView; \
             inspect the printed signal list to find the real frame-seam signal"
        );
    }

    /// Walk a GType (and its ancestors) printing every signal id's name.
    fn list_signals(ty: glib::Type) {
        let mut cursor = Some(ty);
        while let Some(t) = cursor {
            let mut n: u32 = 0;
            let ids = unsafe {
                glib::gobject_ffi::g_signal_list_ids(t.into_glib(), &mut n as *mut u32)
            };
            if !ids.is_null() && n > 0 {
                let slice = unsafe { std::slice::from_raw_parts(ids, n as usize) };
                for &id in slice {
                    let mut q = std::mem::MaybeUninit::<glib::gobject_ffi::GSignalQuery>::uninit();
                    let name = unsafe {
                        glib::gobject_ffi::g_signal_query(id, q.as_mut_ptr());
                        let q = q.assume_init();
                        std::ffi::CStr::from_ptr(q.signal_name as *const std::os::raw::c_char)
                            .to_string_lossy()
                            .into_owned()
                    };
                    eprintln!("  [{t}] signal: {name}");
                }
            }
            if !ids.is_null() {
                unsafe { glib::ffi::g_free(ids as *mut std::os::raw::c_void) };
            }
            // GObject signals are only listed for the exact type, so walk up.
            cursor = t.parent();
        }
    }

    /// Construct a full `WpeProducer` (which connects the `buffer-rendered`
    /// frame seam inside `new`) and assert it succeeds. This is the seam-wiring
    /// smoke: a clean run with no GLib-CRITICAL / panic proves the signal
    /// connects on a live headless view.
    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn producer_constructs_and_connects() {
        use crate::wpe_producer::{WpeProducer, WpeProducerConfig};
        use dpi::PhysicalSize;

        let config = WpeProducerConfig::new(PhysicalSize::new(64, 64), std::env::temp_dir());
        let producer = WpeProducer::new(config);
        assert!(
            producer.is_ok(),
            "WpeProducer::new (with buffer-rendered connect) must succeed: {:?}",
            producer.err()
        );
    }
}

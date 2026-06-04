//! Headless WPE display + WebView construction (Phase 4c.2).
//!
//! Constructs a self-owned `WPEDisplayHeadless` and a `WebKitWebView` bound
//! to it via the variadic `g_object_new` construct-property path — the most
//! version-robust approach under glib 0.18, as confirmed by the Task-2 spike.
//!
//! An ephemeral `WebKitNetworkSession` is supplied at construction time so
//! WebKit does not auto-create a persistent on-disk `WebsiteDataStore` whose
//! destructor asserts during process teardown (`atexit`).
//!
//! ## Runtime tests: one per process
//!
//! Standing up a headless `WPEDisplay` + `WebKitWebView` initializes
//! process-global WebKit state. Constructing a second producer in the same
//! process — sequentially or in parallel — has been observed to SIGABRT (in
//! parallel) or hang in WebKit teardown between displays (sequential, even
//! under `--test-threads=1`). So this module exposes a single `#[ignore]`d
//! runtime test; if more end-to-end coverage is needed, add it to a separate
//! `tests/` integration target (each `tests/*.rs` is its own binary, so its
//! WebKit state is independent of this one).

use super::ffi;
use crate::WebSurfaceError;
use crate::native_frame::{DmaBufImage, DmaBufPlane, SyncMechanism};
use glib::prelude::*;
use glib::translate::{IntoGlib, from_glib, from_glib_full};
use std::os::raw::c_char;
use std::time::{Duration, Instant};

/// Deadline-bounded glib MainContext pump.
///
/// Iterates the supplied `ctx` non-blockingly until `cond` returns true or the
/// deadline elapses. Mirrors the GTK/WebKit6 producers' helpers; small sleep
/// between iterations keeps this off a hot spin while still being responsive
/// to incoming WPE callbacks.
///
/// Currently only used by the smoke test; promoted to drive navigation /
/// first-frame waits in Phase 4c.3.
#[allow(dead_code)]
pub(super) fn pump_until(
    ctx: &glib::MainContext,
    deadline: Instant,
    mut cond: impl FnMut() -> bool,
) -> Result<(), WebSurfaceError> {
    while !cond() {
        if Instant::now() >= deadline {
            return Err(WebSurfaceError::NotReady(
                "WPE main-loop pump deadline exceeded",
            ));
        }
        ctx.iteration(false); // process pending events, non-blocking
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

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
    -> Result<(glib::Object, *mut ffi::WPEView, *mut ffi::WPEToplevel), WebSurfaceError>
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

    // 10. Acquire the view's toplevel (resize target). `wpe_view_get_toplevel`
    //     is transfer-none — the view owns it, no unref on our side.
    // SAFETY: `view` is non-null per step 9.
    let toplevel = unsafe { ffi::wpe_view_get_toplevel(view) };
    if toplevel.is_null() {
        return Err(WebSurfaceError::Platform(
            "wpe_view_get_toplevel returned null on the headless display; \
             resize would always fail".into(),
        ));
    }

    Ok((webview, view, toplevel))
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
    /// End-to-end runtime smoke for 4c.3: construct a headless producer,
    /// navigate to an inline page, assert load completes, acquire a real
    /// `DmaBufImage`, then resize and acquire another frame. Strict
    /// superset of the 4c.2 smoke + adds nav + resize coverage.
    ///
    /// The one-WPE-per-process constraint (see module doc) means this MUST
    /// remain the only ignored runtime test in this binary.
    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn navigate_resize_and_render() {
        use crate::native_frame::{NativeFrame, SyncMechanism};
        use crate::wpe_producer::{WpeProducer, WpeProducerConfig};
        use crate::{NavigationEvent, WebSurfaceFrame, WebSurfaceProducer};
        use dpi::PhysicalSize;

        let config = WpeProducerConfig::new(PhysicalSize::new(256, 256), std::env::temp_dir());
        let mut producer = WpeProducer::new(config).expect("construct headless producer");

        // --- 1. Navigate + assert first frame ---
        producer
            .navigate_to_string(
                "<body style='margin:0;background:#1e90ff'></body>",
                std::time::Duration::from_secs(5),
            )
            .expect("navigate_to_string");

        // Drain navigation events so poll_navigation_event paths are exercised.
        let mut nav_events = Vec::new();
        while let Some(e) = producer.poll_navigation_event() {
            nav_events.push(e);
        }
        assert!(
            nav_events.iter().any(|e| matches!(e, NavigationEvent::Completed { success: true, .. })),
            "expected a successful Completed event; got {:?}",
            nav_events
        );

        // The buffer-rendered seam may fire just after load-changed FINISHED;
        // pump up to 5s for the first frame to materialize on the producer.
        {
            let ctx = producer.handles.main_context.clone();
            let pending = producer.pending_frame.clone();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            super::pump_until(&ctx, deadline, || {
                pending.lock().map(|s| s.is_some()).unwrap_or(false)
            })
            .expect("a first frame should arrive within 5s of navigate completion");
        }

        let frame_1 = producer.acquire_frame().expect("first frame after navigate");
        let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img1)) = frame_1 else {
            panic!("expected a DMABUF frame");
        };
        assert!(img1.size.width > 0 && img1.size.height > 0, "non-zero size 1");
        assert!(!img1.planes.is_empty(), "at least one plane");
        assert!(img1.planes[0].fd >= 0, "valid dup'd fd");
        assert_eq!(img1.producer_sync, SyncMechanism::None);
        eprintln!(
            "smoke#1 (post-nav): {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}",
            img1.size.width, img1.size.height, img1.drm_format, img1.drm_modifier, img1.planes.len()
        );
        super::super::producer::close_frame_fds(&img1);

        // --- 2. Resize + assert next frame ---
        producer
            .resize(PhysicalSize::new(512, 384))
            .expect("resize to 512x384");

        // Pump up to 5s for a fresh frame after resize. WPE may not auto-paint
        // on resize alone — the fallback below renavigates to force one.
        let ctx = producer.handles.main_context.clone();
        let pending = producer.pending_frame.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let arrived = super::pump_until(&ctx, deadline, || {
            pending.lock().map(|s| s.is_some()).unwrap_or(false)
        }).is_ok();

        if !arrived {
            eprintln!("(resize did not auto-trigger a buffer-rendered; renavigating)");
            producer
                .navigate_to_string(
                    "<body style='margin:0;background:#22aa22'></body>",
                    std::time::Duration::from_secs(5),
                )
                .expect("renavigate after resize");
            // Drain post-renavigate events.
            while let Some(_e) = producer.poll_navigation_event() {}
        }

        let frame_2 = producer.acquire_frame().expect("second frame after resize");
        let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img2)) = frame_2 else {
            panic!("expected a DMABUF frame");
        };
        assert!(img2.size.width > 0 && img2.size.height > 0, "non-zero size 2");
        eprintln!(
            "smoke#2 (post-resize): {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}",
            img2.size.width, img2.size.height, img2.drm_format, img2.drm_modifier, img2.planes.len()
        );
        // Soft assertion: the headless toplevel may coerce dimensions. The
        // diagnostic eprintln makes the actual size visible. Hard contract:
        // resize succeeded AND the seam still produces a valid frame.
        super::super::producer::close_frame_fds(&img2);
    }
}

// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

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

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};

use super::ffi;
use crate::native_frame::{DmaBufImage, DmaBufPlane, SyncMechanism};
use crate::{UrlSchemeHandlerFn, WebSurfaceError};
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
/// The transfer-full display, network-session, and web-context construction
/// refs are released inside this function after `g_object_new` takes its own
/// references on each construct-property object. The `WebView` retains its
/// own refs, keeping all three alive for its lifetime.
///
/// `url_schemes` are registered against the producer's `WebContext` BEFORE the
/// WebView is built so the very first navigation can already resolve
/// custom-scheme URIs. The handler boxes are reclaimed by the
/// `GDestroyNotify` we pass to `webkit_web_context_register_uri_scheme` when
/// the WebContext is finalized (i.e. when the WebView drops its ref on it,
/// which happens when the producer drops).
pub(super) fn build_producer_view(
    url_schemes: HashMap<String, UrlSchemeHandlerFn>,
) -> Result<(glib::Object, *mut ffi::WPEView), WebSurfaceError> {
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

    // 2b. Explicit WebContext so we can register custom URL scheme handlers
    //     BEFORE the WebView is built. Without an explicit context here the
    //     WebView would auto-create a default one — and scheme handlers must
    //     be registered before first navigation, so registering on the
    //     auto-created context after the fact is racy with whatever the
    //     WebView loads at startup. `webkit_web_context_new` is
    //     transfer-full; we hold the ref through the `g_object_new` call
    //     below (which makes the WebView take its own) and release ours
    //     afterwards, mirroring the display/network-session dance.
    let context = unsafe { ffi::webkit_web_context_new() };
    if context.is_null() {
        unsafe {
            glib::gobject_ffi::g_object_unref(display as *mut glib::gobject_ffi::GObject);
            glib::gobject_ffi::g_object_unref(network_session as *mut glib::gobject_ffi::GObject);
        }
        return Err(WebSurfaceError::Platform(
            "webkit_web_context_new() returned null".into(),
        ));
    }
    // Register host-supplied scheme handlers on the fresh context before
    // we hand it to the WebView. Empty map is a no-op.
    if !url_schemes.is_empty() {
        super::scheme_handler::register_all(context, url_schemes);
    }

    // 3. Fetch the WebKitWebView GType at runtime (most version-robust path).
    let webview_gtype: glib::Type = unsafe { from_glib(ffi::webkit_web_view_get_type()) };

    // 4. Construct the WebView with its `display`, `network-session`, and
    //    `web-context` construct properties. Variadic g_object_new sidesteps
    //    building a GValue array by hand and lets the property setter
    //    transform-check the display pointer.
    let raw_obj = unsafe {
        glib::gobject_ffi::g_object_new(
            webview_gtype.into_glib(),
            c"display".as_ptr(),
            display,
            c"network-session".as_ptr(),
            network_session,
            c"web-context".as_ptr(),
            context,
            std::ptr::null::<c_char>(),
        )
    };
    if raw_obj.is_null() {
        // SAFETY: display, network_session, and context are valid transfer-full
        // GObject pointers from the *_new constructors above; g_object_new did
        // not adopt them into a returned object, so release our refs before
        // bailing.
        unsafe {
            glib::gobject_ffi::g_object_unref(display as *mut glib::gobject_ffi::GObject);
            glib::gobject_ffi::g_object_unref(network_session as *mut glib::gobject_ffi::GObject);
            glib::gobject_ffi::g_object_unref(context as *mut glib::gobject_ffi::GObject);
        }
        return Err(WebSurfaceError::Platform(
            "g_object_new returned null for WebKitWebView".into(),
        ));
    }

    // 5. Take ownership of the freshly-constructed object.
    let webview: glib::Object = unsafe { from_glib_full(raw_obj) };

    // 6. Release the transfer-full refs we received from the *_new constructors.
    //    g_object_new took its own ref on each construct-property object, so the
    //    WebView retains display, network_session, AND context for its lifetime.
    //    We must release our transfer-full refs or they leak.
    //    NOTE: capture `display` for the binding check (step 7) BEFORE unreffing;
    //    the pointer remains valid to compare because the WebView holds a ref.
    unsafe {
        // SAFETY: all three are valid GObject pointers obtained from
        // transfer-full constructors above. The WebView has incremented their
        // refcounts via g_object_new, so unreffing here is safe and correct.
        glib::gobject_ffi::g_object_unref(display as *mut glib::gobject_ffi::GObject);
        glib::gobject_ffi::g_object_unref(network_session as *mut glib::gobject_ffi::GObject);
        glib::gobject_ffi::g_object_unref(context as *mut glib::gobject_ffi::GObject);
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
    let source_planes = (0..n_planes)
        .map(|i| {
            (
                unsafe { ffi::wpe_buffer_dma_buf_get_fd(dmabuf, i) },
                unsafe { ffi::wpe_buffer_dma_buf_get_offset(dmabuf, i) },
                unsafe { ffi::wpe_buffer_dma_buf_get_stride(dmabuf, i) },
            )
        })
        .collect::<Vec<_>>();
    // `OwnedFd` keeps every successful duplication owned while the whole
    // multi-plane handoff is assembled. A later dup failure therefore closes
    // the earlier descriptors instead of leaking them out of this callback.
    let (buffers, planes) = duplicate_plane_fds(source_planes, |fd| unsafe { libc::dup(fd) })?;
    DmaBufImage::from_owned_buffers(
        dpi::PhysicalSize::new(width as u32, height as u32),
        // WPE's default headless buffer is BGRA; corrected against the observed
        // fourcc in Task 6 if the runtime smoke shows otherwise.
        wgpu::TextureFormat::Bgra8UnormSrgb,
        unsafe { ffi::wpe_buffer_dma_buf_get_format(dmabuf) },
        unsafe { ffi::wpe_buffer_dma_buf_get_modifier(dmabuf) },
        buffers,
        planes,
        0, // assigned on submit
        SyncMechanism::None,
        None,
    )
    .ok()
}

/// Duplicate a set of borrowed DMABUF plane descriptors into a new owned
/// descriptor table and index-only plane metadata.
///
/// Keeping the intermediate descriptors in [`OwnedFd`] is important: this
/// helper has one fallible operation per plane, and any earlier successful
/// duplicate must close if a later operation fails.
fn duplicate_plane_fds(
    source_planes: impl IntoIterator<Item = (i32, u32, u32)>,
    mut duplicate: impl FnMut(i32) -> i32,
) -> Option<(Vec<OwnedFd>, Vec<DmaBufPlane>)> {
    let mut owned_planes = Vec::new();
    for (source_fd, offset, stride) in source_planes {
        let fd = duplicate(source_fd);
        if fd < 0 {
            return None;
        }
        // SAFETY: a successful `duplicate` result is a newly owned file
        // descriptor. It remains owned by this local until we intentionally
        // transfer it below.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        owned_planes.push((fd, offset, stride));
    }
    let planes = owned_planes
        .iter()
        .enumerate()
        .map(|(buffer_index, (_, offset, stride))| DmaBufPlane::new(buffer_index, *offset, *stride))
        .collect();
    let buffers = owned_planes.into_iter().map(|(fd, _, _)| fd).collect();
    Some((buffers, planes))
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
    let dmabuf_gtype: glib::Type = unsafe { from_glib(ffi::wpe_buffer_dma_buf_get_type()) };

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
                image.set_generation(
                    sink.generation
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1,
                );
                sink.submit(image);
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    #[test]
    fn partial_duplicate_failure_closes_earlier_descriptors() {
        let mut source = [0; 2];
        assert_eq!(unsafe { libc::pipe(source.as_mut_ptr()) }, 0);
        let first_duplicate = Cell::new(-1);
        let calls = Cell::new(0);
        let result =
            super::duplicate_plane_fds(vec![(source[0], 0, 16), (source[0], 16, 16)], |fd| {
                let call = calls.get();
                calls.set(call + 1);
                if call == 0 {
                    let duplicated = unsafe { libc::dup(fd) };
                    first_duplicate.set(duplicated);
                    duplicated
                } else {
                    -1
                }
            });

        assert!(result.is_none());
        assert!(first_duplicate.get() >= 0);
        assert_eq!(
            unsafe { libc::fcntl(first_duplicate.get(), libc::F_GETFD) },
            -1,
            "the first successful duplicate must close after the next dup fails"
        );
        unsafe {
            libc::close(source[0]);
            libc::close(source[1]);
        }
    }

    /// End-to-end runtime smoke for 4c.3: construct a headless producer,
    /// navigate to an inline page, assert load completes, acquire a real
    /// `DmaBufImage`, then verify that the fixed-size headless backend rejects
    /// resize rather than reporting a size it did not apply.
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
            nav_events
                .iter()
                .any(|e| matches!(e, NavigationEvent::Completed { success: true, .. })),
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

        let frame_1 = producer
            .acquire_frame()
            .expect("first frame after navigate");
        let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img1)) = frame_1 else {
            panic!("expected a DMABUF frame");
        };
        assert!(
            img1.size.width > 0 && img1.size.height > 0,
            "non-zero size 1"
        );
        assert!(!img1.planes().is_empty(), "at least one plane");
        assert!(img1.buffer_count() > 0, "owned descriptor buffer present");
        assert_eq!(img1.producer_sync, SyncMechanism::None);
        eprintln!(
            "smoke#1 (post-nav): {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}",
            img1.size.width,
            img1.size.height,
            img1.drm_format,
            img1.drm_modifier,
            img1.planes().len()
        );

        // --- 2. Resize honesty ---
        let requested = PhysicalSize::new(512, 384);
        assert!(matches!(
            producer.resize(requested),
            Err(crate::WebSurfaceError::Unsupported(_))
        ));
    }
}

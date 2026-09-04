// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Blocking variants: [`super::super::WkWebViewProducer::start_capture`]
//! and the run-loop-pumping `resolve_target_window` helper, plus
//! `try_acquire_frame` (the non-blocking acquire used by
//! `acquire_frame` and SCK consumers).
//!
//! These methods pump the main `NSRunLoop` and so cannot be called
//! from inside a host event-loop callback (winit `resumed` /
//! `window_event`) — they would re-enter the host's dispatch and
//! panic. Use the `async_start` siblings from event-loop contexts.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use dpi::PhysicalSize;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetWidth,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSError};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLDevice,
    MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamOutputType, SCWindow,
};

use crate::native_frame::MetalTextureRef as NativeMetalTextureRef;
use crate::{
    HostWgpuContext, InteropBackend, NativeFrame, SyncMechanism, WebSurfaceError, WebSurfaceFrame,
    WebSurfaceMode,
};

use super::super::helpers::pump_until;
use super::super::producer::WkWebViewProducer;
use super::{
    CaptureSignal, CaptureState, LatestSample, SendCFRetained, StreamErrorDelegate,
    StreamOutputDelegate, host_window_pixel_size, make_stream_configuration,
};

impl WkWebViewProducer {
    /// Stand up the ScreenCaptureKit pipeline against the WKWebView's
    /// host window and start streaming `MTLTexture` frames into the
    /// shared sample slot. Idempotent.
    ///
    /// On success, [`Self::acquire_frame`] / [`Self::try_acquire_frame`]
    /// flip from `OverlayOnly` to
    /// `WebSurfaceFrame::Native(NativeFrame::MetalTextureRef(...))`,
    /// and `capabilities().preferred_mode` flips to
    /// [`WebSurfaceMode::ImportedTexture`].
    ///
    /// `host` must be a wgpu device on the Metal backend; the textures
    /// are allocated on `host.device`'s underlying `MTLDevice` so they
    /// can be imported into wgpu without cross-device migration.
    ///
    /// `timeout` bounds both the `SCShareableContent` enumeration and
    /// the `startCaptureWithCompletionHandler:` callback wait.
    ///
    /// Requires the user-facing **Screen Recording** privacy
    /// permission. The first call triggers the system prompt; if
    /// denied, this method returns `Platform(...)`.
    ///
    /// # ⚠️ Blocking — host-event-loop hazard
    ///
    /// Pumps the main `NSRunLoop` twice (once for
    /// `SCShareableContent`, once for
    /// `startCaptureWithCompletionHandler:`). **Do not call from
    /// inside a host event-loop callback** (winit `resumed` /
    /// `window_event`) — the pump re-enters the host's dispatch
    /// and panics. Use [`Self::start_capture_async`] +
    /// [`Self::capture_status`] from event-loop contexts.
    pub fn start_capture(
        &mut self,
        host: HostWgpuContext,
        timeout: Duration,
    ) -> Result<(), WebSurfaceError> {
        if self.capture.is_some() {
            return Ok(());
        }

        let mtm = MainThreadMarker::new().ok_or_else(|| {
            WebSurfaceError::Platform("start_capture must be called on the main thread".into())
        })?;

        if host.backend != InteropBackend::Metal {
            return Err(WebSurfaceError::Platform(format!(
                "start_capture requires a Metal wgpu backend, got {:?}",
                host.backend
            )));
        }

        // Acquire the host's MTLDevice via the wgpu-hal escape hatch so
        // textures we allocate land on the same device the consumer
        // renders against.
        let metal_device: Retained<ProtocolObject<dyn MTLDevice>> = unsafe {
            crate::wgpu_compat::metal_device(&host.device)
        }
        .ok_or_else(|| {
            WebSurfaceError::Platform("host wgpu device is not on the Metal backend".into())
        })?;

        let target_window = self.resolve_target_window(timeout)?;
        // Capture the *full* host window at native pixel
        // resolution; `try_acquire_frame` does the per-frame
        // blit-crop down to the WKWebView's pixel rect.
        let host_window = self.webview.window().ok_or_else(|| {
            WebSurfaceError::Platform(
                "WKWebView is not in a window — start_capture requires the producer's parent NSView to be embedded in an NSWindow".into(),
            )
        })?;
        let window_pixel_size = host_window_pixel_size(&host_window);

        let command_queue = metal_device.newCommandQueue().ok_or_else(|| {
            WebSurfaceError::Platform("MTLDevice::newCommandQueue returned nil".into())
        })?;
        let shared_event = metal_device.newSharedEvent().ok_or_else(|| {
            WebSurfaceError::Platform("MTLDevice::newSharedEvent returned nil".into())
        })?;

        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(
                SCContentFilter::alloc(),
                &target_window,
            )
        };

        let stream_config =
            make_stream_configuration(window_pixel_size, self.config.color_pipeline);

        let stream_error = Arc::new(Mutex::new(None::<String>));
        let error_delegate = StreamErrorDelegate::new(Arc::clone(&stream_error));
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_config,
                Some(ProtocolObject::from_ref(&*error_delegate)),
            )
        };

        let latest: Arc<LatestSample> = Arc::new(Mutex::new(None));
        let samples_received = Arc::new(AtomicU64::new(0));
        let samples_consumed = Arc::new(AtomicU64::new(0));
        let output_delegate =
            StreamOutputDelegate::new(Arc::clone(&latest), Arc::clone(&samples_received));
        let sample_queue = DispatchQueue::new("scrying.wkwebview.sck-sample", None);

        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output_delegate),
                    SCStreamOutputType::Screen,
                    Some(&sample_queue),
                )
                .map_err(|e| {
                    WebSurfaceError::Platform(format!(
                        "addStreamOutput failed: {}",
                        e.localizedDescription()
                    ))
                })?;
        }

        let signal = Arc::new(Mutex::new(CaptureSignal::default()));
        {
            let signal = Arc::clone(&signal);
            let block = RcBlock::new(move |err: *mut NSError| {
                let result = if err.is_null() {
                    Ok(())
                } else {
                    let msg = unsafe { (*err).localizedDescription().to_string() };
                    Err(msg)
                };
                if let Ok(mut s) = signal.lock() {
                    s.result = Some(result);
                }
            });
            unsafe {
                stream.startCaptureWithCompletionHandler(Some(&block));
            }
        }

        // Pump the main run loop until the start completion handler
        // fires (it runs on a background queue but the signal is
        // observed on the main thread via the Mutex).
        match pump_until(timeout, || {
            signal.lock().ok().and_then(|s| s.result.clone())
        }) {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                return Err(WebSurfaceError::Platform(format!(
                    "startCapture failed: {}",
                    msg
                )));
            }
            Err(()) => {
                return Err(WebSurfaceError::Platform(format!(
                    "startCapture did not resolve within {:?}",
                    timeout
                )));
            }
        }

        self.capture = Some(CaptureState {
            metal_device,
            command_queue,
            stream,
            output: output_delegate,
            _error_delegate: error_delegate,
            _sample_queue: sample_queue,
            latest,
            stream_error,
            samples_received,
            samples_consumed,
            last_emitted: None,
            generation: AtomicU64::new(0),
            // The initial config was applied synchronously by
            // `startCaptureWithCompletionHandler:` resolving
            // success — start both counters at 0 (revision == applied).
            config_revision: Arc::new(AtomicU64::new(0)),
            applied_config_revision: Arc::new(AtomicU64::new(0)),
            shared_event,
            next_signal_value: AtomicU64::new(0),
        });

        // Capture is live — flip the advertised capability so consumers
        // know the GPU-handoff path is now preferred over overlay.
        self.capabilities.preferred_mode = WebSurfaceMode::ImportedTexture;
        self.capabilities.imported_texture = crate::native_frame::CapabilityStatus::Supported;
        self.capabilities.reason = "WkWebViewProducer slice B: ScreenCaptureKit → IOSurface → MetalTextureRef capture is live; consumer should render the imported texture each frame.";
        let _ = mtm;
        Ok(())
    }

    /// Walk `SCShareableContent.windows` for the entry whose
    /// `windowID` matches the WKWebView's host window's
    /// `windowNumber`. The first call triggers the **Screen
    /// Recording** privacy prompt.
    pub(super) fn resolve_target_window(
        &self,
        timeout: Duration,
    ) -> Result<Retained<SCWindow>, WebSurfaceError> {
        let host_window =
            self.webview
                .window()
                .ok_or_else(|| WebSurfaceError::Platform(
                    "WKWebView is not in a window — start_capture requires the producer's parent NSView to be embedded in an NSWindow".into(),
                ))?;
        let target_window_number = host_window.windowNumber();
        if target_window_number <= 0 {
            return Err(WebSurfaceError::Platform(
                "host NSWindow has no valid windowNumber".into(),
            ));
        }

        /// Window-resolution result handed off from the async
        /// completion block (which runs on a dispatch queue chosen by
        /// SCK) to the main thread.
        ///
        /// `Retained<SCWindow>` is not `Send` by default, but the
        /// `SCShareableContent` API hands us a fully-formed object
        /// that the system documents as safe to hold across thread
        /// boundaries — same reasoning as `SendCFRetained` above.
        struct WindowQueryResult {
            matched: Option<Retained<SCWindow>>,
            error: Option<String>,
        }
        // SAFETY: SCWindow is a thread-safe descriptor; the SC framework
        // hands these out via an async API explicitly intended to be
        // consumed off the dispatch queue.
        unsafe impl Send for WindowQueryResult {}

        let signal: Arc<Mutex<Option<WindowQueryResult>>> = Arc::new(Mutex::new(None));
        let target_id = target_window_number as u32;
        {
            let signal = Arc::clone(&signal);
            let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
                let result = if !err.is_null() {
                    WindowQueryResult {
                        matched: None,
                        error: Some(unsafe { (*err).localizedDescription().to_string() }),
                    }
                } else if let Some(non_null) = NonNull::new(content) {
                    // SAFETY: SCShareableContent hands us a +0
                    // borrow; retain to extend lifetime past the
                    // callback long enough to walk the windows
                    // array.
                    let content: Option<Retained<SCShareableContent>> =
                        unsafe { Retained::retain(non_null.as_ptr()) };
                    match content {
                        Some(content) => {
                            let windows: Retained<NSArray<SCWindow>> = unsafe { content.windows() };
                            let mut matched: Option<Retained<SCWindow>> = None;
                            for i in 0..windows.count() {
                                let window = windows.objectAtIndex(i);
                                if unsafe { window.windowID() } == target_id {
                                    matched = Some(window);
                                    break;
                                }
                            }
                            WindowQueryResult {
                                matched,
                                error: None,
                            }
                        }
                        None => WindowQueryResult {
                            matched: None,
                            error: Some("Retained<SCShareableContent> failed".into()),
                        },
                    }
                } else {
                    WindowQueryResult {
                        matched: None,
                        error: Some("SCShareableContent was null".into()),
                    }
                };
                if let Ok(mut s) = signal.lock() {
                    *s = Some(result);
                }
            });
            unsafe {
                SCShareableContent::getShareableContentWithCompletionHandler(&block);
            }
        }

        let result = pump_until(timeout, || {
            signal.lock().ok().and_then(|mut s| s.take())
        })
        .map_err(|()| {
            WebSurfaceError::Platform(format!(
                "SCShareableContent did not resolve within {:?} (Screen Recording permission may be denied)",
                timeout
            ))
        })?;

        if let Some(msg) = result.error {
            return Err(WebSurfaceError::Platform(format!(
                "SCShareableContent failed: {}",
                msg
            )));
        }
        result.matched.ok_or_else(|| {
            WebSurfaceError::Platform(format!(
                "no SCWindow matched the WKWebView's host windowNumber {}",
                target_window_number
            ))
        })
    }

    /// Non-blocking acquire. Returns:
    /// - `Ok(Some(WebSurfaceFrame::Native(...)))` if the
    ///   ScreenCaptureKit pipeline has produced a new sample since the
    ///   last call.
    /// - `Ok(None)` if no sample is currently waiting (or capture has
    ///   not been started — in that case the caller should fall back
    ///   to overlay-only rendering via [`Self::acquire_frame`]).
    /// - `Err(...)` if the stream has reported a fatal error since the
    ///   last call.
    pub fn try_acquire_frame(&mut self) -> Result<Option<WebSurfaceFrame>, WebSurfaceError> {
        // Re-apply size if the host window crossed a backing-scale
        // boundary since the last call. Cheap when no change is
        // pending.
        self.flush_pending_dpi_change();
        let Some(capture) = self.capture.as_mut() else {
            return Ok(None);
        };

        if let Ok(mut slot) = capture.stream_error.lock()
            && let Some(msg) = slot.take()
        {
            return Err(WebSurfaceError::Platform(format!(
                "SCStream stopped with error: {}",
                msg
            )));
        }

        let sample = match capture.latest.lock() {
            Ok(mut slot) => match slot.take() {
                Some(SendCFRetained(buffer)) => buffer,
                None => return Ok(None),
            },
            Err(_) => {
                return Err(WebSurfaceError::Platform(
                    "latest-sample lock poisoned".into(),
                ));
            }
        };

        // SCK delivers status-only sample buffers (start, idle,
        // suspended, stopped, blank) that have no `CVImageBuffer`
        // attached — only `SCFrameStatus::Complete` samples carry
        // pixel data. Treat the no-image case as "no frame ready
        // yet" rather than an error so the consumer just polls
        // again on the next tick.
        let Some(image_buffer) = (unsafe { sample.image_buffer() }) else {
            return Ok(None);
        };
        // CVPixelBuffer is a type alias for CVImageBuffer in objc2-core-video,
        // so the SCK screen sample buffer can be used directly here without a
        // downcast.
        let pixel_buffer: &CVPixelBuffer = &image_buffer;

        let iosurface = CVPixelBufferGetIOSurface(Some(pixel_buffer)).ok_or_else(|| {
            WebSurfaceError::Platform(
                "CVPixelBuffer was not IOSurface-backed (configure SCStreamConfiguration to BGRA)"
                    .into(),
            )
        })?;

        let source_width = CVPixelBufferGetWidth(pixel_buffer);
        let source_height = CVPixelBufferGetHeight(pixel_buffer);

        // Drop ambiguous in-flight samples while a configuration
        // change is in flight. `config_revision` ticks the moment
        // we hand SCK a new `SCStreamConfiguration`;
        // `applied_config_revision` catches up only when SCK's
        // completion handler fires. Any sample arriving in that
        // gap could have been encoded under either the old or the
        // new config (Apple doesn't tag CMSampleBuffers with
        // their generating config), so the only safe move is to
        // wait. Once `applied == revision`, future samples are
        // guaranteed to be at the latest config and we accept
        // them blindly — including for color-pipeline changes
        // where dim-match alone wouldn't catch the difference.
        let revision = capture
            .config_revision
            .load(std::sync::atomic::Ordering::Relaxed);
        let applied = capture
            .applied_config_revision
            .load(std::sync::atomic::Ordering::Relaxed);
        if applied < revision {
            return Ok(None);
        }

        // Defense-in-depth dim check. The revision gate above
        // catches samples *during* a config change; this catches
        // anything that slips through (e.g. SCK delivering a
        // sample at a slightly off-axis size due to an upstream
        // bug, or a config-change path that didn't go through
        // `update_capture_for_layout_change`). Cheap to keep.
        let host_window_for_dims = self.webview.window().ok_or_else(|| {
            WebSurfaceError::Platform("WKWebView's host window vanished mid-capture".into())
        })?;
        let expected_dims = super::host_window_pixel_size(&host_window_for_dims);
        if source_width as u32 != expected_dims.width
            || source_height as u32 != expected_dims.height
        {
            return Ok(None);
        }

        // Wrap the IOSurface as a transient source MTLTexture.
        // We don't hand this out — it's the full host-window
        // capture; we blit a sub-rect of it into a webview-sized
        // destination below. Format follows the configured color
        // pipeline (`BGRA8Unorm` for Srgb / DisplayP3, `RGBA16Float`
        // for Hdr16f) — must match what SCK encoded into the
        // IOSurface.
        let metal_format = super::metal_pixel_format_for(self.config.color_pipeline);
        let source_descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                metal_format,
                source_width,
                source_height,
                false,
            )
        };
        source_descriptor.setUsage(MTLTextureUsage::ShaderRead);
        source_descriptor.setStorageMode(MTLStorageMode::Shared);

        let source_texture: Retained<ProtocolObject<dyn MTLTexture>> = capture
            .metal_device
            .newTextureWithDescriptor_iosurface_plane(&source_descriptor, &iosurface, 0)
            .ok_or_else(|| {
                WebSurfaceError::Platform(
                    "MTLDevice::newTextureWithDescriptor:iosurface:plane: returned nil".into(),
                )
            })?;

        // Compute the WKWebView's pixel rect within the source
        // texture. SCK captures the host window's content view at
        // its native pixel dimensions; we set
        // `SCStreamConfiguration::width/height` to match (no
        // scaling), so the source rect is just the webview's
        // window-coords rect × backing scale.
        let host_window = host_window_for_dims;
        let webview_rect_pts = super::webview_window_rect(&self.webview, &host_window);
        let scale = host_window.backingScaleFactor().max(1.0);
        let crop_x = (webview_rect_pts.origin.x * scale).round().max(0.0) as usize;
        let crop_y = (webview_rect_pts.origin.y * scale).round().max(0.0) as usize;
        let crop_w_raw = (webview_rect_pts.size.width * scale).round().max(1.0) as usize;
        let crop_h_raw = (webview_rect_pts.size.height * scale).round().max(1.0) as usize;
        // Clamp to the source texture so a stale layout-rect
        // doesn't request bytes past the IOSurface bounds.
        let crop_w = crop_w_raw.min(source_width.saturating_sub(crop_x));
        let crop_h = crop_h_raw.min(source_height.saturating_sub(crop_y));
        if crop_w == 0 || crop_h == 0 {
            return Ok(None);
        }

        // Allocate a fresh destination texture sized to the
        // webview's pixel rect. Per-frame allocation matches the
        // existing source-texture pattern (cheap on Apple
        // silicon; the IOSurface-backed path already creates a
        // texture per frame). Same pixel format as the source —
        // a Metal blit copies bytes directly between matching
        // formats.
        let dest_descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                metal_format,
                crop_w,
                crop_h,
                false,
            )
        };
        dest_descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
        dest_descriptor.setStorageMode(MTLStorageMode::Shared);

        let dest_texture: Retained<ProtocolObject<dyn MTLTexture>> = capture
            .metal_device
            .newTextureWithDescriptor(&dest_descriptor)
            .ok_or_else(|| {
                WebSurfaceError::Platform(
                    "MTLDevice::newTextureWithDescriptor: (dest) returned nil".into(),
                )
            })?;

        // Encode the blit. Source origin is the webview's pixel
        // offset within the captured window; dest origin is the
        // top-left of the cropped texture.
        let cmd_buf = capture.command_queue.commandBuffer().ok_or_else(|| {
            WebSurfaceError::Platform("MTLCommandQueue::commandBuffer returned nil".into())
        })?;
        let blit = cmd_buf.blitCommandEncoder().ok_or_else(|| {
            WebSurfaceError::Platform("MTLCommandBuffer::blitCommandEncoder returned nil".into())
        })?;
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                &source_texture,
                0,
                0,
                objc2_metal::MTLOrigin { x: crop_x, y: crop_y, z: 0 },
                objc2_metal::MTLSize { width: crop_w, height: crop_h, depth: 1 },
                &dest_texture,
                0,
                0,
                objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            );
        }
        blit.endEncoding();
        // Encode an `MTLSharedEvent` signal *after* the blit so
        // the GPU advances `event.signaledValue` to
        // `signal_value` only once the per-frame copy is fully
        // complete. Consumers that opt in to explicit-event
        // synchronization wait on this value via
        // `MTLCommandBuffer::encodeWaitForEvent:value:` on their
        // own queue. Producers without an explicit-sync consumer
        // pay only the cost of a u64 GPU write — implicit
        // IOSurface coherence still does the work.
        let signal_value = capture.next_signal_value.fetch_add(1, Ordering::Relaxed) + 1;
        cmd_buf.encodeSignalEvent_value(
            ProtocolObject::from_ref(&*capture.shared_event),
            signal_value,
        );
        cmd_buf.commit();
        // CPU-side wait until the blit's GPU work finishes before
        // we hand the destination texture out. Necessary for
        // correctness: the destination is `newTextureWithDescriptor`
        // (not IOSurface-backed), and the consumer's wgpu queue
        // is a separate `MTLCommandQueue` from this producer's.
        // Metal does *not* implicitly synchronize work across
        // queues, even when both are on the same device. Without
        // this wait, the consumer can sample the destination
        // before the blit's writes land. IOSurface coherence
        // covers the *source* texture (IOSurface-backed, implicit
        // cross-queue coherence per Apple) but not a non-IOSurface
        // destination. ~1ms stall per acquire on Apple silicon —
        // measurable but small at 60fps; the medium-term refactor
        // is to insert a consumer-side `encodeWaitForEvent:value:`
        // via the wgpu-hal Metal escape against
        // `metal_shared_event()` + the per-frame `signal_value`,
        // which removes the CPU stall entirely. The producer-side
        // signal is already encoded above, so the upgrade is a
        // consumer-side-only change.
        cmd_buf.waitUntilCompleted();

        let frame_texture = unsafe { Retained::retain(Retained::as_ptr(&dest_texture)) }
            .ok_or_else(|| {
                WebSurfaceError::Platform("failed to retain captured MTLTexture".into())
            })?;
        let frame = NativeMetalTextureRef::from_retained(
            PhysicalSize::new(crop_w as u32, crop_h as u32),
            super::wgpu_format_for(self.config.color_pipeline),
            capture.generation.fetch_add(1, Ordering::Relaxed),
            // The producer encodes an `MTLSharedEvent` signal at
            // `signal_value` after each frame's blit; consumers
            // that need explicit sync wait on
            // `producer.metal_shared_event()` for that value via
            // `encodeWaitForEvent:value:`. IOSurface coherence
            // still covers the implicit-sync path on Apple silicon
            // — the explicit signal is opt-in surface for
            // consumers who want to interleave their own Metal
            // queue with the producer's blit.
            SyncMechanism::ExplicitMetalEvent,
            signal_value,
            frame_texture,
        );

        // Keep the destination texture alive past this function.
        // The consumer's importer
        // (`scrying::native_frame::metal::import`) will retain its
        // own reference before our `last_emitted` is overwritten on
        // the next `try_acquire_frame`, so consumers must consume
        // each frame before requesting the next.
        capture.last_emitted = Some(dest_texture);
        capture.samples_consumed.fetch_add(1, Ordering::Relaxed);

        Ok(Some(WebSurfaceFrame::Native(NativeFrame::MetalTextureRef(
            frame,
        ))))
    }
}

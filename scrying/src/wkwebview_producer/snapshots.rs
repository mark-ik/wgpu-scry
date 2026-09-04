// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! `WKWebView.takeSnapshotWithConfiguration:` wrappers — both the
//! non-blocking pair ([`super::WkWebViewProducer::request_snapshot`] /
//! [`super::WkWebViewProducer::poll_snapshot`]) and the blocking
//! [`super::WkWebViewProducer::capture_cpu_snapshot`] convenience.
//! Decoding TIFF → RGBA happens here too so callers don't have to
//! pull `image` directly.

use std::ptr::NonNull;
use std::sync::Arc;

use block2::RcBlock;
use dpi::PhysicalSize;
use objc2::rc::Retained;
use objc2_app_kit::NSImage;
use objc2_foundation::{MainThreadMarker, NSError};
use objc2_web_kit::WKSnapshotConfiguration;

use crate::{WebSurfaceError, WebSurfaceFrame};

use super::helpers::pump_until;
use super::producer::{PendingSnapshot, WkWebViewProducer};

impl WkWebViewProducer {
    /// Non-blocking variant of [`Self::capture_cpu_snapshot`].
    ///
    /// Kicks off `takeSnapshotWithConfiguration:completionHandler:`
    /// and returns immediately. The result is buffered in a
    /// most-recent slot drained via [`Self::poll_snapshot`]. Pair
    /// this with [`Self::poll_snapshot`] from a host event-loop
    /// callback (e.g. winit's `window_event` / `about_to_wait`) so
    /// the host can drive snapshot capture without blocking on the
    /// main run loop.
    ///
    /// Multiple calls in flight are allowed; only the most recent
    /// completion is preserved (older completions overwrite each
    /// other before the consumer polls).
    pub fn request_snapshot(&mut self) -> Result<(), WebSurfaceError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            WebSurfaceError::Platform("request_snapshot must be called on the main thread".into())
        })?;

        let slot = Arc::clone(&self.pending_snapshot);
        let block = RcBlock::new(move |image: *mut NSImage, err: *mut NSError| {
            let result = if !err.is_null() {
                PendingSnapshot::Failed(unsafe { (*err).localizedDescription().to_string() })
            } else if let Some(non_null) = NonNull::new(image) {
                // WKWebView invokes this completion on the main thread. Copy
                // the image's TIFF data while the callback still owns its +0
                // NSImage borrow, so the cross-thread result slot contains
                // plain bytes rather than an AppKit object.
                let image = unsafe { non_null.as_ref() };
                match image.TIFFRepresentation() {
                    Some(data) => PendingSnapshot::Tiff(data.to_vec()),
                    None => PendingSnapshot::Failed(
                        "snapshot NSImage has no TIFF representation".into(),
                    ),
                }
            } else {
                PendingSnapshot::Failed("WKWebView.takeSnapshot returned nil image".into())
            };
            if let Ok(mut s) = slot.lock() {
                *s = Some(result);
            }
        });

        let snapshot_config = unsafe { WKSnapshotConfiguration::new(mtm) };
        unsafe {
            self.webview()
                .takeSnapshotWithConfiguration_completionHandler(Some(&snapshot_config), &block);
        }
        Ok(())
    }

    /// Drain the most recently completed [`Self::request_snapshot`]
    /// result. Returns `None` until a snapshot is ready,
    /// `Some(Ok(WebSurfaceFrame::CpuRgba {..}))` once decoded, or
    /// `Some(Err(...))` on snapshot or decode failure.
    ///
    /// Decoding is performed lazily here (not in the completion
    /// handler) so the main-thread completion callback finishes fast.
    pub fn poll_snapshot(&mut self) -> Option<Result<WebSurfaceFrame, WebSurfaceError>> {
        let pending = self
            .pending_snapshot
            .lock()
            .ok()
            .and_then(|mut s| s.take())?;
        match pending {
            PendingSnapshot::Failed(msg) => Some(Err(WebSurfaceError::Platform(format!(
                "snapshot failed: {msg}"
            )))),
            PendingSnapshot::Tiff(tiff_bytes) => Some(self.decode_tiff_snapshot(tiff_bytes)),
        }
    }

    fn decode_tiff_snapshot(
        &mut self,
        tiff_bytes: Vec<u8>,
    ) -> Result<WebSurfaceFrame, WebSurfaceError> {
        let rgba = image::load_from_memory_with_format(&tiff_bytes, image::ImageFormat::Tiff)
            .map_err(|e| WebSurfaceError::Platform(format!("failed to decode TIFF snapshot: {e}")))?
            .to_rgba8();
        let size = PhysicalSize::new(rgba.width(), rgba.height());
        let generation = self.snapshot_generation;
        self.snapshot_generation = self.snapshot_generation.wrapping_add(1);
        Ok(WebSurfaceFrame::CpuRgba {
            size,
            pixels: rgba,
            generation,
        })
    }

    /// Acquire a content-pixel snapshot via
    /// `WKWebView.takeSnapshotWithConfiguration:completionHandler:` and
    /// decode it into an `image::RgbaImage`.
    ///
    /// Independent of [`Self::start_capture`] — works whether or not
    /// the ScreenCaptureKit pipeline has been started, and does not
    /// require the Screen Recording privacy permission. Useful for
    /// thumbnails, layout-debug captures, and verifying the WebView is
    /// actually rendering before standing up the SCK path.
    ///
    /// # ⚠️ Blocking — host-event-loop hazard
    ///
    /// Pumps the main `NSRunLoop` until the snapshot's completion
    /// handler fires or `config.frame_timeout` elapses. **Do not
    /// call from inside a host event-loop callback** (winit
    /// `resumed` / `window_event`) — the pump re-enters the host's
    /// dispatch and panics. Prefer the non-blocking
    /// [`Self::request_snapshot`] / [`Self::poll_snapshot`] pair
    /// from event-loop contexts.
    pub fn capture_cpu_snapshot(&mut self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            WebSurfaceError::Platform(
                "capture_cpu_snapshot must be called on the main thread".into(),
            )
        })?;

        struct SnapshotResult {
            image: Option<Retained<NSImage>>,
            error: Option<String>,
        }
        // SAFETY: `Retained<NSImage>` is not auto-`Send`, but
        // `WKWebView.takeSnapshot` documents that the completion
        // handler runs on the main thread — same thread that
        // observes the slot — so the cross-thread `Send` is satisfied
        // trivially. The `unsafe impl` is for clippy's benefit.
        unsafe impl Send for SnapshotResult {}

        let timeout = self.config.frame_timeout;
        let signal: Arc<std::sync::Mutex<Option<SnapshotResult>>> =
            Arc::new(std::sync::Mutex::new(None));
        {
            let signal = Arc::clone(&signal);
            let block = RcBlock::new(move |image: *mut NSImage, err: *mut NSError| {
                let result = if !err.is_null() {
                    SnapshotResult {
                        image: None,
                        error: Some(unsafe { (*err).localizedDescription().to_string() }),
                    }
                } else if let Some(non_null) = NonNull::new(image) {
                    let retained = unsafe { Retained::retain(non_null.as_ptr()) };
                    SnapshotResult {
                        image: retained,
                        error: None,
                    }
                } else {
                    SnapshotResult {
                        image: None,
                        error: Some("WKWebView.takeSnapshot returned nil image".into()),
                    }
                };
                if let Ok(mut s) = signal.lock() {
                    *s = Some(result);
                }
            });
            let snapshot_config = unsafe { WKSnapshotConfiguration::new(mtm) };
            unsafe {
                self.webview()
                    .takeSnapshotWithConfiguration_completionHandler(
                        Some(&snapshot_config),
                        &block,
                    );
            }
        }

        let result = pump_until(timeout, || signal.lock().ok().and_then(|mut s| s.take()))
            .map_err(|()| {
                WebSurfaceError::Platform(format!(
                    "takeSnapshot did not resolve within {:?}",
                    timeout
                ))
            })?;

        if let Some(msg) = result.error {
            return Err(WebSurfaceError::Platform(format!(
                "takeSnapshot failed: {msg}"
            )));
        }
        let ns_image = result
            .image
            .ok_or_else(|| WebSurfaceError::Platform("snapshot returned no image".into()))?;
        let tiff_data = ns_image.TIFFRepresentation().ok_or_else(|| {
            WebSurfaceError::Platform("NSImage has no TIFF representation".into())
        })?;
        self.decode_tiff_snapshot(tiff_data.to_vec())
    }
}

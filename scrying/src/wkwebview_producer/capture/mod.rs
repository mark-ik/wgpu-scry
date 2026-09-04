// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! ScreenCaptureKit pipeline state, delegates, and the lazy
//! `stop_capture` teardown method. Lifecycle entry points
//! ([`super::WkWebViewProducer::start_capture`] and
//! [`super::WkWebViewProducer::start_capture_async`]) live in the
//! `blocking` / `async_start` siblings.

use std::ptr::NonNull;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use dispatch2::{DispatchQueue, DispatchRetained};
use dpi::PhysicalSize;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::kCVPixelFormatType_32BGRA;
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_metal::{MTLCommandQueue, MTLDevice, MTLSharedEvent};
use objc2_screen_capture_kit::{
    SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};

use crate::WebSurfaceMode;

use super::producer::WkWebViewProducer;

mod async_start;
mod blocking;

/// The one Core Foundation object this module transfers from SCK's
/// sample queue to the main-thread consumer.
///
/// `CMSampleBuffer` and the CF types it transitively references
/// (`CVImageBuffer`, `IOSurfaceRef`) are documented thread-safe by
/// Apple — retain/release is atomic and the underlying data is
/// immutable from the consumer's perspective. The objc2-core-foundation
/// crate is conservative and doesn't auto-derive `Send` for
/// `CFRetained<T>`. This wrapper intentionally cannot be used for
/// arbitrary CF objects: only a retained sample buffer crosses this
/// boundary.
pub(super) struct QueuedSampleBuffer(pub(super) CFRetained<CMSampleBuffer>);
// SAFETY: Core Media documents CMSampleBuffer as thread-safe. The
// producer transfers the retained buffer from SCK's sample queue into a
// mutex and consumes it only on the main thread.
unsafe impl Send for QueuedSampleBuffer {}

/// Latest screen-capture sample handed off from the
/// `SCStreamOutput::stream:didOutputSampleBuffer:ofType:` callback
/// (which fires on a background dispatch queue) to `try_acquire_frame`
/// on the main thread. Only the most recent sample is kept; older
/// samples are dropped on overwrite.
pub(super) type LatestSample = Mutex<Option<QueuedSampleBuffer>>;

/// State the SCK output delegate writes to from the background
/// dispatch queue. Bundles the latest-sample slot with a delivery
/// counter so [`CaptureMetrics`] can report SCK push cadence.
pub(super) struct OutputDelegateState {
    pub(super) latest: Arc<LatestSample>,
    /// Total Screen-typed samples SCK has delivered to this stream
    /// since `start_capture` resolved. Includes samples that
    /// `try_acquire_frame` later drops via the dim-match guard or
    /// overwrites in `LatestSample` before the consumer polls.
    pub(super) samples_received: Arc<std::sync::atomic::AtomicU64>,
}

/// Live ScreenCaptureKit pipeline counters. Read via
/// [`super::WkWebViewProducer::capture_metrics`]. `Default` if no
/// capture is active.
///
/// `samples_received` is incremented on the SCK background dispatch
/// queue every time the stream delivers a `Screen`-typed sample.
/// `samples_consumed` is incremented on the main thread for every
/// `try_acquire_frame` call that returns `Ok(Some(...))` — i.e. every
/// frame the consumer actually got. Their delta is the drop /
/// dim-mismatch / no-imaging-payload count.
#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureMetrics {
    pub samples_received: u64,
    pub samples_consumed: u64,
}

#[derive(Default)]
pub(super) struct CaptureSignal {
    /// `Some(Ok(()))` once `startCaptureWithCompletionHandler:` /
    /// `stopCaptureWithCompletionHandler:` resolves, `Some(Err(msg))`
    /// on error, `None` while pending.
    pub(super) result: Option<Result<(), String>>,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `StreamOutputDelegate` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[ivars = OutputDelegateState]
    pub(super) struct StreamOutputDelegate;

    unsafe impl NSObjectProtocol for StreamOutputDelegate {}

    // SAFETY: signature matches Apple's `SCStreamOutput` protocol.
    unsafe impl SCStreamOutput for StreamOutputDelegate {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Screen {
                return;
            }
            let state = self.ivars();
            state
                .samples_received
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Retain the sample; the protocol contract is that the
            // callee must retain if it wants to outlive this call.
            let retained = unsafe { CFRetained::retain(NonNull::from(sample_buffer)) };
            if let Ok(mut slot) = state.latest.lock() {
                *slot = Some(QueuedSampleBuffer(retained));
            }
        }
    }
);

impl StreamOutputDelegate {
    pub(super) fn new(
        latest: Arc<LatestSample>,
        samples_received: Arc<std::sync::atomic::AtomicU64>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputDelegateState {
            latest,
            samples_received,
        });
        // SAFETY: NSObject's `init` returns a valid initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `StreamErrorDelegate` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[ivars = Arc<Mutex<Option<String>>>]
    pub(super) struct StreamErrorDelegate;

    unsafe impl NSObjectProtocol for StreamErrorDelegate {}

    unsafe impl SCStreamDelegate for StreamErrorDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            if let Ok(mut slot) = self.ivars().lock() {
                *slot = Some(error.localizedDescription().to_string());
            }
        }
    }
);

impl StreamErrorDelegate {
    pub(super) fn new(error_slot: Arc<Mutex<Option<String>>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(error_slot);
        unsafe { msg_send![super(this), init] }
    }
}

/// Cross-thread observable status of the ScreenCaptureKit pipeline,
/// reported by [`super::WkWebViewProducer::capture_status`] so
/// non-blocking consumers (e.g. winit hosts) can poll instead of
/// blocking on the main run loop.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CaptureStatus {
    /// `start_capture_async` has not been called yet (or `stop_capture`
    /// reset the state machine).
    Idle,
    /// `start_capture_async` was called but neither
    /// `SCShareableContent` nor `startCaptureWithCompletionHandler:`
    /// have resolved yet.
    Starting,
    /// Capture is live; `try_acquire_frame` / `acquire_frame` will
    /// emit `Native` frames.
    Live,
    /// The async start failed at some stage. The consumer can call
    /// `start_capture_async` again to retry.
    Failed(String),
}

/// Internal state machine slot for the async start-capture flow.
/// Held behind `Arc<Mutex<...>>` so the SCK completion blocks
/// (which fire on a private background queue) can advance it without
/// touching the producer's `&mut self`.
pub(super) enum PendingCaptureSlot {
    Idle,
    Starting,
    Ready(CaptureStateForMainThread),
    Failed(String),
}

/// A fully initialized capture state waiting to be installed by
/// [`super::WkWebViewProducer::capture_status`] on the main thread.
///
/// This is a one-way ownership handoff from ScreenCaptureKit's start
/// completion queue. `CaptureState` contains only SCK, Metal, dispatch,
/// and synchronization objects, never AppKit or WebKit objects. The state
/// is removed from the mutex and installed into the non-Send producer before
/// it is used.
pub(super) struct CaptureStateForMainThread(pub(super) CaptureState);
// SAFETY: see `CaptureStateForMainThread`'s concrete handoff contract.
unsafe impl Send for CaptureStateForMainThread {}

/// Metal resources allocated from the host device before entering
/// ScreenCaptureKit's asynchronous start path.
///
/// This narrowly permits the three Metal resources to travel into SCK's
/// completion queue. They are retained Objective-C objects with atomic
/// lifetime management; their use remains confined to the capture pipeline
/// and the state is handed back to the main-thread producer.
pub(super) struct AsyncMetalResources {
    pub(super) metal_device: Retained<ProtocolObject<dyn MTLDevice>>,
    pub(super) command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pub(super) shared_event: Retained<ProtocolObject<dyn MTLSharedEvent>>,
}
// SAFETY: the captured MTLDevice, MTLCommandQueue, and MTLSharedEvent are
// documented reference-counted Metal objects. This wrapper is used only by
// the SCK start completion chain and cannot carry arbitrary Objective-C
// objects.
unsafe impl Send for AsyncMetalResources {}

/// The concrete SCK/Metal state retained while `startCapture` is pending.
/// It exists only inside the nested completion block and is converted into a
/// [`CaptureStateForMainThread`] after a successful start.
pub(super) struct CaptureStartResources(pub(super) InProgressCaptureState);
// SAFETY: this bundle contains only the concrete SCK, Metal, dispatch, and
// synchronization resources listed in `InProgressCaptureState`; it excludes
// AppKit and WebKit objects and is transferred once to the start completion.
unsafe impl Send for CaptureStartResources {}

/// Captured-by-block bag of all the SCK pieces the inner
/// `startCaptureWithCompletionHandler:` block needs to assemble a
/// [`CaptureState`] when the stream goes live.
pub(super) struct InProgressCaptureState {
    pub(super) metal_device: Retained<ProtocolObject<dyn MTLDevice>>,
    pub(super) command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pub(super) stream: Retained<SCStream>,
    pub(super) output: Retained<StreamOutputDelegate>,
    pub(super) error_delegate: Retained<StreamErrorDelegate>,
    pub(super) sample_queue: DispatchRetained<DispatchQueue>,
    pub(super) latest: Arc<LatestSample>,
    pub(super) stream_error: Arc<Mutex<Option<String>>>,
    pub(super) samples_received: Arc<AtomicU64>,
    pub(super) samples_consumed: Arc<AtomicU64>,
    pub(super) config_revision: Arc<AtomicU64>,
    pub(super) applied_config_revision: Arc<AtomicU64>,
    pub(super) configuration_error: Arc<Mutex<Option<ConfigurationFailure>>>,
    pub(super) shared_event: Retained<ProtocolObject<dyn MTLSharedEvent>>,
}

/// A failed `SCStream::updateConfiguration:` callback. Kept with the
/// requested revision so an older failure cannot poison a newer layout
/// request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConfigurationFailure {
    pub(super) revision: u64,
    pub(super) message: String,
}

/// Complete one asynchronous stream-configuration request.
///
/// A failed callback deliberately does *not* advance `applied`: frames are
/// ambiguous until a later request succeeds, and `try_acquire_frame` reports
/// the retained error rather than pretending capture remains healthy.
pub(super) fn complete_configuration_update(
    requested: &AtomicU64,
    applied: &AtomicU64,
    failure: &Mutex<Option<ConfigurationFailure>>,
    revision: u64,
    error: Option<String>,
) {
    use std::sync::atomic::Ordering;

    // A more recent resize/DPI update superseded this callback. Its result
    // says nothing about the configuration consumers now need.
    if requested.load(Ordering::Acquire) != revision {
        return;
    }

    if let Some(message) = error {
        if let Ok(mut slot) = failure.lock() {
            *slot = Some(ConfigurationFailure { revision, message });
        }
        return;
    }

    // Keep the acknowledgement monotonic even if callbacks arrive out of
    // order. Only a successful callback may open the sample gate.
    applied.fetch_max(revision, Ordering::Release);
}

/// Helper used by SCK completion blocks to update the shared
/// [`PendingCaptureSlot`]. Lock-poisoning failures are silently
/// dropped because there's no useful recovery path from a callback —
/// the next [`super::WkWebViewProducer::capture_status`] poll will
/// surface the prior state (or `Failed` if a poisoned lock makes
/// things inconsistent).
pub(super) fn write_pending(pending: &Arc<Mutex<PendingCaptureSlot>>, state: PendingCaptureSlot) {
    if let Ok(mut s) = pending.lock() {
        *s = state;
    }
}

/// State held while ScreenCaptureKit is actively streaming.
pub(super) struct CaptureState {
    /// Strong reference to the host wgpu device's `MTLDevice`. Used to
    /// allocate IOSurface-backed `MTLTexture`s on the same device the
    /// consumer renders against (no cross-device migration).
    pub(super) metal_device: Retained<ProtocolObject<dyn MTLDevice>>,
    /// Command queue for the per-frame Metal blit that crops the
    /// full-window captured texture down to the WKWebView's pixel
    /// rect. Allocated once at `start_capture` time on
    /// `metal_device` and reused across frames.
    pub(super) command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pub(super) stream: Retained<SCStream>,
    pub(super) output: Retained<StreamOutputDelegate>,
    pub(super) _error_delegate: Retained<StreamErrorDelegate>,
    pub(super) _sample_queue: DispatchRetained<DispatchQueue>,
    pub(super) latest: Arc<LatestSample>,
    /// Surfaced via [`StreamErrorDelegate`] when the stream stops
    /// unexpectedly (e.g., capture target window closed). Inspected
    /// from `try_acquire_frame` so the consumer learns the stream is
    /// dead.
    pub(super) stream_error: Arc<Mutex<Option<String>>>,
    /// Shared with [`StreamOutputDelegate`]; incremented on the SCK
    /// background dispatch queue when a `Screen`-typed sample
    /// arrives. Read by [`super::WkWebViewProducer::capture_metrics`].
    pub(super) samples_received: Arc<AtomicU64>,
    /// Incremented on the main thread inside `try_acquire_frame`
    /// when it returns `Ok(Some(...))` to the consumer. Read by
    /// [`super::WkWebViewProducer::capture_metrics`].
    pub(super) samples_consumed: Arc<AtomicU64>,
    /// Most-recently-emitted MTLTexture. The producer keeps it alive
    /// here because [`crate::native_frame::MetalTextureRef::raw_metal_texture`]
    /// is a raw pointer; the consumer's [`crate::native_frame`]
    /// importer re-retains the object during import. Replaced on
    /// each successful `try_acquire_frame`.
    pub(super) last_emitted:
        Option<Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLTexture>>>,
    pub(super) generation: AtomicU64,
    /// `MTLSharedEvent` allocated against `metal_device` and
    /// signalled inside `try_acquire_frame`'s per-frame blit
    /// command buffer. Consumers can wait on monotonic
    /// `signal_value`s via
    /// `MTLCommandBuffer::encodeWaitForEvent:value:` on their own
    /// queue before sampling. Today the IOSurface coherence story
    /// makes this redundant on Apple silicon, but exposing the
    /// signal flips `producer_sync` to
    /// `ExplicitMetalEvent` so consumers that *do* want explicit
    /// sync (e.g. for a future `MTLSharedEvent`-based
    /// `InteropSynchronizer`, or for cross-queue ordering with
    /// non-wgpu Metal work) have the handle and value to use.
    pub(super) shared_event: Retained<ProtocolObject<dyn MTLSharedEvent>>,
    /// Monotonic counter for the `MTLSharedEvent` signal value.
    /// Bumped *before* encoding the signal so each frame gets a
    /// fresh value the consumer can wait on; the value the
    /// producer just signalled at lives in
    /// [`crate::native_frame::MetalTextureRef::signal_value`].
    pub(super) next_signal_value: AtomicU64,
    /// Monotonic counter incremented every time we hand SCK a new
    /// `SCStreamConfiguration` (resize, DPI flip, color-pipeline
    /// adaptation). Compared against [`Self::applied_config_revision`]
    /// to detect "we've asked for a new config but SCK hasn't
    /// confirmed application yet, so any sample arriving in this
    /// window is ambiguous (could be old, could be new) — drop it."
    /// Sole writer is `update_capture_for_layout_change`; the
    /// completion-handler block bumps `applied` to match.
    pub(super) config_revision: Arc<AtomicU64>,
    /// Bumped from the SCK background queue inside the completion
    /// handler of `updateConfiguration_completionHandler`. Once it
    /// equals `config_revision`, samples are guaranteed to be at
    /// the latest configuration. While unequal,
    /// [`super::super::WkWebViewProducer::try_acquire_frame`]
    /// returns `Ok(None)` to drop the in-flight ambiguous samples.
    /// Atomic because the completion fires off-main.
    pub(super) applied_config_revision: Arc<AtomicU64>,
    /// Most-recent failure for the current configuration revision.
    /// `try_acquire_frame` reports this instead of silently returning
    /// an endless sequence of `None` values after SCK rejects a resize.
    pub(super) configuration_error: Arc<Mutex<Option<ConfigurationFailure>>>,
}

/// Build the [`SCStreamConfiguration`] used by both
/// [`super::WkWebViewProducer::start_capture`] and live resizes.
///
/// `window_pixel_size` is the full host window's pixel
/// dimensions (window-points × backing-scale) — *not* the
/// WKWebView's pixel size. SCK's `initWithDesktopIndependentWindow:`
/// filter captures the entire window unconditionally; the
/// `setWidth:` / `setHeight:` properties just control the IOSurface
/// dimensions the captured pixels are scaled into. We deliberately
/// match output to the source size so no scaling happens, and the
/// per-frame crop in `try_acquire_frame` blits the WKWebView's
/// pixel rect from this full-window texture into a webview-sized
/// destination. Apple's `setSourceRect:` is ignored for
/// single-window filters (per Apple's
/// [sourceRect docs](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/3919829-sourcerect));
/// the Metal-blit crop is what actually limits the imported
/// texture to webview pixels.
pub(super) fn make_stream_configuration(
    window_pixel_size: PhysicalSize<u32>,
    color_pipeline: crate::ColorPipeline,
) -> Retained<SCStreamConfiguration> {
    use objc2_core_graphics::{
        kCGColorSpaceDisplayP3, kCGColorSpaceExtendedLinearDisplayP3, kCGColorSpaceSRGB,
    };
    use objc2_core_video::kCVPixelFormatType_64RGBAHalf;
    unsafe {
        let cfg = SCStreamConfiguration::new();
        cfg.setWidth(window_pixel_size.width.max(1) as usize);
        cfg.setHeight(window_pixel_size.height.max(1) as usize);
        // Pixel format and color space travel together: SCK
        // encodes captured pixels into the IOSurface using both,
        // and the consumer-side Metal texture's pixel format has
        // to match the IOSurface's. The mapping is driven by the
        // `ColorPipeline` choice — see [`crate::ColorPipeline`]
        // for the per-variant doc and the matching Metal/wgpu
        // formats applied later in `try_acquire_frame`.
        let (cv_pixel_format, color_space_name) = match color_pipeline {
            crate::ColorPipeline::Srgb => (kCVPixelFormatType_32BGRA, kCGColorSpaceSRGB),
            crate::ColorPipeline::DisplayP3 => (kCVPixelFormatType_32BGRA, kCGColorSpaceDisplayP3),
            crate::ColorPipeline::Hdr16f => (
                // 16-bit float per channel, RGBA channel order
                // (note: not BGRA). The consumer's
                // `MTLPixelFormat::RGBA16Float` and
                // `wgpu::TextureFormat::Rgba16Float` use the
                // same channel order, so no swizzle.
                kCVPixelFormatType_64RGBAHalf,
                // Extended-linear means values > 1.0 are valid
                // (HDR over-bright highlights); the linear
                // transfer means the GPU shader can sample
                // without decoding a non-linear curve. P3
                // primaries widen the gamut alongside.
                kCGColorSpaceExtendedLinearDisplayP3,
            ),
        };
        cfg.setPixelFormat(cv_pixel_format);
        cfg.setColorSpaceName(color_space_name);
        cfg.setShowsCursor(false);
        // Our consumer only ever keeps the latest sample (the
        // `LatestSample` Mutex overwrites on each callback), so
        // we can afford a shallow queue. Smaller depth = less
        // pipeline latency between WebKit render and demo present.
        cfg.setQueueDepth(2);
        cfg
    }
}

/// Per-frame Metal pixel format (used for both source and
/// destination textures in `try_acquire_frame`'s blit). Must
/// match the pixel format SCK encoded into the IOSurface, which
/// is in turn driven by [`make_stream_configuration`].
pub(super) fn metal_pixel_format_for(
    pipeline: crate::ColorPipeline,
) -> objc2_metal::MTLPixelFormat {
    match pipeline {
        crate::ColorPipeline::Srgb | crate::ColorPipeline::DisplayP3 => {
            objc2_metal::MTLPixelFormat::BGRA8Unorm
        }
        crate::ColorPipeline::Hdr16f => objc2_metal::MTLPixelFormat::RGBA16Float,
    }
}

/// wgpu format for the texture handed to the consumer.
/// Same gamut/transfer rules as [`metal_pixel_format_for`].
pub(super) fn wgpu_format_for(pipeline: crate::ColorPipeline) -> wgpu::TextureFormat {
    match pipeline {
        crate::ColorPipeline::Srgb | crate::ColorPipeline::DisplayP3 => {
            wgpu::TextureFormat::Bgra8Unorm
        }
        crate::ColorPipeline::Hdr16f => wgpu::TextureFormat::Rgba16Float,
    }
}

/// Compute the host window's pixel dimensions (window-frame
/// points × backing scale). The SCK stream is configured to this
/// size so the captured IOSurface preserves the full window at
/// native resolution; `try_acquire_frame` then blits the
/// WKWebView's pixel rect out of it.
///
/// Uses `window.frame()` (full frame including the title bar) —
/// **not** `contentView().frame()`. SCK's
/// `initWithDesktopIndependentWindow:` filter captures the entire
/// window including its chrome; if we configured SCK to a
/// content-view-only size, SCK would scale the chrome+content
/// pair down to fit, putting the WKWebView's pixels at unexpected
/// coordinates (and bleeding the title bar into the imported
/// texture). At full frame size SCK doesn't scale, and our crop
/// can pin the webview's region precisely.
pub(super) fn host_window_pixel_size(window: &objc2_app_kit::NSWindow) -> PhysicalSize<u32> {
    let scale = window.backingScaleFactor().max(1.0);
    let frame = window.frame();
    PhysicalSize::new(
        ((frame.size.width * scale).round() as u32).max(1),
        ((frame.size.height * scale).round() as u32).max(1),
    )
}

/// Compute the WKWebView's rect within its host window, in
/// **points** with **top-left origin** measured from the
/// window's frame top edge (i.e. *including* the chrome/title-bar
/// region above the content view).
///
/// This matches the coordinate space of the SCK-captured texture:
/// `initWithDesktopIndependentWindow:` captures the entire
/// window — title bar plus content — so a crop into the captured
/// texture must measure Y from the top of the title bar, not the
/// top of the content view.
///
/// AppKit's `convertRect_toView(.., None)` lifts the webview's
/// `bounds` into window coords (bottom-left origin, content-view
/// relative). We flip Y against the content-view height to reach
/// content-view-top-left, then add the chrome height
/// (`window.frame().height - contentView.frame().height`) to
/// reach window-frame-top-left.
pub(super) fn webview_window_rect(
    webview: &objc2_web_kit::WKWebView,
    window: &objc2_app_kit::NSWindow,
) -> objc2_core_foundation::CGRect {
    let local_bounds = webview.bounds();
    let window_pt_rect = webview.convertRect_toView(local_bounds, None);
    let frame_height = window.frame().size.height;
    let content_height = window
        .contentView()
        .map(|cv| cv.frame().size.height)
        .unwrap_or(frame_height);
    let chrome_height = (frame_height - content_height).max(0.0);
    let y_in_content_top = content_height - window_pt_rect.origin.y - window_pt_rect.size.height;
    let y_in_frame_top = y_in_content_top + chrome_height;
    objc2_core_foundation::CGRect {
        origin: objc2_core_foundation::CGPoint {
            x: window_pt_rect.origin.x,
            y: y_in_frame_top,
        },
        size: objc2_core_foundation::CGSize {
            width: window_pt_rect.size.width,
            height: window_pt_rect.size.height,
        },
    }
}

impl WkWebViewProducer {
    /// Stop the capture stream and tear down ScreenCaptureKit state.
    /// Idempotent. Safe to call from `Drop`.
    pub fn stop_capture(&mut self) {
        let Some(capture) = self.capture.take() else {
            return;
        };

        // Synchronously stop on the SCK background thread, but don't
        // block the main thread waiting — completion errors are
        // surfaced via `stream_error` if useful.
        unsafe {
            capture.stream.stopCaptureWithCompletionHandler(None);
            let _ = capture.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*capture.output),
                SCStreamOutputType::Screen,
            );
        }

        // Walk back the advertised capability so a future
        // `start_capture` correctly re-flips it.
        self.capabilities.preferred_mode = WebSurfaceMode::NativeChildOverlay;
        self.capabilities.imported_texture = crate::native_frame::CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::PlatformNotImplemented,
        );
        self.capabilities.reason =
            "WkWebViewProducer slice B: capture stopped; reverting to overlay surface.";
        if let Ok(mut p) = self.pending_capture.lock() {
            *p = PendingCaptureSlot::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{ConfigurationFailure, complete_configuration_update};

    #[test]
    fn failed_configuration_never_acknowledges_its_revision() {
        let requested = AtomicU64::new(4);
        let applied = AtomicU64::new(3);
        let failure = Mutex::new(None);

        complete_configuration_update(
            &requested,
            &applied,
            &failure,
            4,
            Some("SCK rejected the output dimensions".into()),
        );

        assert_eq!(applied.load(Ordering::Acquire), 3);
        assert_eq!(
            *failure.lock().unwrap(),
            Some(ConfigurationFailure {
                revision: 4,
                message: "SCK rejected the output dimensions".into(),
            })
        );
    }

    #[test]
    fn stale_configuration_completion_cannot_change_current_state() {
        let requested = AtomicU64::new(8);
        let applied = AtomicU64::new(7);
        let failure = Mutex::new(None);

        complete_configuration_update(
            &requested,
            &applied,
            &failure,
            7,
            Some("old request failed".into()),
        );

        assert_eq!(applied.load(Ordering::Acquire), 7);
        assert_eq!(*failure.lock().unwrap(), None);
    }

    #[test]
    fn successful_configuration_acknowledges_current_revision() {
        let requested = AtomicU64::new(9);
        let applied = AtomicU64::new(8);
        let failure = Mutex::new(None);

        complete_configuration_update(&requested, &applied, &failure, 9, None);

        assert_eq!(applied.load(Ordering::Acquire), 9);
        assert_eq!(*failure.lock().unwrap(), None);
    }
}

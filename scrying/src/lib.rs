// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

#![doc = include_str!("../README.md")]

// Alias the feature-selected wgpu family back to the plain crate names. Public
// re-exports let hosts name the exact Device/Texture types scrying expects.
#[cfg(feature = "wgpu-30")]
pub extern crate wgpu_30 as wgpu;
#[cfg(feature = "wgpu-30")]
pub extern crate wgpu_hal_30 as wgpu_hal;

#[cfg(all(feature = "wgpu-29", not(feature = "wgpu-30")))]
pub extern crate wgpu_29 as wgpu;
#[cfg(all(feature = "wgpu-29", not(feature = "wgpu-30")))]
pub extern crate wgpu_hal_29 as wgpu_hal;

#[cfg(all(
    feature = "wgpu-28",
    not(feature = "wgpu-29"),
    not(feature = "wgpu-30")
))]
pub extern crate wgpu_28 as wgpu;
#[cfg(all(
    feature = "wgpu-28",
    not(feature = "wgpu-29"),
    not(feature = "wgpu-30")
))]
pub extern crate wgpu_hal_28 as wgpu_hal;

#[cfg(not(any(feature = "wgpu-28", feature = "wgpu-29", feature = "wgpu-30")))]
compile_error!(
    "scrying needs one wgpu version feature: enable `wgpu-30` (default), `wgpu-29`, or `wgpu-28`"
);

pub mod native_frame;

#[cfg(target_os = "macos")]
mod wgpu_compat;

use dpi::PhysicalSize;
use thiserror::Error;

#[cfg(target_os = "windows")]
pub use native_frame::Dx12FenceSynchronizer;
pub use native_frame::{
    CapabilityStatus, DmaBufImage, DmaBufPlane, Dx12SharedTexture, HostWgpuContext, ImportOptions,
    ImportedTexture, InteropBackend, InteropError, MetalTextureRef, NativeFrame, NativeFrameKind,
    ProducerCapabilities, SyncMechanism, TextureImporter, UnsupportedReason, WgpuTextureImporter,
};

#[cfg(target_os = "linux")]
pub use native_frame::dmabuf::{DmaBufDeviceError, build_dmabuf_capable_device};

#[cfg(target_os = "windows")]
pub mod windows_capture;

#[cfg(target_os = "windows")]
pub mod webview2_composition_producer;

#[cfg(target_os = "windows")]
pub use webview2_composition_producer::{
    CaptureMetrics, CompositionRoot as PlatformCompositionRoot,
    WebView2CompositionConfig as PlatformWebSurfaceConfig,
    WebView2CompositionProducer as PlatformWebSurfaceProducer,
};

#[cfg(target_os = "macos")]
pub mod wkwebview_producer;

#[cfg(target_os = "macos")]
pub use wkwebview_producer::{
    WkWebViewProducer as PlatformWebSurfaceProducer,
    WkWebViewProducerConfig as PlatformWebSurfaceConfig,
};

#[cfg(target_os = "macos")]
pub use wkwebview_producer::{
    CaptureMetrics, CaptureStatus, CookieChangeHandlerFn, WkWebViewProducer,
    WkWebViewProducerConfig,
};
// `ColorPipeline` lives in `lib.rs` (cross-platform-public) but is
// listed alongside the macOS producer above so the `cfg(target_os
// = "macos")` re-exports stay together. It's already public here.

#[cfg(target_os = "linux")]
pub mod wpe_producer;

#[cfg(all(target_os = "linux", feature = "webkitgtk-fallback"))]
pub mod webkitgtk_producer;

#[cfg(all(target_os = "linux", feature = "webkit6"))]
pub mod webkit6_producer;

// Linux producer selection. Three co-equal backends — pick one via
// cargo feature:
//   webkit6              → WebKitGTK 6.0 / GTK 4 (newest, libadwaita line)
//   webkitgtk-fallback   → WebKitGTK 4.1 / GTK 3 (widest installed base, Tauri-shaped)
//   (neither)            → WPE producer type; `wpe` enables its runtime FFI
// When both `webkit6` and `webkitgtk-fallback` are on, `webkit6` wins.
#[cfg(all(target_os = "linux", feature = "webkit6"))]
pub use webkit6_producer::{
    WebKit6Producer as PlatformWebSurfaceProducer,
    WebKit6ProducerConfig as PlatformWebSurfaceConfig,
};

#[cfg(all(
    target_os = "linux",
    feature = "webkitgtk-fallback",
    not(feature = "webkit6")
))]
pub use webkitgtk_producer::{
    WebKitGtkProducer as PlatformWebSurfaceProducer,
    WebKitGtkProducerConfig as PlatformWebSurfaceConfig,
};

#[cfg(all(
    target_os = "linux",
    not(feature = "webkit6"),
    not(feature = "webkitgtk-fallback")
))]
pub use wpe_producer::{
    WpeProducer as PlatformWebSurfaceProducer, WpeProducerConfig as PlatformWebSurfaceConfig,
};

/// How a system webview can participate in a host compositor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WebSurfaceMode {
    /// The adapter can emit a native GPU frame importable through Graft.
    ImportedTexture,
    /// The webview must remain a platform child window/visual overlay.
    NativeChildOverlay,
    /// The adapter can emit CPU pixels or encoded snapshots.
    CpuSnapshot,
    /// No usable surface path is available.
    Unsupported,
}

/// The selected system-webview backend on the current platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SystemWebviewBackend {
    WebView2,
    WkWebView,
    Wpe,
    WebKitGtk,
    Unknown,
}

impl SystemWebviewBackend {
    pub fn detect() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::WebView2
        }
        #[cfg(target_os = "macos")]
        {
            Self::WkWebView
        }
        #[cfg(all(target_os = "linux", feature = "webkit6"))]
        {
            Self::WebKitGtk
        }
        #[cfg(all(
            target_os = "linux",
            feature = "webkitgtk-fallback",
            not(feature = "webkit6")
        ))]
        {
            Self::WebKitGtk
        }
        #[cfg(all(
            target_os = "linux",
            not(feature = "webkit6"),
            not(feature = "webkitgtk-fallback")
        ))]
        {
            Self::Wpe
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            Self::Unknown
        }
    }
}

/// Probe result for a system-webview surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSurfaceCapabilities {
    pub backend: SystemWebviewBackend,
    pub preferred_mode: WebSurfaceMode,
    pub imported_texture: CapabilityStatus,
    pub native_child_overlay: CapabilityStatus,
    pub cpu_snapshot: CapabilityStatus,
    pub supported_frames: Vec<NativeFrameKind>,
    pub reason: &'static str,
    /// Non-frame operations exposed by the selected producer.
    pub features: WebSurfaceFeatureCapabilities,
}

/// Cookie-store operations exposed by a producer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CookieCapabilities {
    pub read: CapabilityStatus,
    pub write: CapabilityStatus,
    pub delete: CapabilityStatus,
    pub change_events: CapabilityStatus,
    pub same_site: CapabilityStatus,
    pub partitioned: CapabilityStatus,
    pub http_only: CapabilityStatus,
    pub secure: CapabilityStatus,
    pub expires: CapabilityStatus,
}

/// JavaScript execution operations exposed by a producer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptCapabilities {
    pub execute: CapabilityStatus,
    pub result: CapabilityStatus,
    pub exceptions: CapabilityStatus,
    /// Whether the result path honors the caller's timeout while blocking.
    pub bounded_blocking: CapabilityStatus,
}

/// Explicit capability matrix for host-visible, non-frame operations.
///
/// `Partial` means the operation exists but has a documented degradation;
/// hosts should inspect its detail instead of assuming browser parity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSurfaceFeatureCapabilities {
    pub cookies: CookieCapabilities,
    pub script: ScriptCapabilities,
    pub page_capture: CapabilityStatus,
    pub devtools: CapabilityStatus,
    pub downloads: CapabilityStatus,
    pub popups: CapabilityStatus,
    pub drag_drop: CapabilityStatus,
    pub pointer_input: CapabilityStatus,
    pub ime: CapabilityStatus,
    pub accessibility: CapabilityStatus,
    /// Stable, host-actionable explanations for degraded or unavailable
    /// paths that do not fit in an individual status field.
    pub degradation_reasons: Vec<&'static str>,
}

impl WebSurfaceFeatureCapabilities {
    fn unsupported(reason: UnsupportedReason, explanation: &'static str) -> Self {
        let status = || CapabilityStatus::Unsupported(reason.clone());
        Self {
            cookies: CookieCapabilities {
                read: status(),
                write: status(),
                delete: status(),
                change_events: status(),
                same_site: status(),
                partitioned: status(),
                http_only: status(),
                secure: status(),
                expires: status(),
            },
            script: ScriptCapabilities {
                execute: status(),
                result: status(),
                exceptions: status(),
                bounded_blocking: status(),
            },
            page_capture: status(),
            devtools: status(),
            downloads: status(),
            popups: status(),
            drag_drop: status(),
            pointer_input: status(),
            ime: status(),
            accessibility: status(),
            degradation_reasons: vec![explanation],
        }
    }
}

impl WebSurfaceCapabilities {
    pub fn probe(host: Option<&HostWgpuContext>) -> Self {
        match SystemWebviewBackend::detect() {
            SystemWebviewBackend::WebView2 => probe_webview2(host),
            SystemWebviewBackend::WkWebView => {
                let imported_texture = match host.map(|h| h.backend) {
                    Some(InteropBackend::Metal) => CapabilityStatus::Partial(
                        "requires an explicit successful start_capture or start_capture_async call and Screen Recording permission",
                    ),
                    Some(_) => CapabilityStatus::Unsupported(
                        crate::native_frame::UnsupportedReason::HostBackendMismatch,
                    ),
                    None => CapabilityStatus::Unsupported(
                        crate::native_frame::UnsupportedReason::HostBackendUnavailable,
                    ),
                };
                let features = wkwebview_features();
                Self {
                    backend: SystemWebviewBackend::WkWebView,
                    // A Metal host makes capture possible, but it does not
                    // make capture live. The producer switches this to
                    // ImportedTexture only after ScreenCaptureKit starts.
                    preferred_mode: WebSurfaceMode::NativeChildOverlay,
                    imported_texture,
                    native_child_overlay: CapabilityStatus::Supported,
                    cpu_snapshot: CapabilityStatus::Supported,
                    supported_frames: vec![NativeFrameKind::MetalTextureRef],
                    reason: "WKWebView producer starts as a NativeChildOverlay. A Metal-backed host may explicitly start ScreenCaptureKit capture; only a successful start changes the live producer capability to ImportedTexture. CpuSnapshot via takeSnapshot: is available independently.",
                    features,
                }
            }
            SystemWebviewBackend::Wpe => {
                // The mutation below is Linux-only; other targets see an
                // unused `mut` without the cfg_attr.
                #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
                let mut caps = linux_wpe_capabilities();
                // The WPE module is also available as a compile-only shell
                // when its runtime feature is disabled. Do not let a host
                // probe turn that shell into a claimed DMABUF producer.
                if !cfg!(feature = "wpe") {
                    return caps;
                }
                // After Phase 4a, the WPE column's `imported_texture`
                // depends on whether the host's wgpu Vulkan device
                // has the DMABUF extensions the import path needs.
                // Without a host (None probe), the producer can still emit
                // DMABUF frames, but there is no host device against which
                // to validate import support. Keep the combined probe
                // conservative without claiming that the producer path is
                // unimplemented.
                #[cfg(target_os = "linux")]
                match host {
                    Some(host) => {
                        match crate::native_frame::dmabuf::probe_dmabuf_extensions(host) {
                            Ok(()) => {
                                caps.imported_texture = CapabilityStatus::Supported;
                                caps.preferred_mode = WebSurfaceMode::ImportedTexture;
                            }
                            Err(reason) => {
                                caps.imported_texture = CapabilityStatus::Unsupported(reason);
                                caps.preferred_mode = WebSurfaceMode::Unsupported;
                            }
                        }
                    }
                    None => {
                        caps.imported_texture = CapabilityStatus::Unsupported(
                            crate::native_frame::UnsupportedReason::HostBackendUnavailable,
                        );
                        caps.preferred_mode = WebSurfaceMode::Unsupported;
                    }
                }
                caps
            }
            SystemWebviewBackend::WebKitGtk => linux_webkitgtk_capabilities(),
            SystemWebviewBackend::Unknown => Self {
                backend: SystemWebviewBackend::Unknown,
                preferred_mode: WebSurfaceMode::Unsupported,
                imported_texture: CapabilityStatus::Unsupported(
                    crate::native_frame::UnsupportedReason::HostBackendUnavailable,
                ),
                native_child_overlay: CapabilityStatus::Unsupported(
                    crate::native_frame::UnsupportedReason::HostBackendUnavailable,
                ),
                cpu_snapshot: CapabilityStatus::Unsupported(
                    crate::native_frame::UnsupportedReason::HostBackendUnavailable,
                ),
                supported_frames: Vec::new(),
                reason: "No system-webview backend is defined for this platform.",
                features: WebSurfaceFeatureCapabilities::unsupported(
                    UnsupportedReason::HostBackendUnavailable,
                    "No system-webview backend is defined for this platform.",
                ),
            },
        }
    }

    pub fn producer_capabilities(&self) -> ProducerCapabilities {
        ProducerCapabilities {
            supported_frames: self.supported_frames.clone(),
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_wpe_capabilities() -> WebSurfaceCapabilities {
    wpe_producer::linux_wpe_capabilities()
}

#[cfg(not(target_os = "linux"))]
fn linux_wpe_capabilities() -> WebSurfaceCapabilities {
    WebSurfaceCapabilities {
        backend: SystemWebviewBackend::Wpe,
        preferred_mode: WebSurfaceMode::Unsupported,
        imported_texture: CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::PlatformNotImplemented,
        ),
        native_child_overlay: CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::PlatformNotImplemented,
        ),
        cpu_snapshot: CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::PlatformNotImplemented,
        ),
        supported_frames: Vec::new(),
        reason: "WPE is only available on Linux.",
        features: WebSurfaceFeatureCapabilities::unsupported(
            UnsupportedReason::PlatformNotImplemented,
            "WPE is only available on Linux.",
        ),
    }
}

// WebKitGTK capabilities shim: `webkit6` wins over `webkitgtk-fallback`
// when both features are enabled. With neither, returns an "unsupported"
// stub used for type-name printing in cross-platform code paths.
#[cfg(all(target_os = "linux", feature = "webkit6"))]
fn linux_webkitgtk_capabilities() -> WebSurfaceCapabilities {
    webkit6_producer::linux_webkit6_capabilities()
}

#[cfg(all(
    target_os = "linux",
    feature = "webkitgtk-fallback",
    not(feature = "webkit6")
))]
fn linux_webkitgtk_capabilities() -> WebSurfaceCapabilities {
    webkitgtk_producer::linux_webkitgtk_capabilities()
}

#[cfg(not(all(
    target_os = "linux",
    any(feature = "webkit6", feature = "webkitgtk-fallback")
)))]
fn linux_webkitgtk_capabilities() -> WebSurfaceCapabilities {
    WebSurfaceCapabilities {
        backend: SystemWebviewBackend::WebKitGtk,
        preferred_mode: WebSurfaceMode::Unsupported,
        imported_texture: CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::PlatformNotImplemented,
        ),
        native_child_overlay: CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::PlatformNotImplemented,
        ),
        cpu_snapshot: CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::PlatformNotImplemented,
        ),
        supported_frames: Vec::new(),
        reason: "WebKitGTK producer is only available on Linux with the `webkitgtk-fallback` (GTK 3 / WebKitGTK 4.1) or `webkit6` (GTK 4 / WebKitGTK 6.0) feature enabled.",
        features: WebSurfaceFeatureCapabilities::unsupported(
            UnsupportedReason::PlatformNotImplemented,
            "WebKitGTK producer is unavailable because its Linux feature is disabled.",
        ),
    }
}

fn webview2_features(imported_texture: &CapabilityStatus) -> WebSurfaceFeatureCapabilities {
    let host_reason = if *imported_texture == CapabilityStatus::Supported {
        None
    } else {
        Some("WebView2 imported frames require a D3D12 host device; overlay remains available.")
    };
    let mut degradation_reasons = vec![
        "WebView2 cookie enumeration does not round-trip SameSite or Partitioned attributes.",
        "WebView2 surfaces script execution failures as WebSurfaceError text; structured JavaScript exception fields are not preserved.",
        "WebView2 drag forwarding is exposed through concrete IDataObject methods, not the portable trait method.",
        "WebView2 accessibility-tree export is not exposed by this producer.",
    ];
    if let Some(reason) = host_reason {
        degradation_reasons.push(reason);
    }
    WebSurfaceFeatureCapabilities {
        cookies: CookieCapabilities {
            read: CapabilityStatus::Supported,
            write: CapabilityStatus::Supported,
            delete: CapabilityStatus::Supported,
            change_events: CapabilityStatus::Supported,
            same_site: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            partitioned: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            http_only: CapabilityStatus::Supported,
            secure: CapabilityStatus::Supported,
            expires: CapabilityStatus::Supported,
        },
        script: ScriptCapabilities {
            execute: CapabilityStatus::Supported,
            result: CapabilityStatus::Supported,
            exceptions: CapabilityStatus::Partial(
                "Execution failures surface as WebSurfaceError text; structured JavaScript exception fields are not preserved.",
            ),
            bounded_blocking: CapabilityStatus::Supported,
        },
        page_capture: CapabilityStatus::Supported,
        devtools: CapabilityStatus::Supported,
        downloads: CapabilityStatus::Supported,
        popups: CapabilityStatus::Supported,
        drag_drop: CapabilityStatus::Partial(
            "Use WebView2CompositionProducer::drag_enter/drag_over/drag_leave/drop_data with an IDataObject.",
        ),
        pointer_input: CapabilityStatus::Supported,
        ime: CapabilityStatus::Partial(
            "WebView2 forwards keyboard and IME messages, but native OS IME ownership remains with the host.",
        ),
        accessibility: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
        degradation_reasons,
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn webview2_features_for_producer() -> WebSurfaceFeatureCapabilities {
    webview2_features(&CapabilityStatus::Supported)
}

fn wkwebview_features() -> WebSurfaceFeatureCapabilities {
    let degradation_reasons = vec![
        "WKWebView cookie setters and reads do not round-trip SameSite or Partitioned attributes.",
        "WKWebView does not expose execute-script results through the portable producer trait.",
        "WKWebView capture-mode drag forwarding cannot synthesize NSDraggingInfo; overlay drags remain native.",
        "WKWebView maps touch and pen input to mouse events, dropping pointer identity, pressure, and tilt.",
        "WKWebView exposes inspector attachment through setInspectable, but cannot open Safari Web Inspector programmatically.",
        "WKWebView accessibility-tree export is not exposed by this producer.",
    ];
    WebSurfaceFeatureCapabilities {
        cookies: CookieCapabilities {
            read: CapabilityStatus::Supported,
            write: CapabilityStatus::Supported,
            delete: CapabilityStatus::Supported,
            change_events: CapabilityStatus::Supported,
            same_site: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            partitioned: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            http_only: CapabilityStatus::Supported,
            secure: CapabilityStatus::Supported,
            expires: CapabilityStatus::Supported,
        },
        script: ScriptCapabilities {
            execute: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            result: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            exceptions: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            bounded_blocking: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
        },
        page_capture: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
        devtools: CapabilityStatus::Partial("Safari Web Inspector must attach externally."),
        downloads: CapabilityStatus::Supported,
        popups: CapabilityStatus::Supported,
        drag_drop: CapabilityStatus::Partial("Capture-mode host drag payloads are unavailable."),
        pointer_input: CapabilityStatus::Partial("Touch and pen metadata are reduced to mouse events."),
        ime: CapabilityStatus::Supported,
        accessibility: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
        degradation_reasons,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn wkwebview_features_for_producer() -> WebSurfaceFeatureCapabilities {
    wkwebview_features()
}

fn probe_webview2(host: Option<&HostWgpuContext>) -> WebSurfaceCapabilities {
    let imported_texture = match host.map(|host| host.backend) {
        Some(InteropBackend::Dx12) => CapabilityStatus::Supported,
        Some(_) => CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::HostBackendMismatch,
        ),
        None => CapabilityStatus::Unsupported(
            crate::native_frame::UnsupportedReason::HostBackendUnavailable,
        ),
    };

    let preferred_mode = if imported_texture == CapabilityStatus::Supported {
        WebSurfaceMode::ImportedTexture
    } else {
        WebSurfaceMode::NativeChildOverlay
    };
    let features = webview2_features(&imported_texture);

    WebSurfaceCapabilities {
        backend: SystemWebviewBackend::WebView2,
        preferred_mode,
        imported_texture,
        native_child_overlay: CapabilityStatus::Supported,
        cpu_snapshot: CapabilityStatus::Supported,
        supported_frames: vec![NativeFrameKind::Dx12SharedTexture],
        reason: "Windows target path is WebView2 CompositionController visual capture into a D3D texture, then Dx12SharedTexture import. Pure visual hosting uses WebView2 CDP Input for keyboard/text; native OS IME ownership remains host-driven.",
        features,
    }
}

/// A frame emitted by a system-webview producer.
#[non_exhaustive]
pub enum WebSurfaceFrame {
    Native(NativeFrame),
    CpuRgba {
        size: PhysicalSize<u32>,
        pixels: image::RgbaImage,
        generation: u64,
    },
    PngSnapshot {
        size: PhysicalSize<u32>,
        bytes: Vec<u8>,
        generation: u64,
    },
    OverlayOnly,
}

impl WebSurfaceFrame {
    pub fn mode(&self) -> WebSurfaceMode {
        match self {
            Self::Native(_) => WebSurfaceMode::ImportedTexture,
            Self::CpuRgba { .. } | Self::PngSnapshot { .. } => WebSurfaceMode::CpuSnapshot,
            Self::OverlayOnly => WebSurfaceMode::NativeChildOverlay,
        }
    }
}

#[derive(Debug, Error)]
pub enum WebSurfaceError {
    #[error("web surface mode is unsupported: {0}")]
    Unsupported(&'static str),
    #[error("frame is not ready yet: {0}")]
    NotReady(&'static str),
    #[error(transparent)]
    Interop(#[from] InteropError),
    /// A native-host migration reached a state whose owner could not be
    /// determined. The producer must not be treated as remaining at its
    /// source host.
    #[error("native host migration is indeterminate: {0}")]
    HostMigrationIndeterminate(String),
    #[error("platform capture failed: {0}")]
    Platform(String),
}

/// Response served by a host-owned web resource handler — MIME type plus the
/// raw bytes that should appear as the resource body to the WebView.
///
/// `headers` contributes extra HTTP response headers (`Content-Disposition`,
/// `Cache-Control`, etc.). Producers always set `Content-Type` from
/// `mime_type` and `Content-Length` from `body.len()`. Use
/// [`Self::with_header`] for the common case.
#[derive(Clone, Debug)]
pub struct UrlSchemeResponse {
    pub mime_type: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl UrlSchemeResponse {
    /// Append an extra HTTP header to this response.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Closure type registered on a producer to serve app-owned resources.
///
/// macOS uses this for custom URL schemes such as `mere://settings`. Windows
/// uses the same response shape for virtual HTTPS hosts such as
/// `https://mere.local/settings`, routed through WebView2's
/// `WebResourceRequested` event.
pub type UrlSchemeHandlerFn =
    std::sync::Arc<dyn Fn(&str) -> UrlSchemeResponse + Send + Sync + 'static>;

/// Win32 physical-key metadata reported by WebView2 for an accelerator event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicalKeyStatus {
    pub repeat_count: u32,
    pub scan_code: u32,
    pub is_extended_key: bool,
    pub is_menu_key_down: bool,
    pub was_key_down: bool,
    pub is_key_released: bool,
}

/// Host-observable WebView2 accelerator key event.
///
/// This is emitted before WebView2's browser accelerator handling and DOM
/// propagation. The producer only observes and forwards the event; it does not
/// mark keys handled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceleratorKeyEvent {
    pub kind: KeyEventKind,
    pub is_system_key: bool,
    pub virtual_key_code: u32,
    pub key_event_lparam: i32,
    pub physical_key_status: PhysicalKeyStatus,
    pub browser_accelerator_key_enabled: Option<bool>,
}

/// CSS-pixel rectangle reported by a system webview for text input UI.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextInputRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Focused editable state reported by the system webview.
///
/// Rect coordinates are CSS pixels relative to the webview viewport. Hosts that
/// own native IME should map them through the producer placement and DPI scale
/// before calling windowing APIs such as winit's `set_ime_cursor_area`.
///
/// Raw DOM strings (`input_type`, `input_mode`, `autocomplete`) are preserved
/// for richer hosts (TSF text stores, future TextInputClient). Thin hosts
/// that only need a coarse IME hint should call [`TextInputState::purpose`].
#[derive(Clone, Debug, PartialEq)]
pub struct TextInputState {
    pub element_kind: String,
    pub input_type: String,
    pub input_mode: String,
    pub autocomplete: String,
    pub is_multiline: bool,
    pub is_password: bool,
    pub selection_start: u32,
    pub selection_end: u32,
    pub caret_rect: TextInputRect,
}

/// Coarse classification of a focused editable, derived from
/// [`TextInputState`] for hosts whose windowing API exposes only a small
/// "input purpose" surface (e.g. winit's `ImePurpose`).
///
/// Richer hosts should consult the raw `input_type` / `input_mode` /
/// `autocomplete` fields directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputPurpose {
    /// Plain text — default IME treatment.
    Text,
    /// Search field — single-line, free text, search-shaped UX hints.
    Search,
    /// Email address — host may surface an email-shaped soft keyboard.
    Email,
    /// URL — host may surface a URL-shaped soft keyboard.
    Url,
    /// Telephone number — host may surface a dial-pad soft keyboard.
    Tel,
    /// Integer-only input (`inputmode="numeric"`).
    Numeric,
    /// Decimal input (`inputmode="decimal"` or `input type="number"`).
    Decimal,
    /// Password-like field. Today the WebView2 producer suppresses these at
    /// the document bridge before emitting state, so this variant is reserved
    /// for future producers (e.g. TSF) that surface password focus.
    Password,
    /// One-time code / SMS code (`autocomplete="one-time-code"`). Also
    /// suppressed at the document bridge today.
    OneTimeCode,
    /// Author explicitly asked for no software keyboard (`inputmode="none"`).
    /// Hosts should refrain from enabling native IME.
    Disabled,
}

impl TextInputState {
    /// Derive the coarse [`InputPurpose`] for this state.
    ///
    /// Precedence: `inputmode` (explicit author signal) > `input_type` >
    /// default `Text`. Password and one-time-code signals win over both.
    pub fn purpose(&self) -> InputPurpose {
        let autocomplete = self.autocomplete.as_str();
        if self.is_password || autocomplete.contains("password") {
            return InputPurpose::Password;
        }
        if autocomplete == "one-time-code" {
            return InputPurpose::OneTimeCode;
        }
        match self.input_mode.as_str() {
            "none" => return InputPurpose::Disabled,
            "tel" => return InputPurpose::Tel,
            "email" => return InputPurpose::Email,
            "url" => return InputPurpose::Url,
            "search" => return InputPurpose::Search,
            "numeric" => return InputPurpose::Numeric,
            "decimal" => return InputPurpose::Decimal,
            "text" => return InputPurpose::Text,
            _ => {}
        }
        match self.input_type.as_str() {
            "search" => InputPurpose::Search,
            "email" => InputPurpose::Email,
            "url" => InputPurpose::Url,
            "tel" => InputPurpose::Tel,
            "number" => InputPurpose::Decimal,
            _ => InputPurpose::Text,
        }
    }
}

/// Lifecycle / state event emitted by the underlying webview.
///
/// Drained from a producer via [`WebSurfaceProducer::poll_navigation_event`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum NavigationEvent {
    /// Navigation has started toward the given URL. Does not guarantee the
    /// load will succeed.
    Starting { url: String },
    /// The committed source URL has changed (covers same-document
    /// navigations and history pushState/replaceState).
    SourceChanged { url: String },
    /// Navigation finished. `success` reflects whether the load completed
    /// without a top-level error; sub-resource failures do not affect it.
    Completed { url: String, success: bool },
    /// The document title changed.
    TitleChanged { title: String },
    /// The page tried to open a new window (`target="_blank"`,
    /// `window.open(...)`, JS-triggered popup). The producer
    /// suppresses the engine-level popup unconditionally — browser-
    /// shape consumers (multiple tabs per process) want full control
    /// over how popups are routed and should observe this event,
    /// then call `load_url(url)` on a fresh producer instance to
    /// open it as a tab.
    NewWindowRequested { url: String },
    /// The web content process backing this WebView terminated
    /// (typically a content-side crash). The producer's WKWebView
    /// is no longer rendering; the host should reload (and may show
    /// a "tab crashed" UI). Recovery is `producer.reload()` or
    /// `load_url(...)` — the WKWebView itself is reusable.
    ContentProcessTerminated,
    /// The engine received an authentication challenge for the
    /// given URL and protection space. The producer responds with
    /// `PerformDefaultHandling` (system Keychain / interactive UI),
    /// so this event is informational only — useful for browser
    /// chrome that wants to log auth events or show a status
    /// indicator. A future slice may grow a `respond_to_auth`
    /// method for hosts that need to drive the disposition
    /// themselves.
    AuthChallenged {
        url: String,
        /// Host the credential is being requested for.
        host: String,
        /// Authentication method identifier (NSURLAuthenticationMethod*),
        /// e.g. `NSURLAuthenticationMethodHTTPBasic`,
        /// `NSURLAuthenticationMethodServerTrust`,
        /// `NSURLAuthenticationMethodClientCertificate`.
        auth_method: String,
        /// Whether this challenge fired on the page-navigation
        /// channel or on a `WKDownload`'s bytes-fetch channel.
        /// Browser-class consumers route the two differently:
        /// page auth is a tab-level UI moment, download auth is
        /// a per-transfer credential prompt.
        source: AuthSource,
    },
    /// A WebKit-managed download started. The producer chose
    /// `destination_path` automatically (under the configured
    /// download directory); the host is responsible for any UI
    /// (progress bars, "show in Finder", etc.). The `id` correlates
    /// with subsequent `DownloadProgress` / `DownloadFinished` /
    /// `DownloadCancelled` events for this download.
    DownloadStarted {
        id: DownloadId,
        url: String,
        suggested_filename: String,
        destination_path: std::path::PathBuf,
        /// Total bytes the server announced via `Content-Length`.
        /// `None` when the server didn't announce one (chunked
        /// transfer, etc.).
        total_bytes_expected: Option<u64>,
    },
    /// Throttled progress notification. Emitted at most ~10 Hz per
    /// download, plus a final emit at completion. `bytes_written` is
    /// the cumulative count of bytes written to disk.
    DownloadProgress {
        id: DownloadId,
        bytes_written: u64,
        total_bytes_expected: Option<u64>,
    },
    /// A download completed. `error` is `Some` on failure (the file
    /// at `destination_path` may be partial or absent), `None` on
    /// successful completion. Hosts that want to distinguish
    /// host-driven cancellation from a server / network error
    /// should listen for `DownloadCancelled` instead — that variant
    /// only fires when the disposition came from
    /// `set_download_handler` returning `DownloadDecision::Cancel`
    /// or the host calling `cancel_download(id)`.
    DownloadFinished {
        id: DownloadId,
        destination_path: std::path::PathBuf,
        error: Option<String>,
    },
    /// A download was cancelled — either because a
    /// host-registered destination handler returned
    /// `DownloadDecision::Cancel`, or because the host called
    /// `cancel_download(id)` mid-stream.
    ///
    /// `resume_data` is `Some` when WebKit captured enough state
    /// to potentially resume the download via
    /// [`crate::wkwebview_producer::WkWebViewProducer::resume_download`].
    /// `None` when the cancel happened before any bytes transferred (e.g.
    /// host destination handler returned `Cancel` from `decideDestination`),
    /// when the protocol / server doesn't support resumption (no
    /// `Accept-Ranges` support, etc.), or on Windows where WebView2 exposes
    /// live pause/resume operations but no portable offline resume-data blob.
    DownloadCancelled {
        id: DownloadId,
        destination_path: std::path::PathBuf,
        resume_data: Option<Vec<u8>>,
    },
    /// External content was dropped on the page (file from
    /// Finder, URL from another browser, image from a different
    /// app, etc.). Fires *in addition to* the page's own
    /// `dragenter` / `drop` JS events and any default webview
    /// behavior — the user-script that drives this event runs
    /// at the capture phase but does *not* call
    /// `event.preventDefault()`, so the page still gets to
    /// handle the drop normally. Browser-class consumers use
    /// the event for analytics, status indicators, or "I want
    /// to route this URL drop to the active tab" decisions.
    ///
    /// Drops originating from inside the page itself (e.g.
    /// dragging a list item to reorder, or any drag whose
    /// source has no external content) are filtered out
    /// page-side and don't fire this event — the heuristic is
    /// "the `DataTransfer` carries at least one
    /// `files` entry, an image MIME, or a `text/uri-list`."
    ///
    /// `x` / `y` are CSS pixels relative to the WebView's
    /// viewport (matching `MouseEvent.clientX/Y`). `file_count`
    /// is the number of OS files in `dataTransfer.files`.
    /// `primary_url` is the first URL parsed from
    /// `dataTransfer.getData("text/uri-list")` (or
    /// `text/plain` if no uri-list is present); `None` when
    /// the drop carries no URL string.
    DropDetected {
        x: f64,
        y: f64,
        file_count: u32,
        primary_url: Option<String>,
    },
    /// The page's WebRTC capture state changed — at least one
    /// `getUserMedia` track started or ended since the last
    /// emission. `audio_active_tracks` / `video_active_tracks` are
    /// counts (not booleans) so a host that wants a "red-dot
    /// indicator" can show it whenever `>0`, while a host that
    /// wants to itemize active streams can still distinguish
    /// "1 mic" from "2 mics."
    ///
    /// Apple's `WKWebView` exposes no public-API hook for tracking
    /// active capture; this event is delivered via a JS user-script
    /// that monkey-patches `navigator.mediaDevices.getUserMedia` and
    /// listens for `track.ended`. Pages that replace `getUserMedia`
    /// before our script installs (or `navigator.mediaDevices`
    /// itself) fall outside the wrap and will not emit this event;
    /// for those edge cases the host should fall back to
    /// inspecting [`PermissionRequest`] grants.
    MediaCaptureStateChanged {
        audio_active_tracks: u32,
        video_active_tracks: u32,
    },
    /// The user right-clicked inside the page. The producer
    /// suppresses WebKit's default context menu and emits this
    /// event so a browser-class consumer can show its own — usually
    /// a host-rendered `NSMenu` with items like "Open link in new
    /// tab" or "Save image as...". Apple's
    /// `webView:contextMenuConfigurationForElement:` is iOS-only,
    /// so on macOS the producer goes through a JS user-script
    /// (`contextmenu` capture-phase listener +
    /// `event.preventDefault()`) and a dedicated
    /// `WKScriptMessageHandler` to deliver this event.
    ///
    /// Coordinates are in CSS pixels relative to the WebView's
    /// viewport (matching `MouseEvent.clientX` /
    /// `MouseEvent.clientY`). `link_url` and `image_url` walk the
    /// click target's ancestor chain to recover the closest
    /// enclosing `<a href>` / `<img src>`; both are `None` when no
    /// such ancestor exists (e.g. a right-click on plain body
    /// text).
    ContextMenuRequested {
        page_url: String,
        x: f64,
        y: f64,
        link_url: Option<String>,
        image_url: Option<String>,
    },
    /// WebView2 observed a browser accelerator key before its browser-feature
    /// handling and DOM propagation stages.
    AcceleratorKeyPressed { event: AcceleratorKeyEvent },
    /// A DOM editable element gained focus and should receive native text input.
    TextInputFocused { state: TextInputState },
    /// The focused DOM editable element's selection, caret rect, or input
    /// metadata changed.
    TextInputChanged { state: TextInputState },
    /// DOM focus left the editable element Scrying was tracking.
    TextInputBlurred,
}

/// Opaque per-producer identifier for a download. Issued when
/// WebKit promotes a navigation to a download; used by the host to
/// correlate `DownloadStarted` / `DownloadProgress` /
/// `DownloadFinished` / `DownloadCancelled` events and to drive
/// [`crate::wkwebview_producer::WkWebViewProducer::cancel_download`].
///
/// IDs are monotonically increasing per producer and are not
/// reused. They have no meaning across producers.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DownloadId(pub u64);

/// Information passed to a host-registered download-destination
/// handler (see
/// [`crate::wkwebview_producer::WkWebViewProducer::set_download_handler`]).
/// The host returns a [`DownloadDecision`] describing whether to
/// accept the download (and where to write it) or cancel it.
#[derive(Clone, Debug)]
pub struct DownloadDestinationRequest {
    pub id: DownloadId,
    pub url: String,
    pub suggested_filename: String,
    pub mime_type: String,
    pub total_bytes_expected: Option<u64>,
}

/// Disposition the host's destination handler returns to
/// [`DownloadDestinationRequest`].
#[derive(Clone, Debug)]
pub enum DownloadDecision {
    /// Accept the download and write it to this absolute path.
    /// Parent directory is created if it doesn't exist.
    AcceptAt(std::path::PathBuf),
    /// Cancel the download. Triggers a `DownloadCancelled` event;
    /// no bytes are written.
    Cancel,
}

/// Which delegate channel produced an auth challenge — useful for
/// browser-class consumers that route page-load auth (the user
/// typed a URL into a tab) differently from download auth (a
/// background `WKDownload` is fetching bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthSource {
    /// Fired by the page's
    /// `WKNavigationDelegate::webView:didReceiveAuthenticationChallenge:`.
    /// The challenge belongs to a top-level navigation request or
    /// a sub-resource fetch initiated by the page.
    Page,
    /// Fired by a `WKDownloadDelegate::download:didReceiveAuthenticationChallenge:`,
    /// either for a programmatic `start_download` call or for a
    /// nav-promoted download whose response triggered an auth
    /// requirement on the bytes-fetch leg.
    Download,
}

/// Information passed to a host-registered auth-challenge handler
/// (see `WkWebViewProducer::set_auth_handler`). The host returns an
/// [`AuthDisposition`] describing how the challenge should be
/// resolved.
#[derive(Clone, Debug)]
pub struct AuthChallenge {
    pub url: String,
    /// Host the credential is being requested for.
    pub host: String,
    /// Authentication method identifier
    /// (`NSURLAuthenticationMethodHTTPBasic`,
    /// `...HTTPDigest`, `...ServerTrust`,
    /// `...ClientCertificate`, etc.).
    pub auth_method: String,
    /// Realm the server announced (HTTP basic / digest only —
    /// empty for other methods).
    pub realm: String,
    /// Which delegate channel produced this challenge — see
    /// [`AuthSource`].
    pub source: AuthSource,
}

/// Disposition the host returns from its auth-challenge handler.
/// Maps onto `NSURLSessionAuthChallengeDisposition`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AuthDisposition {
    /// Fall back to WebKit's default handling (system Keychain /
    /// interactive prompts).
    PerformDefault,
    /// Cancel the auth challenge — the load fails.
    Cancel,
    /// "I can't satisfy this protection space; ask the next one."
    /// Useful for client-cert challenges where the host has no
    /// matching cert.
    RejectProtectionSpace,
    /// Provide a username + password credential (HTTP basic /
    /// digest). Persistence is session-only — no Keychain write.
    UseCredential { username: String, password: String },
}

/// Information passed to a host-registered permission handler
/// (see `WkWebViewProducer::set_permission_handler`). The host
/// returns a [`PermissionDecision`].
#[derive(Clone, Debug)]
pub struct PermissionRequest {
    /// Web origin requesting the permission, e.g.
    /// `"https://example.com"`. Empty for `about:` / `data:` URLs.
    pub origin: String,
    /// URL of the frame that initiated the request — usually the
    /// same as `origin` plus the path; differs from `origin` for
    /// nested iframes.
    pub frame_url: String,
    pub kind: PermissionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PermissionKind {
    Camera,
    Microphone,
    CameraAndMicrophone,
    /// `DeviceMotionEvent` / `DeviceOrientationEvent`.
    DeviceOrientation,
}

/// Disposition the host returns from its permission handler. Maps
/// onto `WKPermissionDecision`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PermissionDecision {
    /// Allow the requested permission.
    Grant,
    /// Refuse the requested permission.
    Deny,
    /// Fall back to WebKit's default behavior — for media-capture
    /// requests this means the OS shows its standard prompt; for
    /// device-orientation it means the engine prompts according to
    /// its own policy. Use this when the host doesn't have an
    /// opinion (e.g. for an "untrusted page, let WebKit handle it"
    /// fallback).
    Prompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

/// HTTP cookie payload used by the producer's cookie-store API.
///
/// `expires_at` is a Unix timestamp (seconds since 1970-01-01 UTC), `None`
/// for session cookies. The shape intentionally carries the full browser
/// session record; producer setters return `Unsupported` when a backend cannot
/// faithfully apply a field such as `Partitioned`.
#[derive(Clone, Debug)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires_at: Option<f64>,
    pub is_secure: bool,
    pub is_http_only: bool,
    pub same_site: Option<SameSite>,
    pub partitioned: bool,
}

/// Reason supplied to a focus move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FocusReason {
    /// Programmatic focus (e.g. user clicked a host control that should
    /// hand focus to the webview).
    Programmatic,
    /// User tabbed forward into the webview.
    Next,
    /// User shift-tabbed into the webview.
    Previous,
}

/// One mouse / scroll event forwarded to the underlying webview.
///
/// Coordinates are in physical pixels, relative to the webview's top-left
/// corner (i.e. the origin of the bounds set by the most recent
/// [`WebSurfaceProducer::resize`] / `set_offset` pair).
#[derive(Clone, Copy, Debug)]
pub struct MouseInput {
    pub kind: MouseEventKind,
    /// Modifier and button state at the moment of the event.
    pub virtual_keys: MouseVirtualKeys,
    /// Wheel delta (for `Wheel` / `HorizontalWheel`) or X-button index
    /// (for `XButton*`). Zero for other event kinds.
    pub mouse_data: i32,
    pub point: (i32, i32),
}

/// Discrete kinds of mouse / scroll event recognised by the underlying
/// composition controller. Mirrors `COREWEBVIEW2_MOUSE_EVENT_KIND`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MouseEventKind {
    LeftButtonDown,
    LeftButtonUp,
    LeftButtonDoubleClick,
    MiddleButtonDown,
    MiddleButtonUp,
    MiddleButtonDoubleClick,
    RightButtonDown,
    RightButtonUp,
    RightButtonDoubleClick,
    XButtonDown,
    XButtonUp,
    XButtonDoubleClick,
    Move,
    Wheel,
    HorizontalWheel,
    Leave,
}

/// Modifier and button state for a [`MouseInput`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MouseVirtualKeys {
    pub control: bool,
    pub shift: bool,
    pub left_button: bool,
    pub middle_button: bool,
    pub right_button: bool,
    pub x_button1: bool,
    pub x_button2: bool,
}

/// One pointer (touch / pen / mouse-as-pointer) event forwarded to the
/// underlying webview. Coordinates are physical pixels relative to the
/// webview's top-left corner.
#[derive(Clone, Copy, Debug)]
pub struct PointerInput {
    pub kind: PointerEventKind,
    pub device: PointerDevice,
    /// Pointer ID. Two simultaneous touches use distinct IDs; a single pen
    /// stays at ID 1. Zero is reserved for "no ID".
    pub pointer_id: u32,
    /// Position of the pointer in physical pixels relative to the webview.
    pub point: (i32, i32),
    /// Pressure in `0.0..=1.0`. `0.0` for non-pressure-aware devices.
    pub pressure: f32,
    /// Tilt in radians for pen input; zero for touch / mouse.
    pub tilt: (f32, f32),
}

/// Discrete kinds of pointer event. Mirrors `COREWEBVIEW2_POINTER_EVENT_KIND`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PointerEventKind {
    Activate,
    Down,
    Enter,
    Leave,
    Up,
    Update,
    CaptureChanged,
}

/// The kind of input device that produced a [`PointerInput`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PointerDevice {
    Touch,
    Pen,
    Mouse,
}

/// Cursor shape the webview wants the host to display, reported via the
/// `cursor_changed` callback registered with
/// [`WebSurfaceProducer::set_cursor_handler`].
///
/// The full Win32 / cocoa / X11 cursor namespace is large and platform-
/// specific. This enum is the subset CSS / WebKit consensus settles
/// on; producers may report `Custom(name)` for shapes not enumerated.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CursorShape {
    Default,
    Pointer,
    Text,
    Wait,
    Crosshair,
    Move,
    NotAllowed,
    Help,
    Progress,
    ResizeNs,
    ResizeEw,
    ResizeNeSw,
    ResizeNwSe,
    ResizeAll,
    Grab,
    Grabbing,
    ZoomIn,
    ZoomOut,
    Custom(String),
}

/// One key / modifier-change event forwarded to the underlying webview.
#[derive(Clone, Debug)]
pub struct KeyboardInput {
    pub kind: KeyEventKind,
    /// Platform-native virtual-key code. Windows: VK_*, Mac: AppKit
    /// `keyCode` (Apple HID usage, e.g. 0x00 = A, 0x24 = Return),
    /// Linux: xkb keycode. Producers map this to whatever the
    /// underlying engine expects.
    pub virtual_key_code: u32,
    /// Text characters this event would produce (after IME / dead-key
    /// composition), if any. Empty for pure modifier-state changes.
    pub characters: String,
    /// Same text but with modifier keys (shift / alt) ignored, for
    /// keyboard-shortcut handling. Empty when not applicable.
    pub characters_ignoring_modifiers: String,
    pub modifiers: KeyModifierFlags,
    /// `true` when the OS reports this event as auto-repeat.
    pub is_repeat: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KeyEventKind {
    Down,
    Up,
    /// A modifier key (shift, control, alt, meta / cmd) toggled.
    /// `virtual_key_code` identifies which one; `characters` is empty.
    ModifiersChanged,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyModifierFlags {
    pub shift: bool,
    pub control: bool,
    /// `Alt` on Windows, `Option` on macOS, `Mod1` on Linux.
    pub alt: bool,
    /// `Win` on Windows, `Command` on macOS, `Mod4` / `Super` on
    /// Linux.
    pub meta: bool,
    /// Caps-lock toggle state at the moment of the event.
    pub caps_lock: bool,
}

/// Drag-and-drop event forwarded to the webview.
#[derive(Clone, Copy, Debug)]
pub struct DragInput {
    pub kind: DragEventKind,
    pub virtual_keys: MouseVirtualKeys,
    pub point: (i32, i32),
    /// Set of allowed effects bitmask; `0` for default.
    pub allowed_effects: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DragEventKind {
    Enter,
    Over,
    Leave,
    Drop,
}

/// Color-space pipeline a producer's capture path is configured
/// for. Picks how the engine encodes captured pixels into the
/// platform-native texture the consumer eventually imports as a
/// wgpu texture.
///
/// Today this is a *static* per-producer choice (set in
/// [`crate::wkwebview_producer::WkWebViewProducerConfig`] or via
/// the producer's `set_color_pipeline` method). A future "adaptive"
/// path could flip it per-page in response to page metadata; the
/// in-flight-sample dropping already handled by the SCK
/// configuration-revision gate would catch the cross-pipeline
/// switch the same way it catches a resize.
///
/// Variants are non-exhaustive on purpose — adding HDR / Rec.2020
/// later shouldn't be a breaking change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorPipeline {
    /// 8-bit BGRA, sRGB primaries, sRGB transfer. The historical
    /// default — every page-side color value lands at the consumer
    /// in sRGB-mapped 8-bit, regardless of whether the page itself
    /// declared `color(display-p3 …)` or wider-gamut images.
    /// Wider-gamut content is tone-mapped to sRGB upstream of us
    /// (by AppKit / SCK).
    #[default]
    Srgb,
    /// 8-bit BGRA, Display P3 primaries, sRGB transfer. Page-side
    /// `color(display-p3 1 0 0)` or photos with embedded P3
    /// profiles arrive at the consumer with their wider gamut
    /// preserved. Same dynamic range as
    /// [`Self::Srgb`] (~100 nits, SDR); only the gamut differs.
    /// Consumers rendering on an sRGB-only display will see the
    /// same colors they would have under `Srgb` (the macOS
    /// composer maps P3→sRGB at present time); on a P3 display,
    /// the wider gamut survives the round-trip.
    DisplayP3,
    /// 16-bit float per channel (RGBA), extended-linear Display P3
    /// primaries. HDR + WCG (wide color gamut). Linear values >
    /// 1.0 represent over-bright highlights; consumers that need
    /// HDR display must configure their wgpu surface for an HDR
    /// format (e.g. `Rgba16Float` + an EDR/PQ alpha mode).
    /// Consumers stuck on SDR surfaces will see HDR-bright values
    /// clamped to ~SDR-white at present time.
    ///
    /// The Metal source / dest textures upgrade from
    /// `BGRA8Unorm` to `RGBA16Float`, the SCK config switches to
    /// `kCVPixelFormatType_64RGBAHalf` /
    /// `kCGColorSpaceExtendedLinearDisplayP3`, and
    /// `MetalTextureRef::format` becomes
    /// `wgpu::TextureFormat::Rgba16Float`. Per-frame GPU
    /// bandwidth ~doubles; per-frame allocation also ~doubles
    /// (8 bytes/pixel vs 4).
    Hdr16f,
}

/// How aggressively the engine throttles a WebView whose host
/// view isn't currently in a window (browser-shape: a tab that
/// isn't the active one). Public-API alternative to the
/// `_setSuspended:`-shaped SPI on macOS 14+ / iOS 17+ —
/// `WKPreferences.inactiveSchedulingPolicy` ships exactly these
/// three options. Older OS versions ignore the setting silently.
///
/// Page Visibility (`set_visible(false)`) is the *light* throttle:
/// it sets `document.hidden = true`, RAF and autoplay throttle.
/// This enum picks how the engine handles a WebView whose view
/// is fully detached from the window hierarchy, where the engine
/// has more latitude to slow or stop background work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InactiveSchedulingPolicy {
    /// Pause JS execution and timer firing entirely. The page
    /// thaws when the WebView returns to a window. Public-API
    /// equivalent of the `_suspendPage:` SPI without the SPI
    /// breakage risk.
    Suspend,
    /// Limit (but don't stop) processing — timers slow,
    /// animation stops, but JS can still run. The "more
    /// aggressive than Page Visibility, less than full suspend"
    /// notch.
    Throttle,
    /// No throttling beyond standard Page Visibility behavior.
    /// Useful when the host knows the page is doing work the
    /// user explicitly wants to keep current (audio playback,
    /// uploads, etc.) even while the tab is off-screen.
    None,
}

/// Snapshot of webview-level settings exposed by the producer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebSurfaceSettings {
    /// Zoom factor (`1.0` is normal). `None` leaves the producer's
    /// default in place.
    pub zoom_factor: Option<f64>,
    /// Custom user-agent string. `None` leaves the producer's default
    /// in place.
    pub user_agent: Option<String>,
    /// Whether developer-tools are accessible (Ctrl+Shift+I, F12, the
    /// host's [`WebSurfaceProducer::open_devtools_window`] call).
    pub devtools_enabled: Option<bool>,
    /// Whether JavaScript execution is enabled in the webview.
    pub javascript_enabled: Option<bool>,
    /// Whether the engine's default right-click context menu is shown.
    pub default_context_menus_enabled: Option<bool>,
    /// Whether the engine's default browser-acceleration shortcuts (zoom,
    /// reload, F5, etc.) are intercepted.
    pub builtin_accelerator_keys_enabled: Option<bool>,
    /// Throttling policy for a WebView whose host view isn't in a
    /// window — browser-shape consumers use this for inactive
    /// tabs. Public on macOS 14+ / iOS 17+ via
    /// `WKPreferences.inactiveSchedulingPolicy`; ignored
    /// silently on older OS versions. `None` leaves the engine's
    /// default in place.
    pub inactive_scheduling_policy: Option<InactiveSchedulingPolicy>,
}

/// Producer contract implemented by platform-specific system-webview frame sources.
///
/// The trait covers the cross-platform lifecycle (capabilities + navigate +
/// resize + offset + a blocking acquire). Per-frame fast-path acquisition
/// and any platform-specific optimization signals (e.g. the Windows
/// "did the shared destination texture get re-allocated this frame"
/// flag) are exposed on the concrete platform producer types and not
/// on the trait, since they have no portable shape.
pub trait WebSurfaceProducer {
    fn capabilities(&self) -> WebSurfaceCapabilities;

    fn mode(&self) -> WebSurfaceMode {
        self.capabilities().preferred_mode
    }

    /// Blocking acquire — returns the next available frame from the
    /// underlying capture path, possibly waiting for the WebView to
    /// produce one.
    fn acquire_frame(&mut self) -> Result<WebSurfaceFrame, WebSurfaceError>;

    /// Non-blocking acquire. Returns `Ok(None)` when no fresh frame is
    /// currently queued.
    fn try_acquire_frame(&mut self) -> Result<Option<WebSurfaceFrame>, WebSurfaceError> {
        self.acquire_frame().map(Some)
    }

    /// Restart a backend capture session after repeated empty non-blocking
    /// polls. Most producers do not need this; Windows Graphics Capture does.
    fn restart_capture_after_stall(&mut self) -> Result<(), WebSurfaceError> {
        Ok(())
    }

    /// Move this producer's native host binding to `parent_hwnd` while
    /// retaining its current page and browser session.
    ///
    /// This is presently implemented only by the Windows WebView2 composition
    /// producer. The caller must keep the destination top-level HWND alive for
    /// the rest of the producer's lifetime and call from the producer's UI
    /// thread. Other backends return [`WebSurfaceError::Unsupported`].
    ///
    /// # Safety
    ///
    /// `parent_hwnd` must be a live top-level native window handle on the
    /// producer's UI thread.
    unsafe fn reparent_to_hwnd(
        &mut self,
        parent_hwnd: *mut std::ffi::c_void,
    ) -> Result<(), WebSurfaceError> {
        let _ = parent_hwnd;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::reparent_to_hwnd is only implemented by the Windows WebView2 composition producer",
        ))
    }

    /// Begin a non-blocking inline-HTML navigation. Completion is observed via
    /// [`Self::poll_navigation_event`].
    fn load_html(&mut self, html: &str) -> Result<(), WebSurfaceError> {
        let _ = html;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::load_html is not implemented for this platform",
        ))
    }

    /// Begin a non-blocking URL navigation. Completion is observed via
    /// [`Self::poll_navigation_event`].
    fn load_url(&mut self, url: &str) -> Result<(), WebSurfaceError> {
        let _ = url;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::load_url is not implemented for this platform",
        ))
    }

    /// Navigate the underlying WebView to inline HTML and block until
    /// `NavigationCompleted` (or analog) fires, or the timeout elapses.
    /// Producers that don't yet support navigation return
    /// [`WebSurfaceError::Unsupported`].
    ///
    /// # ⚠️ Blocking — host-event-loop hazard
    ///
    /// On macOS this method pumps the main `NSRunLoop` to wait for
    /// the navigation delegate. Calling it from inside a host
    /// event-loop callback (e.g. winit's `resumed` / `window_event`
    /// under macOS, where winit guards against re-entrant handler
    /// invocation) will trigger a re-entrancy panic. From event-loop
    /// callbacks, prefer the non-blocking inherent
    /// [`crate::wkwebview_producer::WkWebViewProducer::load_html`]
    /// and observe completion via
    /// [`Self::poll_navigation_event`].
    fn navigate_to_string(
        &mut self,
        html: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        let _ = (html, timeout);
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::navigate_to_string is not implemented for this platform",
        ))
    }

    /// Resize the underlying WebView and capture region.
    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), WebSurfaceError> {
        let _ = size;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::resize is not implemented for this platform",
        ))
    }

    /// Reposition the underlying WebView overlay relative to the parent
    /// host, in physical pixels.
    fn set_offset(&mut self, x: f32, y: f32) -> Result<(), WebSurfaceError> {
        let _ = (x, y);
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::set_offset is not implemented for this platform",
        ))
    }

    /// Navigate the underlying WebView to a URL and block until
    /// `NavigationCompleted` fires (or the timeout elapses). Producers
    /// that don't yet support URL navigation return
    /// [`WebSurfaceError::Unsupported`].
    ///
    /// # ⚠️ Blocking — host-event-loop hazard
    ///
    /// Same caveat as [`Self::navigate_to_string`]: pumps the main
    /// `NSRunLoop` on macOS, panics if invoked from a winit event
    /// handler. Use [`crate::wkwebview_producer::WkWebViewProducer::load_url`]
    /// paired with [`Self::poll_navigation_event`] for non-blocking
    /// navigation from event-loop contexts.
    fn navigate_to_url(
        &mut self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        let _ = (url, timeout);
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::navigate_to_url is not implemented for this platform",
        ))
    }

    /// Forward a mouse / scroll event to the underlying webview.
    /// Coordinates are physical pixels relative to the webview's top-left
    /// corner.
    fn send_mouse_input(&mut self, event: MouseInput) -> Result<(), WebSurfaceError> {
        let _ = event;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::send_mouse_input is not implemented for this platform",
        ))
    }

    /// Move keyboard focus into the underlying webview. Hosts typically
    /// call this when the user clicks the webview region or tabs into it
    /// from a host control.
    fn move_focus(&mut self, reason: FocusReason) -> Result<(), WebSurfaceError> {
        let _ = reason;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::move_focus is not implemented for this platform",
        ))
    }

    /// Drain the next pending [`NavigationEvent`], if any. Returns `None`
    /// when no event is queued.
    ///
    /// Consumers should poll this each frame (or on demand) to reflect
    /// load progress in their UI. Events are queued FIFO per producer.
    fn poll_navigation_event(&mut self) -> Option<NavigationEvent> {
        None
    }

    /// Post a string message into the webview's `window.chrome.webview`
    /// listener. Producers that don't support JS messaging return
    /// [`WebSurfaceError::Unsupported`].
    fn post_web_message(&mut self, message: &str) -> Result<(), WebSurfaceError> {
        let _ = message;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::post_web_message is not implemented for this platform",
        ))
    }

    /// Drain the next pending message posted from JS via
    /// `window.chrome.webview.postMessage(...)`, if any. Messages are
    /// queued FIFO per producer.
    fn poll_web_message(&mut self) -> Option<String> {
        None
    }

    /// Set / overwrite one cookie in the webview's store.
    fn set_cookie(&mut self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        let _ = cookie;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::set_cookie is not implemented for this platform",
        ))
    }

    /// Execute JavaScript and return the engine's serialized result.
    fn execute_script_with_result(
        &mut self,
        script: &str,
        timeout: std::time::Duration,
    ) -> Result<String, WebSurfaceError> {
        let _ = (script, timeout);
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::execute_script_with_result is not implemented for this platform",
        ))
    }

    /// Take a one-shot PNG snapshot of the current webview document.
    /// Useful for thumbnails / previews / diagnostics; not a substitute
    /// for the live capture path. Producers that don't support snapshot
    /// capture return [`WebSurfaceError::Unsupported`].
    fn capture_snapshot_png(&mut self) -> Result<Vec<u8>, WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::capture_snapshot_png is not implemented for this platform",
        ))
    }

    /// Forward a touch / pen / pointer event to the webview.
    fn send_pointer_input(&mut self, event: PointerInput) -> Result<(), WebSurfaceError> {
        let _ = event;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::send_pointer_input is not implemented for this platform",
        ))
    }

    /// Forward a keyboard / modifier-state event to the webview. The
    /// host typically calls this when the webview is the focus target
    /// (see [`WebSurfaceProducer::move_focus`]) and the windowing
    /// system delivers a key event. Producers that don't yet support
    /// keyboard forwarding return [`WebSurfaceError::Unsupported`].
    fn send_keyboard_input(&mut self, event: KeyboardInput) -> Result<(), WebSurfaceError> {
        let _ = event;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::send_keyboard_input is not implemented for this platform",
        ))
    }

    /// Forward a drag / drop event to the webview. Hosts call this when
    /// the user drags content over the webview region.
    fn send_drag_input(&mut self, event: DragInput) -> Result<(), WebSurfaceError> {
        let _ = event;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::send_drag_input is not implemented for this platform",
        ))
    }

    /// Drain the next pending cursor shape requested by the webview.
    /// Producers that support cursor reporting push a fresh
    /// [`CursorShape`] each time the engine's hovered element changes.
    fn poll_cursor_shape(&mut self) -> Option<CursorShape> {
        None
    }

    /// Reload the current page (equivalent to the user pressing F5).
    fn reload(&mut self) -> Result<(), WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::reload is not implemented for this platform",
        ))
    }

    /// Stop loading the current navigation.
    fn stop(&mut self) -> Result<(), WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::stop is not implemented for this platform",
        ))
    }

    /// Navigate one entry back in the session history if possible. Returns
    /// `Ok(false)` if the back stack is empty, `Ok(true)` if a navigation
    /// was started.
    fn go_back(&mut self) -> Result<bool, WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::go_back is not implemented for this platform",
        ))
    }

    /// Navigate one entry forward in the session history if possible.
    fn go_forward(&mut self) -> Result<bool, WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::go_forward is not implemented for this platform",
        ))
    }

    /// Whether the back stack currently has at least one entry.
    fn can_go_back(&self) -> bool {
        false
    }

    /// Whether the forward stack currently has at least one entry.
    fn can_go_forward(&self) -> bool {
        false
    }

    /// Open the developer-tools UI for this webview.
    ///
    /// - **Windows**: opens the WebView2 DevTools window.
    /// - **macOS**: macOS WebKit doesn't expose a way to *open* the
    ///   inspector programmatically — Apple's public API only flips
    ///   the *attachable* bit (`setInspectable:`, macOS 13.3+, wired
    ///   via [`WebSurfaceSettings::devtools_enabled`]). The host or
    ///   user must then attach Safari's Web Inspector manually:
    ///   `Safari → Develop → <hostname> → <webview-page>` (Safari
    ///   must be running and Develop menu enabled in
    ///   `Safari → Settings → Advanced → Show features for web
    ///   developers`). Calling this on macOS returns `Unsupported`;
    ///   set `devtools_enabled = Some(true)` via `apply_settings`
    ///   first to make the WebView discoverable.
    /// - **Linux**: opens the WebKit Web Inspector.
    fn open_devtools_window(&mut self) -> Result<(), WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::open_devtools_window is not implemented for this platform",
        ))
    }

    /// Toggle the WebView's *page visibility* — what the page sees
    /// via the W3C Page Visibility API (`document.hidden`,
    /// `document.visibilityState`, the `visibilitychange` event).
    ///
    /// Browser-shape consumers call this when a tab becomes / stops
    /// being the active one in their tab UI. `false` causes:
    ///
    /// - `document.hidden = true`, `visibilityState = "hidden"`
    /// - `requestAnimationFrame` callbacks throttle to at most ~1 Hz
    /// - Background-tab autoplay / video-decoding throttles per
    ///   the engine's policy
    /// - Setinterval / setTimeout callbacks may coalesce
    ///
    /// This is the *light* throttle: the page still runs, just
    /// slower. It's distinct from the heavier
    /// `_setSuspended:` SPI-only path which fully pauses execution
    /// and is unsupported here. Most tabs-in-one-process consumers
    /// only need the light path.
    fn set_visible(&mut self, visible: bool) -> Result<(), WebSurfaceError> {
        let _ = visible;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::set_visible is not implemented for this platform",
        ))
    }

    /// Apply a partial settings update to the webview. Each `Some` field
    /// is applied; `None` fields are left at their current value.
    /// Producers report unsupported fields by ignoring them silently.
    fn apply_settings(&mut self, settings: &WebSurfaceSettings) -> Result<(), WebSurfaceError> {
        let _ = settings;
        Err(WebSurfaceError::Unsupported(
            "WebSurfaceProducer::apply_settings is not implemented for this platform",
        ))
    }
}

/// Conservative overlay-only producer used when no capture backend is available yet.
#[derive(Clone, Debug)]
pub struct OverlayOnlyProducer {
    capabilities: WebSurfaceCapabilities,
}

impl OverlayOnlyProducer {
    pub fn new(capabilities: WebSurfaceCapabilities) -> Self {
        Self { capabilities }
    }
}

impl WebSurfaceProducer for OverlayOnlyProducer {
    fn capabilities(&self) -> WebSurfaceCapabilities {
        self.capabilities.clone()
    }

    fn acquire_frame(&mut self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        Ok(WebSurfaceFrame::OverlayOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_frame_reports_overlay_mode() {
        assert_eq!(
            WebSurfaceFrame::OverlayOnly.mode(),
            WebSurfaceMode::NativeChildOverlay
        );
    }

    #[test]
    fn unknown_host_on_windows_does_not_promise_imported_texture() {
        let caps = probe_webview2(None);
        assert_eq!(caps.backend, SystemWebviewBackend::WebView2);
        assert_eq!(caps.preferred_mode, WebSurfaceMode::NativeChildOverlay);
        assert_eq!(
            caps.imported_texture,
            CapabilityStatus::Unsupported(
                crate::native_frame::UnsupportedReason::HostBackendUnavailable,
            )
        );
        assert_eq!(caps.features.cookies.read, CapabilityStatus::Supported);
        assert!(matches!(caps.features.cookies.same_site, CapabilityStatus::Unsupported(_)));
        assert!(matches!(caps.features.cookies.partitioned, CapabilityStatus::Unsupported(_)));
        assert_eq!(caps.features.script.result, CapabilityStatus::Supported);
        assert!(matches!(
            caps.features.script.exceptions,
            CapabilityStatus::Partial(_)
        ));
        assert_eq!(caps.features.pointer_input, CapabilityStatus::Supported);
        assert!(matches!(
            caps.features.drag_drop,
            CapabilityStatus::Partial(_)
        ));
        assert!(matches!(
            caps.features.accessibility,
            CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented)
        ));
        assert!(!caps.features.degradation_reasons.is_empty());
    }

    #[test]
    fn default_host_migration_is_explicitly_unsupported() {
        let mut producer = OverlayOnlyProducer::new(probe_webview2(None));
        let result = unsafe { producer.reparent_to_hwnd(0x1234usize as *mut _) };
        assert!(matches!(result, Err(WebSurfaceError::Unsupported(_))));
    }

    #[test]
    fn wkwebview_matrix_does_not_claim_unimplemented_operations() {
        let features = wkwebview_features();
        assert!(matches!(
            features.script.result,
            CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented)
        ));
        assert!(matches!(features.devtools, CapabilityStatus::Partial(_)));
        assert_eq!(features.downloads, CapabilityStatus::Supported);
        assert!(matches!(features.page_capture, CapabilityStatus::Unsupported(_)));
    }

    #[test]
    fn indeterminate_host_migration_has_a_distinct_terminal_error() {
        let error = WebSurfaceError::HostMigrationIndeterminate("unknown native host".into());
        assert!(matches!(
            error,
            WebSurfaceError::HostMigrationIndeterminate(_)
        ));
    }
}

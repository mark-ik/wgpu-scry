// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Linux WPE producer (WPEPlatform headless).
//!
//! The Linux primary: a self-owned headless `WPEDisplay` +
//! `WebKitWebView` render into DMABUF buffers that scrying imports
//! through wgpu's Vulkan backend. GObject mechanics come from the `glib`
//! crate; only WPE-specific symbols are hand-written `extern "C"`
//! (see [`ffi`]). FFI is gated behind the `wpe` cargo feature; the
//! producer types compile without it so the `lib.rs` alias still builds.

#![cfg(target_os = "linux")]

mod config;
mod producer;

#[cfg(feature = "wpe")]
mod cookies;
#[cfg(feature = "wpe")]
mod cursor;
#[cfg(feature = "wpe")]
mod downloads;
#[cfg(feature = "wpe")]
mod ffi;
#[cfg(feature = "wpe")]
mod headless;
#[cfg(feature = "wpe")]
mod ime;
#[cfg(feature = "wpe")]
mod input;
#[cfg(feature = "wpe")]
mod navigation;
#[cfg(feature = "wpe")]
mod scheme_handler;
#[cfg(feature = "wpe")]
mod script_message;

pub use config::WpeProducerConfig;
pub use producer::WpeProducer;

use crate::native_frame::{CapabilityStatus, NativeFrameKind, UnsupportedReason};
use crate::{SystemWebviewBackend, WebSurfaceCapabilities, WebSurfaceFeatureCapabilities, WebSurfaceMode};
#[cfg(feature = "wpe")]
use crate::{CookieCapabilities, ScriptCapabilities};

pub(crate) fn linux_wpe_capabilities() -> WebSurfaceCapabilities {
    if cfg!(feature = "wpe") {
        WebSurfaceCapabilities {
            backend: SystemWebviewBackend::Wpe,
            preferred_mode: WebSurfaceMode::ImportedTexture,
            imported_texture: CapabilityStatus::Supported,
            native_child_overlay: CapabilityStatus::Unsupported(
                UnsupportedReason::PlatformNotImplemented,
            ),
            cpu_snapshot: CapabilityStatus::Unsupported(
                UnsupportedReason::PlatformNotImplemented,
            ),
            supported_frames: vec![NativeFrameKind::DmaBufImage],
            reason: "WPE is the Linux primary backend: a headless WPEDisplay + WebKitWebView produces DmaBufImage frames, and a compatible wgpu Vulkan host imports them through Graft.",
            features: wpe_features(),
        }
    } else {
        WebSurfaceCapabilities {
            backend: SystemWebviewBackend::Wpe,
            preferred_mode: WebSurfaceMode::Unsupported,
            imported_texture: CapabilityStatus::Unsupported(
                UnsupportedReason::PlatformNotImplemented,
            ),
            native_child_overlay: CapabilityStatus::Unsupported(
                UnsupportedReason::PlatformNotImplemented,
            ),
            cpu_snapshot: CapabilityStatus::Unsupported(
                UnsupportedReason::PlatformNotImplemented,
            ),
            supported_frames: Vec::new(),
            reason: "WPE is a compile-only producer shell until the `wpe` feature is enabled; enable it to build the WPEPlatform FFI bridge and DMABUF frame production.",
            features: wpe_features(),
        }
    }
}

fn wpe_features() -> WebSurfaceFeatureCapabilities {
    #[cfg(feature = "wpe")]
    {
        WebSurfaceFeatureCapabilities {
            cookies: CookieCapabilities {
                read: CapabilityStatus::Supported,
                write: CapabilityStatus::Supported,
                delete: CapabilityStatus::Supported,
                change_events: CapabilityStatus::Unsupported(
                    UnsupportedReason::PlatformNotImplemented,
                ),
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
                bounded_blocking: CapabilityStatus::Unsupported(
                    UnsupportedReason::PlatformNotImplemented,
                ),
            },
            page_capture: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            devtools: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            downloads: CapabilityStatus::Supported,
            popups: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            drag_drop: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            pointer_input: CapabilityStatus::Partial(
                "WPE does not emit Enter, Leave, or CaptureChanged pointer events through its native mapping.",
            ),
            ime: CapabilityStatus::Partial(
                "WPE exposes host keyboard/commit and caret observability, but does not forward native preedit text.",
            ),
            accessibility: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
            degradation_reasons: vec![
                "WPE has no portable PNG page-snapshot operation.",
                "WPE has no producer-owned Web Inspector or popup routing API.",
                "WPE host drag payload synthesis and cookie-change events are not implemented.",
            ],
        }
    }
    #[cfg(not(feature = "wpe"))]
    {
        WebSurfaceFeatureCapabilities::unsupported(
            UnsupportedReason::PlatformNotImplemented,
            "WPE runtime operations are unavailable because the `wpe` feature is disabled.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "wpe"))]
    #[test]
    fn compile_only_wpe_does_not_claim_a_frame_path() {
        let caps = linux_wpe_capabilities();
        assert_eq!(caps.preferred_mode, WebSurfaceMode::Unsupported);
        assert!(matches!(
            caps.imported_texture,
            CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented)
        ));
        assert!(caps.supported_frames.is_empty());
        assert!(caps.reason.contains("compile-only"));
        assert!(matches!(
            caps.features.cookies.read,
            CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented)
        ));
        assert!(matches!(
            caps.features.pointer_input,
            CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented)
        ));
    }

    #[cfg(feature = "wpe")]
    #[test]
    fn live_wpe_claims_dmabuf_import() {
        let caps = linux_wpe_capabilities();
        assert_eq!(caps.preferred_mode, WebSurfaceMode::ImportedTexture);
        assert_eq!(caps.imported_texture, CapabilityStatus::Supported);
        assert_eq!(caps.supported_frames, vec![NativeFrameKind::DmaBufImage]);
        assert!(matches!(caps.features.pointer_input, CapabilityStatus::Partial(_)));
        assert!(matches!(caps.features.ime, CapabilityStatus::Partial(_)));
    }
}

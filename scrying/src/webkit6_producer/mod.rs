// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Linux WebKitGTK 6.0 (GTK 4 / libadwaita-era) producer.
//!
//! Sibling to [`crate::webkitgtk_producer`] (GTK 3 / WebKitGTK 4.1).
//! Selected via the `webkit6` cargo feature, which pulls
//! `gtk4 = "0.11"` + `webkit6 = "0.6"` (+ their transitive
//! glib 0.22 / gdk4 / gio / soup3 0.9 / javascriptcore 1.x) and
//! supersedes `webkitgtk-fallback`'s GTK 3 stack when enabled.
//!
//! ## Architecture differences from the GTK 3 producer
//!
//! GTK 4 removed `GtkOffscreenWindow`. To host a WebView without a
//! visible window we create a top-level `gtk4::Window`, parent the
//! WebView via `window.set_child(...)`, and explicitly `realize()`
//! it — never calling `present()`. WebKit's GPU process renders
//! independently of GTK widget visibility, so snapshots work; only
//! the widget's input routing is degraded (GTK 4 doesn't accept
//! synthetic events through the old `gtk_main_do_event` path).
//!
//! ## Phase coverage
//!
//! First-slice scope (this commit): navigate + offscreen-rendered
//! CPU snapshot via `webkit_web_view_get_snapshot` →
//! [`gdk::Texture::download`] → un-premultiplied RGBA →
//! [`crate::WebSurfaceFrame::CpuRgba`]. Cookies / URL schemes /
//! input forwarding / IME / cursor reporting / popup intercept /
//! downloads all parallel-port to follow-on slices.

#![cfg(all(target_os = "linux", feature = "webkit6"))]

mod capture;
mod config;
mod cookies;
mod cursor;
mod downloads;
mod helpers;
mod ime;
mod input;
mod navigation;
mod producer;
mod scheme_handler;
mod script_message;
mod trait_impl;

use crate::native_frame::{CapabilityStatus, UnsupportedReason};
use crate::{
    CookieCapabilities, ScriptCapabilities, SystemWebviewBackend,
    WebSurfaceCapabilities, WebSurfaceFeatureCapabilities, WebSurfaceMode,
};

pub use config::WebKit6ProducerConfig;
pub use producer::WebKit6Producer;

/// The CPU snapshot path is demand-driven. `try_acquire_frame` therefore
/// returns `None` without initiating a snapshot; callers that deliberately
/// accept a blocking capture must call `acquire_frame`.
pub(crate) fn linux_webkit6_capabilities() -> WebSurfaceCapabilities {
    WebSurfaceCapabilities {
        backend: SystemWebviewBackend::WebKitGtk,
        preferred_mode: WebSurfaceMode::CpuSnapshot,
        imported_texture: CapabilityStatus::Unsupported(
            UnsupportedReason::NativeImportNotYetImplemented,
        ),
        native_child_overlay: CapabilityStatus::Unsupported(
            UnsupportedReason::PlatformNotImplemented,
        ),
        cpu_snapshot: CapabilityStatus::Supported,
        supported_frames: Vec::new(),
        reason: "WebKitGTK 6.0 / GTK 4 producer: hidden gtk4::Window hosting the WebKitWebView, CpuRgba snapshots via webkit_web_view_get_snapshot → gdk::Texture::download, with input, cookie, download, and scheme-handler bridges wired; the capability matrix reports remaining degradations explicitly.",
        features: gtk6_features(),
    }
}

fn gtk6_features() -> WebSurfaceFeatureCapabilities {
    WebSurfaceFeatureCapabilities {
        cookies: CookieCapabilities {
            read: CapabilityStatus::Supported,
            write: CapabilityStatus::Supported,
            delete: CapabilityStatus::Supported,
            change_events: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
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
        page_capture: CapabilityStatus::Supported,
        devtools: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
        downloads: CapabilityStatus::Supported,
        popups: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
        drag_drop: CapabilityStatus::Partial(
            "GTK 4 drag forwarding synthesizes DOM events without a native data payload.",
        ),
        pointer_input: CapabilityStatus::Partial(
            "GTK 4 pointer forwarding is JS-synthesized and may not preserve native event trust or device metadata.",
        ),
        ime: CapabilityStatus::Partial(
            "GTK 4 exposes host keyboard/commit and caret observability, but does not forward native preedit text.",
        ),
        accessibility: CapabilityStatus::Unsupported(UnsupportedReason::PlatformNotImplemented),
        degradation_reasons: vec![
            "GTK 4 CPU snapshots are blocking and must be requested through acquire_frame.",
            "GTK 4 cookie-change events, developer tools, and accessibility-tree export are not exposed by this producer.",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_matrix_names_gtk6_degradations() {
        let caps = linux_webkit6_capabilities();
        assert_eq!(caps.features.page_capture, CapabilityStatus::Supported);
        assert!(matches!(caps.features.cookies.partitioned, CapabilityStatus::Unsupported(_)));
        assert!(matches!(caps.features.drag_drop, CapabilityStatus::Partial(_)));
        assert!(matches!(caps.features.ime, CapabilityStatus::Partial(_)));
        assert!(matches!(caps.features.devtools, CapabilityStatus::Unsupported(_)));
    }
}

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
mod helpers;
mod navigation;
mod producer;
mod script_message;
mod trait_impl;

use crate::native_frame::{CapabilityStatus, UnsupportedReason};
use crate::{SystemWebviewBackend, WebSurfaceCapabilities, WebSurfaceMode};

pub use config::WebKit6ProducerConfig;
pub use producer::WebKit6Producer;

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
        reason: "WebKitGTK 6.0 / GTK 4 producer (Phase 5 first slice): hidden gtk4::Window hosting the WebKitWebView, CpuRgba snapshots via webkit_web_view_get_snapshot → gdk::Texture::download. Input forwarding / cookies / scheme handlers / etc. are follow-on slices.",
    }
}

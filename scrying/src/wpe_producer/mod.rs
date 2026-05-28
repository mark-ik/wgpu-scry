//! Linux WPE producer (WPEPlatform headless).
//!
//! The planned Linux primary: a self-owned headless `WPEDisplay` +
//! `WebKitWebView` render into DMABUF buffers that scrying imports
//! through wgpu's Vulkan backend. GObject mechanics come from the `glib`
//! crate; only WPE-specific symbols are hand-written `extern "C"`
//! (see [`ffi`]). FFI is gated behind the `wpe` cargo feature; the
//! producer types compile without it so the `lib.rs` alias still builds.

#![cfg(target_os = "linux")]

mod config;
mod producer;

#[cfg(feature = "wpe")]
mod ffi;
#[cfg(feature = "wpe")]
mod headless;

pub use config::WpeProducerConfig;
pub use producer::WpeProducer;

use crate::native_frame::{CapabilityStatus, NativeFrameKind, UnsupportedReason};
use crate::{SystemWebviewBackend, WebSurfaceCapabilities, WebSurfaceMode};

pub(crate) fn linux_wpe_capabilities() -> WebSurfaceCapabilities {
    WebSurfaceCapabilities {
        backend: SystemWebviewBackend::Wpe,
        preferred_mode: WebSurfaceMode::Unsupported,
        imported_texture: CapabilityStatus::Unsupported(
            UnsupportedReason::NativeImportNotYetImplemented,
        ),
        native_child_overlay: CapabilityStatus::Unsupported(
            UnsupportedReason::PlatformNotImplemented,
        ),
        cpu_snapshot: CapabilityStatus::Unsupported(
            UnsupportedReason::NativeImportNotYetImplemented,
        ),
        supported_frames: vec![NativeFrameKind::DmaBufImage],
        reason: "WPE is the planned Linux primary backend (DMABUF + Vulkan external memory); the producer API and DMABUF frame contract are present, but the WPE FFI callback bridge and Vulkan importer are not wired yet.",
    }
}

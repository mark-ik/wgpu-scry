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
#[cfg(feature = "wpe")]
mod input;
#[cfg(feature = "wpe")]
mod navigation;

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
        reason: if cfg!(feature = "wpe") {
            "WPE is the Linux primary backend (DMABUF + Vulkan external memory); the producer constructs a headless WPEDisplay + WebKitWebView and the buffer-rendered seam produces DmaBufImage frames. The wgpu-side importer integration is the remaining wiring."
        } else {
            "WPE producer is compiled as a no-op scaffold; rebuild with `--features wpe` to enable the WPEPlatform FFI bridge and DMABUF frame production."
        },
    }
}

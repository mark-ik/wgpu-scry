use std::sync::{Arc, Mutex};

use dpi::PhysicalSize;

use crate::native_frame::{DmaBufImage, NativeFrame, SyncMechanism};
use crate::{
    WebSurfaceCapabilities, WebSurfaceError, WebSurfaceFrame, WebSurfaceProducer,
};

use super::WpeProducerConfig;

/// Owned GObject handles for the WPE headless producer.
///
/// All fields live as long as the `WpeProducer` that contains them.
#[cfg(feature = "wpe")]
pub(super) struct WpeHandles {
    /// Owns the WebKitWebView (and, transitively, the bound headless display
    /// and ephemeral network session).
    pub webview: glib::Object,
    /// Raw WPEView pointer borrowed from the webview; valid for the webview's
    /// lifetime (i.e. for the lifetime of this struct).
    pub view: *mut super::ffi::WPEView,
    /// GLib main context the producer is affine to; pumped by
    /// acquire/navigate calls.
    pub main_context: glib::MainContext,
}


/// Linux WPE producer — constructs and owns a headless WPEPlatform display,
/// a `WebKitWebView` bound to that display, and the associated `WPEView`.
/// Frames arrive as DMABUF exports that scrying imports through wgpu's Vulkan
/// external-memory path. All GObject lifetime management is handled internally;
/// callers interact only through the `WebSurfaceProducer` trait.
pub struct WpeProducer {
    pub(super) capabilities: WebSurfaceCapabilities,
    pub(super) size: PhysicalSize<u32>,
    pub(super) offset: (f32, f32),
    pub(super) pending_frame: Arc<Mutex<Option<DmaBufImage>>>,
    pub(super) generation: u64,
    #[cfg(feature = "wpe")]
    pub(super) handles: WpeHandles,
}

impl WpeProducer {
    #[cfg(feature = "wpe")]
    pub fn new(config: WpeProducerConfig) -> Result<Self, crate::WebSurfaceError> {
        use crate::WebSurfaceError;
        if config.size.width == 0 || config.size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WPE producer size must be non-zero, got {}x{}",
                config.size.width, config.size.height
            )));
        }
        let main_context = glib::MainContext::default();
        let (webview, view) = super::headless::build_producer_view()?;
        Ok(Self {
            capabilities: super::linux_wpe_capabilities(),
            size: config.size,
            offset: config.offset,
            pending_frame: Arc::new(Mutex::new(None)),
            generation: 0,
            handles: WpeHandles { webview, view, main_context },
        })
    }

    #[cfg(not(feature = "wpe"))]
    pub fn new(config: WpeProducerConfig) -> Result<Self, crate::WebSurfaceError> {
        if config.size.width == 0 || config.size.height == 0 {
            return Err(crate::WebSurfaceError::Platform(format!(
                "WPE producer size must be non-zero, got {}x{}",
                config.size.width, config.size.height
            )));
        }
        Ok(Self {
            capabilities: super::linux_wpe_capabilities(),
            size: config.size,
            offset: config.offset,
            pending_frame: Arc::new(Mutex::new(None)),
            generation: 0,
        })
    }

    /// Queue a DMABUF frame from the WPE backend callback.
    ///
    /// This is the seam the Linux FFI bridge should call when
    /// `WPEViewBackendDMABuf` exports a fresh buffer. It is public so a Linux
    /// smoke harness can inject a known frame before the real callback bridge
    /// is complete.
    pub fn enqueue_dmabuf_frame(&mut self, mut frame: DmaBufImage) -> Result<(), WebSurfaceError> {
        if frame.size.width == 0 || frame.size.height == 0 {
            return Err(WebSurfaceError::Platform(
                "WPE DMABUF frame size must be non-zero".to_string(),
            ));
        }
        if frame.planes.is_empty() {
            return Err(WebSurfaceError::Platform(
                "WPE DMABUF frame did not include any planes".to_string(),
            ));
        }
        self.generation = self.generation.saturating_add(1);
        frame.generation = self.generation;
        frame.producer_sync = if frame.semaphore_fd.is_some() {
            SyncMechanism::ExplicitExternalSemaphore
        } else {
            SyncMechanism::None
        };
        *self.pending_frame.lock().map_err(|_| {
            WebSurfaceError::Platform("WPE pending frame mutex was poisoned".to_string())
        })? = Some(frame);
        Ok(())
    }

    /// Non-blocking acquire. Returns the newest queued DMABUF frame, if any.
    pub fn try_acquire_frame(&mut self) -> Result<Option<WebSurfaceFrame>, WebSurfaceError> {
        let Some(frame) = self
            .pending_frame
            .lock()
            .map_err(|_| {
                WebSurfaceError::Platform("WPE pending frame mutex was poisoned".to_string())
            })?
            .take()
        else {
            return Ok(None);
        };
        Ok(Some(WebSurfaceFrame::Native(NativeFrame::DmaBufImage(
            frame,
        ))))
    }

    pub fn offset(&self) -> (f32, f32) {
        self.offset
    }
}

impl WebSurfaceProducer for WpeProducer {
    fn capabilities(&self) -> WebSurfaceCapabilities {
        self.capabilities.clone()
    }

    fn acquire_frame(&mut self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        self.try_acquire_frame()?
            .ok_or(WebSurfaceError::Unsupported(
                "WpeProducer has no queued DMABUF frame; WPE callback bridge is not wired yet",
            ))
    }

    fn navigate_to_string(
        &mut self,
        _html: &str,
        _timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WpeProducer::navigate_to_string is waiting on the WPE WebKit FFI bridge",
        ))
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), WebSurfaceError> {
        if size.width == 0 || size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WPE producer size must be non-zero, got {}x{}",
                size.width, size.height
            )));
        }
        self.size = size;
        Ok(())
    }

    fn set_offset(&mut self, x: f32, y: f32) -> Result<(), WebSurfaceError> {
        self.offset = (x, y);
        Ok(())
    }
}

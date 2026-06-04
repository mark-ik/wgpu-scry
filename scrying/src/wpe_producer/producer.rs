use std::sync::atomic::{AtomicU64, Ordering};
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
///
/// `webview` and `main_context` are held for their Drop / pump side effects
/// (the field-Drop of `webview` releases the WebView's refs on display and
/// network-session; `main_context` is pumped by acquire/navigate helpers in
/// tests and in 4c.3). Their fields are not read directly here.
#[cfg(feature = "wpe")]
#[allow(dead_code)]
pub(super) struct WpeHandles {
    /// Owns the WebKitWebView (and, transitively, the bound headless display
    /// and ephemeral network session).
    pub webview: glib::Object,
    /// Raw WPEView pointer borrowed from the webview; valid for the webview's
    /// lifetime (i.e. for the lifetime of this struct).
    pub view: *mut super::ffi::WPEView,
    /// Borrowed from the view (transfer-none); valid for the view's lifetime.
    /// `resize` calls `wpe_toplevel_resize` against this. NOT unref'd in Drop
    /// — the view (held alive transitively via webview) owns it.
    pub toplevel: *mut super::ffi::WPEToplevel,
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
    /// Monotonic frame counter shared with the `buffer-rendered` closure (which
    /// owns only a clone, not `&mut self`). Each submitted frame stamps the
    /// pre-increment value + 1.
    pub(super) generation: Arc<AtomicU64>,
    #[cfg(feature = "wpe")]
    pub(super) handles: WpeHandles,
    #[cfg(feature = "wpe")]
    pub(super) nav_state: std::rc::Rc<std::cell::RefCell<super::navigation::NavState>>,
}

/// The single-slot frame channel a producer's render callback writes into.
///
/// Cloned into the `buffer-rendered` glib closure so the callback (which holds
/// no `&mut WpeProducer`) can publish frames and advance the shared generation.
/// `submit` closes the fds of any frame it evicts so a consumer that falls
/// behind the producer can't leak the dup'd plane descriptors.
#[derive(Clone)]
pub(super) struct FrameSink {
    pub pending: Arc<Mutex<Option<DmaBufImage>>>,
    /// Read only by the `buffer-rendered` closure (wpe-only); without the `wpe`
    /// feature `enqueue_dmabuf_frame` stamps the generation directly on the
    /// producer, so the sink's copy is unread there.
    #[cfg_attr(not(feature = "wpe"), allow(dead_code))]
    pub generation: Arc<AtomicU64>,
}

impl FrameSink {
    /// Store a new frame; close the fds of any evicted stale frame first.
    pub fn submit(&self, frame: DmaBufImage) {
        let mut slot = match self.pending.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        if let Some(old) = slot.take() {
            close_frame_fds(&old);
        }
        *slot = Some(frame);
    }
}

/// Close the dup'd fds a producer owns for a frame not handed to the importer.
///
/// A `DmaBufImage` has no `Drop` (its fds are managed by contract — ownership
/// transfers to the Vulkan importer once handed off). This closes the plane and
/// semaphore fds for frames the producer evicts before the consumer takes them.
pub(super) fn close_frame_fds(frame: &DmaBufImage) {
    for plane in &frame.planes {
        // SAFETY: producer-owned dup'd fd not yet transferred to the Vulkan importer.
        unsafe {
            libc::close(plane.fd);
        }
    }
    if let Some(fd) = frame.semaphore_fd {
        // SAFETY: producer-owned dup'd semaphore fd, likewise not yet transferred.
        unsafe {
            libc::close(fd);
        }
    }
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
        let (webview, view, toplevel) = super::headless::build_producer_view()?;
        let nav_state = std::rc::Rc::new(std::cell::RefCell::new(
            super::navigation::NavState::default(),
        ));
        super::navigation::install_load_signals(&webview, &nav_state);
        let producer = Self {
            capabilities: super::linux_wpe_capabilities(),
            size: config.size,
            offset: config.offset,
            pending_frame: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            handles: WpeHandles { webview, view, toplevel, main_context },
            nav_state,
        };
        // Wire the WPEView frame seam now that the producer (and thus its
        // shared FrameSink) exists. The closure captures a FrameSink clone and
        // the raw view pointer; the connection persists on the underlying
        // GObject for the producer's lifetime (the webview keeps the view alive).
        let view_obj: glib::Object = unsafe {
            glib::translate::from_glib_none(
                producer.handles.view as *mut glib::gobject_ffi::GObject,
            )
        };
        super::headless::connect_buffer_rendered(
            &view_obj,
            producer.handles.view,
            producer.frame_sink(),
        );
        Ok(producer)
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
            generation: Arc::new(AtomicU64::new(0)),
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
        frame.generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        frame.producer_sync = if frame.semaphore_fd.is_some() {
            SyncMechanism::ExplicitExternalSemaphore
        } else {
            SyncMechanism::None
        };
        // Route through the shared sink so a stale evicted frame's dup'd fds are
        // closed (same path the `buffer-rendered` closure uses).
        self.frame_sink().submit(frame);
        Ok(())
    }

    /// The shared single-slot frame channel + generation counter, cloneable for
    /// the `buffer-rendered` closure (which holds no `&mut self`).
    pub(super) fn frame_sink(&self) -> FrameSink {
        FrameSink {
            pending: self.pending_frame.clone(),
            generation: self.generation.clone(),
        }
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

#[cfg(feature = "wpe")]
impl WpeProducer {
    /// Non-blocking HTML load. Companion `wait_for_load` (or the trait's
    /// `navigate_to_string`) drives completion.
    ///
    /// Clears prior `finished`/`failed` state via `arm_navigation` before
    /// the load, so back-to-back `load_html(...); wait_for_load(...)` works
    /// the second time too. Mirrors `webkitgtk_producer::Producer::load_html`.
    pub fn load_html(&self, html: &str, base_uri: Option<&str>) {
        super::navigation::arm_navigation(&self.nav_state);
        use glib::translate::ToGlibPtr;
        let raw: *mut super::ffi::WebKitWebView =
            ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&self.handles.webview).0
                as *mut _;
        let c_html = std::ffi::CString::new(html)
            .unwrap_or_else(|_| std::ffi::CString::new("").unwrap());
        let c_base = base_uri.and_then(|s| std::ffi::CString::new(s).ok());
        // SAFETY: `raw` is borrowed from the owned `webview`; load_html copies
        // both strings before returning.
        unsafe {
            super::ffi::webkit_web_view_load_html(
                raw,
                c_html.as_ptr(),
                c_base.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
            );
        }
    }

    /// Non-blocking URI load. Companion `wait_for_load` (or the trait's
    /// `navigate_to_url`) drives completion.
    ///
    /// Clears prior `finished`/`failed` state via `arm_navigation` before
    /// the load. Mirrors `webkitgtk_producer::Producer::load_uri`.
    pub fn load_uri(&self, uri: &str) {
        super::navigation::arm_navigation(&self.nav_state);
        use glib::translate::ToGlibPtr;
        let raw: *mut super::ffi::WebKitWebView =
            ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&self.handles.webview).0
                as *mut _;
        let c_uri = std::ffi::CString::new(uri)
            .unwrap_or_else(|_| std::ffi::CString::new("about:blank").unwrap());
        // SAFETY: raw borrowed; load_uri copies the string before returning.
        unsafe { super::ffi::webkit_web_view_load_uri(raw, c_uri.as_ptr()); }
    }

    /// Pump the producer's main context until the most recent navigation
    /// finishes or `timeout` elapses.
    pub fn wait_for_load(
        &self,
        timeout: std::time::Duration,
    ) -> Result<(), crate::WebSurfaceError> {
        super::navigation::wait_for_load(&self.handles.main_context, &self.nav_state, timeout)
    }
}

impl Drop for WpeProducer {
    fn drop(&mut self) {
        // Close fds on any frame that was queued but never handed to the
        // importer. Mutex poisoning -> still take and close (best effort).
        //
        // Drop order is load-bearing: `pending_frame` is declared above
        // `handles` in `WpeProducer`, so Rust drops it (and any frame fds
        // we just took) BEFORE `handles.webview`'s field-Drop tears the
        // WebView down and disconnects the buffer-rendered closure. Don't
        // reorder those struct fields without revisiting this.
        let slot = match self.pending_frame.lock() {
            Ok(mut s) => s.take(),
            Err(p) => p.into_inner().take(),
        };
        if let Some(frame) = slot {
            close_frame_fds(&frame);
        }
        // GObject handles (under feature = "wpe") drop via their own field
        // Drops — webview unref propagates to display + network-session.
    }
}

impl WebSurfaceProducer for WpeProducer {
    fn capabilities(&self) -> WebSurfaceCapabilities {
        self.capabilities.clone()
    }

    fn acquire_frame(&mut self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        self.try_acquire_frame()?
            .ok_or(WebSurfaceError::NotReady(
                "no DMABUF frame queued yet; pump the producer's main context",
            ))
    }

    fn navigate_to_string(
        &mut self,
        html: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            // `load_html` arms the nav state internally — no double-arm here.
            self.load_html(html, None);
            self.wait_for_load(timeout)
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = (html, timeout);
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }

    fn navigate_to_url(
        &mut self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            // `load_uri` arms the nav state internally — no double-arm here.
            self.load_uri(url);
            self.wait_for_load(timeout)
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = (url, timeout);
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }

    /// Resize the producer's render target.
    ///
    /// **NOTE on `self.size`:** this method updates `self.size` to the
    /// REQUESTED size if `wpe_toplevel_resize` returns TRUE. On the
    /// headless WPE display in WPE 2.52.3, the toplevel silently
    /// coerces all dimensions to its default (1024×768 in current
    /// testing) — `wpe_toplevel_resize` still returns TRUE, but the
    /// next `DmaBufImage` reflects the coerced size, not `self.size`.
    /// Callers that need the true rendered dimensions should read them
    /// off the most recent `DmaBufImage::size`, not from this method's
    /// updated `self.size`. Honoring the requested size on headless
    /// is tracked for 4c.4+.
    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), WebSurfaceError> {
        if size.width == 0 || size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WPE producer size must be non-zero, got {}x{}",
                size.width, size.height
            )));
        }
        #[cfg(feature = "wpe")] {
            // SAFETY: handles.toplevel is non-null per the construction guard
            // in build_producer_view; transfer-none borrow, valid for the
            // view's (and producer's) lifetime.
            let ok = unsafe {
                super::ffi::wpe_toplevel_resize(
                    self.handles.toplevel,
                    size.width as std::os::raw::c_int,
                    size.height as std::os::raw::c_int,
                )
            };
            if ok == 0 {
                return Err(WebSurfaceError::Platform(
                    format!("wpe_toplevel_resize returned FALSE for {}x{}",
                            size.width, size.height),
                ));
            }
        }
        self.size = size;
        Ok(())
    }

    fn set_offset(&mut self, x: f32, y: f32) -> Result<(), WebSurfaceError> {
        self.offset = (x, y);
        Ok(())
    }

    fn poll_navigation_event(&mut self) -> Option<crate::NavigationEvent> {
        #[cfg(feature = "wpe")] {
            self.nav_state.borrow_mut().events.pop_front()
        }
        #[cfg(not(feature = "wpe"))] { None }
    }

    fn send_keyboard_input(&mut self, event: crate::KeyboardInput) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            // SAFETY: handles.view is non-null per the construction guard;
            // dispatch_keyboard is single-threaded on the producer's thread.
            unsafe { super::input::dispatch_keyboard(self.handles.view, &event); }
            Ok(())
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = event;
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }

    fn send_mouse_input(&mut self, event: crate::MouseInput) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            unsafe { super::input::dispatch_mouse(self.handles.view, &event); }
            Ok(())
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = event;
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }

    fn send_pointer_input(&mut self, event: crate::PointerInput) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            unsafe { super::input::dispatch_pointer(self.handles.view, &event) }
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = event;
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }
}

#[cfg(test)]
mod fd_tests {
    use super::*;
    use crate::native_frame::{DmaBufImage, DmaBufPlane, SyncMechanism};

    // Serialize all fd tests so parallel test threads don't recycle fd
    // numbers between the close-under-test and the fd_open assertion.
    static FD_TEST_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    fn fd_test_lock() -> std::sync::MutexGuard<'static, ()> {
        FD_TEST_LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Open a one-shot pipe fd we can watch for closure. Closes the write
    /// end so only the read end is observable.
    fn pipe_fd() -> i32 {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { libc::close(fds[1]) }; // close write end, keep read end
        fds[0]
    }

    /// True iff the fd is still open in this process.
    fn fd_open(fd: i32) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    fn frame_with_fd(fd: i32) -> DmaBufImage {
        DmaBufImage {
            size: dpi::PhysicalSize::new(4, 4),
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            drm_format: 0,
            drm_modifier: 0,
            planes: vec![DmaBufPlane { fd, offset: 0, stride: 16 }],
            generation: 0,
            producer_sync: SyncMechanism::None,
            semaphore_fd: None,
        }
    }

    #[test]
    fn evicting_stale_frame_closes_its_fds() {
        let _guard = fd_test_lock();
        let sink = FrameSink {
            pending: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        };
        let stale_fd = pipe_fd();
        sink.submit(frame_with_fd(stale_fd));
        let fresh_fd = pipe_fd();
        sink.submit(frame_with_fd(fresh_fd)); // evicts stale -> must close stale_fd
        assert!(!fd_open(stale_fd), "stale frame's fd must be closed on eviction");
        assert!(fd_open(fresh_fd),  "fresh frame's fd must remain open");
    }

    // Gated to the non-wpe build so this test exercises Drop without
    // standing up a real WPE display (which would (a) violate the
    // one-WPE-per-process constraint documented in headless.rs and (b)
    // collide with the smoke test in --features wpe runs). The Drop
    // logic itself is feature-independent.
    #[cfg(not(feature = "wpe"))]
    #[test]
    fn dropping_producer_closes_unconsumed_fd() {
        let _guard = fd_test_lock();
        use crate::wpe_producer::WpeProducerConfig;
        let leftover_fd = pipe_fd();
        {
            let producer = WpeProducer::new(WpeProducerConfig::new(
                dpi::PhysicalSize::new(8, 8),
                std::env::temp_dir(),
            )).expect("non-wpe stub constructor must succeed");
            // Inject an unconsumed frame directly into the producer's slot.
            *producer.pending_frame.lock().unwrap() = Some(frame_with_fd(leftover_fd));
            // (producer goes out of scope here -> Drop must close the fd)
        }
        assert!(!fd_open(leftover_fd), "unconsumed frame's fd must be closed when producer drops");
    }
}

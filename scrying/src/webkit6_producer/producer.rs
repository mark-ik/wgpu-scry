//! [`WebKit6Producer`] struct, construction, Drop.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use dpi::PhysicalSize;
use webkit6::gtk;
use webkit6::gtk::prelude::*;
use webkit6::{NetworkSession, UserContentManager, WebContext, WebView};

use crate::{WebSurfaceCapabilities, WebSurfaceError};

use super::config::WebKit6ProducerConfig;
use super::helpers::ensure_gtk_init;
use super::navigation::{NavState, install_load_signals};
use super::script_message;

/// Linux WebKitGTK 6.0 producer. Hosts a `WebKitWebView` inside a
/// hidden top-level `gtk4::Window` and emits CPU RGBA snapshots via
/// `webkit_web_view_get_snapshot` (→ `gdk::Texture`).
pub struct WebKit6Producer {
    pub(crate) capabilities: WebSurfaceCapabilities,
    pub(crate) webview: WebView,
    pub(crate) window: gtk::Window,
    pub(crate) _context: WebContext,
    /// WebKitGTK 6.0 split website-data + cache directories out of
    /// `WebContext` into a separate `NetworkSession` (1:1 with a
    /// WebView). Held for lifetime — the WebView refs it internally.
    pub(crate) _network_session: NetworkSession,
    pub(crate) size: PhysicalSize<u32>,
    pub(crate) offset: (f32, f32),
    pub(crate) generation: Cell<u64>,
    pub(crate) nav_state: Rc<RefCell<NavState>>,
    /// FIFO of incoming `window.webkit.messageHandlers.scry.postMessage`
    /// payloads from page JS. Drained by
    /// [`crate::WebSurfaceProducer::poll_web_message`] or pumped via
    /// [`Self::wait_for_web_message`]. Installed by
    /// [`super::script_message::install`] in [`Self::new`].
    pub(crate) web_messages: Rc<RefCell<VecDeque<String>>>,
}

impl WebKit6Producer {
    /// Construct the producer. Initializes GTK 4 if needed (must be
    /// on the GTK main thread), creates a persistent `WebContext`
    /// rooted at `config.data_dir`, builds the WebView inside a
    /// hidden top-level `gtk4::Window`, and wires load-event
    /// signal handlers.
    pub fn new(config: WebKit6ProducerConfig) -> Result<Self, WebSurfaceError> {
        ensure_gtk_init()?;

        if config.size.width == 0 || config.size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WebKit6 producer size must be non-zero, got {}x{}",
                config.size.width, config.size.height
            )));
        }
        std::fs::create_dir_all(&config.data_dir).map_err(|err| {
            WebSurfaceError::Platform(format!(
                "could not create data_dir {:?}: {err}",
                config.data_dir
            ))
        })?;

        // WebKitGTK 6.0 moved data + cache directory configuration
        // out of `WebContext` (which is process-wide) and into a new
        // `NetworkSession` type that's 1:1 with the WebView. Build
        // both and wire them to the view via `WebViewBuilder`.
        let data_dir_str = config.data_dir.to_string_lossy().to_string();
        let network_session = NetworkSession::new(Some(&data_dir_str), Some(&data_dir_str));
        let context = WebContext::new();
        // Build an explicit `UserContentManager` so the JS-messaging
        // bridge has a stable handle to register handlers + inject
        // user scripts against. `WebView::builder().user_content_manager(...)`
        // wires it via the GObject construct property.
        let ucm = UserContentManager::new();
        let webview = WebView::builder()
            .web_context(&context)
            .network_session(&network_session)
            .user_content_manager(&ucm)
            .build();

        // GTK 4 dropped `GtkOffscreenWindow`, and WebKitGTK 6.0
        // refuses to snapshot a hidden surface ("There was an error
        // creating the snapshot" — the renderer no-ops when the
        // window isn't mapped). Workaround: keep the window mapped
        // but visually invisible via `set_opacity(0.0)`. The
        // compositor allocates the surface so WebKit's GPU process
        // engages, but the window draws nothing on screen.
        // Additionally we set the WebView's `set_size_request` to
        // the requested capture dimensions while the host window
        // itself can stay tiny — `present()` will then resize the
        // window to fit the child (Wayland compositor permitting).
        //
        // A truly headless GTK 4 path (custom GtkRoot, direct
        // GdkSurface creation, etc.) is future work.
        let window = gtk::Window::new();
        window.set_decorated(false);
        window.set_default_size(config.size.width as i32, config.size.height as i32);
        webview.set_size_request(config.size.width as i32, config.size.height as i32);
        window.set_child(Some(&webview));
        window.set_opacity(0.0);
        window.present();

        let nav_state = Rc::new(RefCell::new(NavState::default()));
        install_load_signals(&webview, &nav_state);

        let web_messages: Rc<RefCell<VecDeque<String>>> =
            Rc::new(RefCell::new(VecDeque::new()));
        script_message::install(&ucm, &web_messages);

        Ok(Self {
            capabilities: super::linux_webkit6_capabilities(),
            webview,
            window,
            _context: context,
            _network_session: network_session,
            size: config.size,
            offset: config.offset,
            generation: Cell::new(0),
            nav_state,
            web_messages,
        })
    }

    pub(crate) fn next_generation(&self) -> u64 {
        let next = self.generation.get().saturating_add(1);
        self.generation.set(next);
        next
    }

    pub fn offset(&self) -> (f32, f32) {
        self.offset
    }

    pub fn size(&self) -> PhysicalSize<u32> {
        self.size
    }
}

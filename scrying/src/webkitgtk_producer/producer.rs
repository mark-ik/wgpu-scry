// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! [`WebKitGtkProducer`] struct, construction, and shared inherent helpers.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use dpi::PhysicalSize;
use gtk::prelude::*;
use webkit2gtk::{WebContext, WebView, WebViewExt, WebsiteDataManager};
// `webkit2gtk::gio::Cancellable::NONE` is reached through the
// `webkit2gtk` re-export in `run_input_js`; no separate import.

use crate::{CursorShape, UrlSchemeHandlerFn, WebSurfaceCapabilities, WebSurfaceError};

use super::config::WebKitGtkProducerConfig;
use super::cursor;
use super::downloads;
use super::helpers::ensure_gtk_init;
use super::ime;
use super::navigation::{NavState, install_load_signals};
use super::scheme_handler;
use super::script_message;

/// Linux WebKitGTK producer.
///
/// Hosts an offscreen `WebKitWebView` and emits CPU RGBA snapshots via
/// [`crate::WebSurfaceProducer::acquire_frame`]. See the
/// [module docs](super) for the broader architecture.
pub struct WebKitGtkProducer {
    pub(crate) capabilities: WebSurfaceCapabilities,
    pub(crate) webview: WebView,
    pub(crate) offscreen: gtk::OffscreenWindow,
    /// WebContext and data manager held for lifetime: the WebView's
    /// internal page references them and they must outlive it.
    pub(crate) _context: WebContext,
    pub(crate) _data_manager: WebsiteDataManager,
    pub(crate) size: PhysicalSize<u32>,
    pub(crate) offset: (f32, f32),
    pub(crate) generation: Cell<u64>,
    pub(crate) nav_state: Rc<RefCell<NavState>>,
    /// FIFO of page → host messages pushed by the
    /// `script-message-received::scry` signal handler and drained by
    /// [`crate::WebSurfaceProducer::poll_web_message`].
    pub(crate) web_messages: Rc<RefCell<VecDeque<String>>>,
    /// Latest cursor shape requested by WebKit's `mouse-target-changed`
    /// signal. Drained by [`crate::WebSurfaceProducer::poll_cursor_shape`].
    pub(crate) cursor_shape: Rc<RefCell<Option<CursorShape>>>,
}

impl WebKitGtkProducer {
    /// Construct the producer. Initializes GTK if needed (must be on
    /// the GTK main thread), creates a persistent `WebContext` rooted
    /// at `config.data_dir`, builds the offscreen WebView, and wires
    /// load-event signal handlers.
    pub fn new(config: WebKitGtkProducerConfig) -> Result<Self, WebSurfaceError> {
        Self::new_with_url_schemes(config, HashMap::new())
    }

    /// Construct the producer with custom URL scheme handlers
    /// registered against the producer's `WebContext` before the
    /// `WebView` is built — so the very first navigation can already
    /// resolve `myapp://...` URIs.
    pub fn new_with_url_schemes(
        config: WebKitGtkProducerConfig,
        url_schemes: HashMap<String, UrlSchemeHandlerFn>,
    ) -> Result<Self, WebSurfaceError> {
        ensure_gtk_init()?;

        if config.size.width == 0 || config.size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WebKitGTK producer size must be non-zero, got {}x{}",
                config.size.width, config.size.height
            )));
        }
        std::fs::create_dir_all(&config.data_dir).map_err(|err| {
            WebSurfaceError::Platform(format!(
                "could not create data_dir {:?}: {err}",
                config.data_dir
            ))
        })?;

        let data_dir_str = config.data_dir.to_string_lossy().to_string();
        let data_manager = WebsiteDataManager::builder()
            .base_data_directory(&data_dir_str)
            .base_cache_directory(&data_dir_str)
            .build();
        let context = WebContext::with_website_data_manager(&data_manager);
        if !url_schemes.is_empty() {
            scheme_handler::register_all(&context, url_schemes);
        }

        let nav_state = Rc::new(RefCell::new(NavState::default()));
        let downloads_dir = config.data_dir.join("downloads");
        let next_download_id = Rc::new(Cell::new(0u64));
        downloads::install(&context, downloads_dir, nav_state.clone(), next_download_id);

        let webview = WebView::with_context(&context);

        let offscreen = gtk::OffscreenWindow::new();
        offscreen.set_default_size(config.size.width as i32, config.size.height as i32);
        webview.set_size_request(config.size.width as i32, config.size.height as i32);
        offscreen.add(&webview);
        offscreen.show_all();

        install_load_signals(&webview, &nav_state);

        let web_messages: Rc<RefCell<VecDeque<String>>> = Rc::new(RefCell::new(VecDeque::new()));
        if let Some(ucm) = WebViewExt::user_content_manager(&webview) {
            script_message::install(&ucm, &web_messages);
            ime::install(&ucm, &nav_state);
        }

        let cursor_shape: Rc<RefCell<Option<CursorShape>>> = Rc::new(RefCell::new(None));
        cursor::install(&webview, &cursor_shape);

        Ok(Self {
            capabilities: super::linux_webkitgtk_capabilities(),
            webview,
            offscreen,
            _context: context,
            _data_manager: data_manager,
            size: config.size,
            offset: config.offset,
            generation: Cell::new(0),
            nav_state,
            web_messages,
            cursor_shape,
        })
    }

    /// Monotonic per-frame generation counter (mirrors the Windows /
    /// macOS producers' frame-id semantics).
    pub(crate) fn next_generation(&self) -> u64 {
        let next = self.generation.get().saturating_add(1);
        self.generation.set(next);
        next
    }

    /// Current configured offset, in DIPs (informational with the
    /// offscreen capture path).
    pub fn offset(&self) -> (f32, f32) {
        self.offset
    }

    /// Current configured render size, in physical pixels.
    pub fn size(&self) -> PhysicalSize<u32> {
        self.size
    }

    /// Fire-and-forget JS dispatch — used by the input-forwarding path
    /// to push synthesized DOM events at page handlers. No completion
    /// callback because input is one-way; the JS errors out silently if
    /// the page doesn't have a sensible target element.
    pub(crate) fn run_input_js(&self, js: &str) {
        self.webview.evaluate_javascript(
            js,
            None,
            None,
            webkit2gtk::gio::Cancellable::NONE,
            |_| {},
        );
    }
}

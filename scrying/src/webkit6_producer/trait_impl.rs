// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! [`WebSurfaceProducer`] trait implementation for [`WebKit6Producer`].

use std::time::Duration;

use dpi::PhysicalSize;
use webkit6::gtk::prelude::*;
use webkit6::prelude::*;

use crate::{
    CursorShape, DragInput, KeyboardInput, MouseInput, NavigationEvent, PointerInput,
    WebSurfaceCapabilities, WebSurfaceError, WebSurfaceFrame, WebSurfaceMode, WebSurfaceProducer,
};

use super::input;
use super::navigation::{arm_navigation, wait_for_load};
use super::producer::WebKit6Producer;
use super::script_message;

impl WebSurfaceProducer for WebKit6Producer {
    fn capabilities(&self) -> WebSurfaceCapabilities {
        self.capabilities.clone()
    }

    fn mode(&self) -> WebSurfaceMode {
        self.capabilities.preferred_mode
    }

    fn acquire_frame(&mut self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        // Phase A.8: try the paintable-render path first (which probes
        // for `GdkDmabufTexture` and uses the format-aware downloader);
        // fall back to the legacy `webkit_web_view_get_snapshot` path
        // inside `capture_native` itself if the paintable-render path
        // can't fire (e.g., the window hasn't mapped yet).
        self.capture_native()
    }

    fn load_html(&mut self, html: &str) -> Result<(), WebSurfaceError> {
        WebKit6Producer::load_html(self, html, None);
        Ok(())
    }

    fn load_url(&mut self, url: &str) -> Result<(), WebSurfaceError> {
        WebKit6Producer::load_uri(self, url);
        Ok(())
    }

    fn navigate_to_string(&mut self, html: &str, timeout: Duration) -> Result<(), WebSurfaceError> {
        arm_navigation(&self.nav_state);
        self.webview.load_html(html, None);
        wait_for_load(&self.nav_state, timeout)
    }

    fn navigate_to_url(&mut self, url: &str, timeout: Duration) -> Result<(), WebSurfaceError> {
        arm_navigation(&self.nav_state);
        self.webview.load_uri(url);
        wait_for_load(&self.nav_state, timeout)
    }

    fn set_cookie(&mut self, cookie: &crate::Cookie) -> Result<(), WebSurfaceError> {
        WebKit6Producer::set_cookie(self, cookie)
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), WebSurfaceError> {
        if size.width == 0 || size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WebKit6 producer size must be non-zero, got {}x{}",
                size.width, size.height
            )));
        }
        self.size = size;
        self.window
            .set_default_size(size.width as i32, size.height as i32);
        self.webview
            .set_size_request(size.width as i32, size.height as i32);
        Ok(())
    }

    fn set_offset(&mut self, x: f32, y: f32) -> Result<(), WebSurfaceError> {
        self.offset = (x, y);
        Ok(())
    }

    fn capture_snapshot_png(&mut self) -> Result<Vec<u8>, WebSurfaceError> {
        WebKit6Producer::capture_snapshot_png(self)
    }

    fn poll_navigation_event(&mut self) -> Option<NavigationEvent> {
        self.nav_state.borrow_mut().events.pop_front()
    }

    fn post_web_message(&mut self, message: &str) -> Result<(), WebSurfaceError> {
        let js = format!(
            "if (window.chrome && window.chrome.webview && window.chrome.webview.__scryDispatch) {{ \
                 window.chrome.webview.__scryDispatch({}); \
             }}",
            script_message::escape_for_js(message)
        );
        // `evaluate_javascript` (WebKitGTK 2.40+; gated behind webkit6's
        // `v2_44` feature, which we have enabled). Default world, no
        // source-URI tagging — this is host-driven dispatch, not page
        // code. Cancellable=NONE / fire-and-forget callback mirrors the
        // GTK 3 precedent: pages without listeners are not an error.
        self.webview
            .evaluate_javascript(&js, None, None, webkit6::gio::Cancellable::NONE, |_| {});
        Ok(())
    }

    fn poll_web_message(&mut self) -> Option<String> {
        self.web_messages.borrow_mut().pop_front()
    }

    fn poll_cursor_shape(&mut self) -> Option<CursorShape> {
        self.cursor_shape.borrow_mut().take()
    }

    fn send_mouse_input(&mut self, event: MouseInput) -> Result<(), WebSurfaceError> {
        // JS-event-synthesis only. GTK 4 removed `gtk_main_do_event`,
        // so the GTK 3 producer's native `GdkEvent`-dispatch primary
        // (which closes the `isTrusted` gap) has no analog here. See
        // [`super::input`] module doc for the full caveat.
        self.run_input_js(&input::mouse_event_js(event));
        Ok(())
    }

    fn send_pointer_input(&mut self, event: PointerInput) -> Result<(), WebSurfaceError> {
        self.run_input_js(&input::pointer_event_js(event));
        Ok(())
    }

    fn send_keyboard_input(&mut self, event: KeyboardInput) -> Result<(), WebSurfaceError> {
        let js = input::keyboard_event_js(&event);
        if !js.is_empty() {
            self.run_input_js(&js);
        }
        Ok(())
    }

    fn send_drag_input(&mut self, event: DragInput) -> Result<(), WebSurfaceError> {
        // JS-event synthesis only. Pages whose drop handlers read
        // `event.dataTransfer.files` see an empty list; coordinate /
        // type discrimination still works. Mirrors the GTK 3 producer
        // (which also routes drag through JS synthesis — its native
        // path needs a `GdkDragContext` from a real drag source).
        self.run_input_js(&input::drag_event_js(event));
        Ok(())
    }
}

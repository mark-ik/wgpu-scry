//! [`WebSurfaceProducer`] trait implementation for [`WebKit6Producer`].

use std::time::Duration;

use dpi::PhysicalSize;
use webkit6::gtk::prelude::*;
use webkit6::prelude::*;

use crate::{
    NavigationEvent, WebSurfaceCapabilities, WebSurfaceError, WebSurfaceFrame, WebSurfaceMode,
    WebSurfaceProducer,
};

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
        self.capture_cpu_snapshot()
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
        self.webview.evaluate_javascript(
            &js,
            None,
            None,
            webkit6::gio::Cancellable::NONE,
            |_| {},
        );
        Ok(())
    }

    fn poll_web_message(&mut self) -> Option<String> {
        self.web_messages.borrow_mut().pop_front()
    }
}

//! Navigation: `load_uri` / `load_html` with main-loop-pumped completion waits.
//!
//! Same shape as the GTK 3 producer's
//! [`crate::webkitgtk_producer::navigation`]. Signal names + payloads
//! line up across the two WebKitGTK lines.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use webkit6::prelude::*;
use webkit6::{LoadEvent, WebView};

use crate::{NavigationEvent, WebSurfaceError};

use super::helpers::pump_until;
use super::producer::WebKit6Producer;

#[derive(Default)]
pub(crate) struct NavState {
    pub committed_uri: Option<String>,
    pub finished: bool,
    pub failed: Option<String>,
    pub events: VecDeque<NavigationEvent>,
}

pub(crate) fn install_load_signals(webview: &WebView, state: &Rc<RefCell<NavState>>) {
    let s = state.clone();
    webview.connect_load_changed(move |view, event| {
        let mut st = s.borrow_mut();
        let url = view.uri().map(|g| g.to_string()).unwrap_or_default();
        match event {
            LoadEvent::Started => {
                st.finished = false;
                st.failed = None;
                st.events.push_back(NavigationEvent::Starting { url });
            }
            LoadEvent::Committed => {
                st.committed_uri = Some(url.clone());
                st.events.push_back(NavigationEvent::SourceChanged { url });
            }
            LoadEvent::Finished => {
                st.finished = true;
                st.events
                    .push_back(NavigationEvent::Completed { url, success: true });
            }
            _ => {}
        }
    });

    let s = state.clone();
    webview.connect_load_failed(move |view, _event, failing_uri, error| {
        let mut st = s.borrow_mut();
        st.failed = Some(error.message().to_string());
        st.finished = true;
        let url = if failing_uri.is_empty() {
            view.uri().map(|g| g.to_string()).unwrap_or_default()
        } else {
            failing_uri.to_string()
        };
        st.events.push_back(NavigationEvent::Completed {
            url,
            success: false,
        });
        false
    });

    let s = state.clone();
    webview.connect_title_notify(move |view| {
        if let Some(title) = view.title() {
            s.borrow_mut()
                .events
                .push_back(NavigationEvent::TitleChanged {
                    title: title.to_string(),
                });
        }
    });
}

pub(crate) fn wait_for_load(
    state: &Rc<RefCell<NavState>>,
    timeout: Duration,
) -> Result<(), WebSurfaceError> {
    let deadline = Instant::now() + timeout;
    pump_until(deadline, || state.borrow().finished)?;
    let st = state.borrow();
    if let Some(err) = &st.failed {
        return Err(WebSurfaceError::Platform(format!(
            "WebKitGTK 6 load failed: {err}"
        )));
    }
    Ok(())
}

pub(crate) fn arm_navigation(state: &Rc<RefCell<NavState>>) {
    let mut st = state.borrow_mut();
    st.finished = false;
    st.failed = None;
}

impl WebKit6Producer {
    pub fn load_html(&self, html: &str, base_uri: Option<&str>) {
        arm_navigation(&self.nav_state);
        self.webview.load_html(html, base_uri);
    }

    pub fn load_uri(&self, uri: &str) {
        arm_navigation(&self.nav_state);
        self.webview.load_uri(uri);
    }

    pub fn wait_for_load(&self, timeout: Duration) -> Result<(), WebSurfaceError> {
        wait_for_load(&self.nav_state, timeout)
    }

    pub fn committed_uri(&self) -> Option<String> {
        self.nav_state.borrow().committed_uri.clone()
    }

    /// Pump the GTK 4 main loop until a [`NavigationEvent`] matching
    /// `predicate` arrives or `timeout` elapses. Drains non-matching
    /// events along the way (they're dropped), so callers should
    /// register interest before driving whatever action would
    /// trigger the wait.
    ///
    /// Mirrors `WebKitGtkProducer::wait_for_navigation_event` — same
    /// shape as the GTK 3 precedent, retargeted to GTK 4's
    /// `glib::MainContext::iteration(false)` pump.
    pub fn wait_for_navigation_event<F: Fn(&NavigationEvent) -> bool>(
        &self,
        timeout: Duration,
        predicate: F,
    ) -> Option<NavigationEvent> {
        let ctx = webkit6::glib::MainContext::default();
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(event) = self.nav_state.borrow_mut().events.pop_front() {
                if predicate(&event) {
                    return Some(event);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            ctx.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

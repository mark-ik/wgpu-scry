// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Navigation: `load_uri` / `load_html` with main-loop-pumped completion waits.
//!
//! Mirrors the macOS WKWebView producer's blocking + non-blocking pair:
//! the trait method (`navigate_to_string` / `navigate_to_url`) pumps the
//! main loop until `load-changed → Finished` or `load-failed`. Hosts that
//! call from an event-loop callback can use the non-blocking inherent
//! `load_uri` / `load_html` and poll `wait_for_load` (or a future
//! `poll_navigation_event`) instead.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use webkit2gtk::{LoadEvent, URIRequestExt, WebView, WebViewExt};

use crate::{NavigationEvent, WebSurfaceError};

use super::helpers::pump_until;
use super::producer::WebKitGtkProducer;

/// Shared signal-handler state — mutated from `load-changed` /
/// `load-failed` / `notify::title` callbacks on the GTK main thread.
#[derive(Default)]
pub(crate) struct NavState {
    /// URI of the most recently committed top-level navigation.
    pub committed_uri: Option<String>,
    /// `LoadEvent::Finished` (or a `load-failed` callback) has fired
    /// since the last `wait_for_load`.
    pub finished: bool,
    /// Last `load-failed` error message, if any.
    pub failed: Option<String>,
    /// FIFO queue of [`NavigationEvent`]s drained by
    /// [`WebKitGtkProducer::poll_navigation_event`]. Filled by the
    /// `load-changed` / `load-failed` / `notify::title` signal
    /// handlers.
    pub events: VecDeque<NavigationEvent>,
}

/// Wire `load-changed`, `load-failed`, and `notify::title` signal
/// handlers to the shared [`NavState`]. The handlers run on the GTK
/// main thread.
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
            // LoadEvent::Redirected fires between Started and
            // Committed — useful for analytics but the producer's
            // public NavigationEvent surface doesn't have a
            // dedicated variant for it, so skip.
            _ => {}
        }
    });
    let s = state.clone();
    webview.connect_load_failed(move |view, _event, failing_uri, error| {
        let mut st = s.borrow_mut();
        st.failed = Some(error.message().to_string());
        // Unblock anyone waiting on `finished` — the trait-level
        // navigate methods turn the captured `failed` into an error
        // result.
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
        // Returning false lets WebKit show its default error page.
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

    // Popup intercept: `window.open(...)`, `target="_blank"` clicks,
    // JS-triggered popups. Return `None` to suppress the engine-level
    // popup unconditionally — browser-shape consumers observe
    // `NavigationEvent::NewWindowRequested` and decide whether to
    // open a new tab, ignore, etc.
    let s = state.clone();
    webview.connect_create(move |_view, nav_action| {
        let url = nav_action
            .request()
            .and_then(|r| r.uri())
            .map(|g| g.to_string())
            .unwrap_or_default();
        s.borrow_mut()
            .events
            .push_back(NavigationEvent::NewWindowRequested { url });
        None
    });

    // Content-process termination (crash). The producer's WebView
    // becomes non-rendering; the host should reload or load a fresh
    // URL. We don't suppress the engine's default behaviour (which
    // is to show its crash page) — just surface the event.
    let s = state.clone();
    webview.connect_web_process_terminated(move |_view, _reason| {
        s.borrow_mut()
            .events
            .push_back(NavigationEvent::ContentProcessTerminated);
    });
}

/// Block (pumping the GTK main loop) until the most recent navigation
/// reaches `LoadEvent::Finished` or `load-failed`, or `timeout` elapses.
pub(crate) fn wait_for_load(
    state: &Rc<RefCell<NavState>>,
    timeout: Duration,
) -> Result<(), WebSurfaceError> {
    let deadline = Instant::now() + timeout;
    pump_until(deadline, || state.borrow().finished)?;
    let st = state.borrow();
    if let Some(err) = &st.failed {
        return Err(WebSurfaceError::Platform(format!(
            "WebKitGTK load failed: {err}"
        )));
    }
    Ok(())
}

/// Reset the load-completion flag so the next `wait_for_load` waits for
/// a fresh navigation rather than returning instantly from a stale
/// `finished = true`.
pub(crate) fn arm_navigation(state: &Rc<RefCell<NavState>>) {
    let mut st = state.borrow_mut();
    st.finished = false;
    st.failed = None;
}

impl WebKitGtkProducer {
    /// Non-blocking: kick off an HTML-string load. Hosts can poll
    /// [`Self::wait_for_load`] to know when it completes.
    pub fn load_html(&self, html: &str, base_uri: Option<&str>) {
        arm_navigation(&self.nav_state);
        self.webview.load_html(html, base_uri);
    }

    /// Non-blocking: kick off a URI load.
    pub fn load_uri(&self, uri: &str) {
        arm_navigation(&self.nav_state);
        self.webview.load_uri(uri);
    }

    /// Block (pumping the GTK main loop) until the most recent load
    /// completes or `timeout` elapses.
    pub fn wait_for_load(&self, timeout: Duration) -> Result<(), WebSurfaceError> {
        wait_for_load(&self.nav_state, timeout)
    }

    /// URI of the most recently committed navigation, if any.
    pub fn committed_uri(&self) -> Option<String> {
        self.nav_state.borrow().committed_uri.clone()
    }

    /// Pump the GTK main loop until a [`NavigationEvent`] matching
    /// `predicate` arrives or `timeout` elapses. Drains non-matching
    /// events along the way (they're dropped), so callers should
    /// register interest before driving whatever action would
    /// trigger the wait.
    pub fn wait_for_navigation_event<F: Fn(&NavigationEvent) -> bool>(
        &self,
        timeout: std::time::Duration,
        predicate: F,
    ) -> Option<NavigationEvent> {
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
            gtk::main_iteration_do(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

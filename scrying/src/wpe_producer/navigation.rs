// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Navigation: `load_uri` / `load_html` with main-loop-pumped completion
//! waits. Direct port of `webkitgtk_producer/navigation.rs` — same
//! `WebKitLoadEvent` enum + `load-changed`/`load-failed` signals on the
//! WebKitWebView. Single-threaded model A: state is `Rc<RefCell<NavState>>`
//! mutated from glib signal closures on the construction thread.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{NavigationEvent, WebSurfaceError, WebSurfaceEvent};

use super::producer::WebSurfaceEventQueue;

/// `WebKitLoadEvent` from `/usr/include/wpe-webkit-2.0/wpe/WebKitWebView.h`
/// (the int values land in the glib closure as `i32`).
pub(super) const WEBKIT_LOAD_STARTED: i32 = 0;
/// Redirected fires between Started and Committed; scrying doesn't surface
/// a NavigationEvent for it, but the constant is exercised by a unit test
/// asserting the no-op match arm. Kept named for parity with the C header.
#[allow(dead_code)]
pub(super) const WEBKIT_LOAD_REDIRECTED: i32 = 1;
pub(super) const WEBKIT_LOAD_COMMITTED: i32 = 2;
pub(super) const WEBKIT_LOAD_FINISHED: i32 = 3;

#[derive(Default)]
pub(super) struct NavState {
    pub committed_uri: Option<String>,
    pub finished: bool,
    pub failed: Option<String>,
    pub events: VecDeque<NavigationEvent>,
}

impl NavState {
    /// Apply a `load-changed` event. `uri` is the WebKitWebView's current URI
    /// at the time the signal fired (may be empty for inline HTML).
    pub fn on_load_changed(&mut self, event: i32, uri: String) -> Option<NavigationEvent> {
        let navigation_event = match event {
            WEBKIT_LOAD_STARTED => {
                self.finished = false;
                self.failed = None;
                Some(NavigationEvent::Starting { url: uri })
            }
            WEBKIT_LOAD_COMMITTED => {
                self.committed_uri = Some(uri.clone());
                Some(NavigationEvent::SourceChanged { url: uri })
            }
            WEBKIT_LOAD_FINISHED => {
                self.finished = true;
                Some(NavigationEvent::Completed {
                    url: uri,
                    success: true,
                })
            }
            // Redirected fires between Started and Committed — no scrying
            // variant for it, so skip (matches GTK).
            _ => None,
        };
        if let Some(event) = navigation_event.as_ref() {
            self.events.push_back(event.clone());
        }
        navigation_event
    }

    /// Apply a `load-failed` event. `error` is the GError message.
    pub fn on_load_failed(&mut self, uri: String, error: String) -> NavigationEvent {
        self.failed = Some(error);
        // Unblock waiters; trait-level navigate methods inspect `failed`.
        self.finished = true;
        let event = NavigationEvent::Completed {
            url: uri,
            success: false,
        };
        self.events.push_back(event.clone());
        event
    }
}

/// Clear pre-load state before a fresh load. Leaves `events` intact — they're
/// drained by `poll_navigation_event` on the producer's own schedule.
pub(super) fn arm_navigation(state: &Rc<RefCell<NavState>>) {
    let mut s = state.borrow_mut();
    s.finished = false;
    s.failed = None;
    s.committed_uri = None;
}

use glib::prelude::*;

/// Connect `load-changed` and `load-failed` on the given WebKitWebView,
/// routing into `state`. The closures run on the producer's main context
/// thread (model A). They capture only `Rc<RefCell<NavState>>` clones — no
/// `&mut self`-like borrows survive into the closure body.
///
/// Implementation note: `WebKitLoadEvent` arrives in the closure as the
/// GObject enum type, NOT as `gint`, so `glib::closure_local!(... event:
/// i32 ...)` fails its arg-type check. We hand-roll a `RustClosure` over
/// `&[glib::Value]` and pull the enum out via `value.get::<i32>()` —
/// glib's value transform tables include enum→gint, which works here.
pub(super) fn install_load_signals(
    webview: &glib::Object,
    state: &Rc<RefCell<NavState>>,
    ordered_events: WebSurfaceEventQueue,
) {
    {
        let s = state.clone();
        let ordered = ordered_events.clone();
        let closure = glib::RustClosure::new_local(move |values| {
            // values: [view: GObject, event: WebKitLoadEvent]
            let view = values[0]
                .get::<glib::Object>()
                .expect("load-changed: arg 0 must be GObject");
            let event = values[1].get::<i32>().unwrap_or_else(|_| {
                // Fallback: read raw enum value through transform.
                values[1]
                    .transform::<i32>()
                    .ok()
                    .and_then(|v| v.get::<i32>().ok())
                    .unwrap_or(-1)
            });
            let uri = view.property::<String>("uri");
            if let Some(event) = s.borrow_mut().on_load_changed(event, uri) {
                ordered
                    .borrow_mut()
                    .push_back(WebSurfaceEvent::Navigation(event));
            }
            None
        });
        webview.connect_closure("load-changed", false, closure);
    }
    {
        let s = state.clone();
        let ordered = ordered_events;
        let closure = glib::RustClosure::new_local(move |values| {
            // values: [view, event, failing_uri: &str, error: GError]
            let failing_uri = values
                .get(2)
                .and_then(|v| v.get::<String>().ok())
                .unwrap_or_default();
            let err_msg = values
                .get(3)
                .and_then(|v| v.get::<glib::Error>().ok())
                .map(|e| e.message().to_string())
                .unwrap_or_else(|| "(error message unavailable)".to_string());
            let event = s.borrow_mut().on_load_failed(failing_uri, err_msg);
            ordered
                .borrow_mut()
                .push_back(WebSurfaceEvent::Navigation(event));
            // load-failed expects a gboolean return (FALSE = let WebKit
            // show its default error page).
            Some(false.to_value())
        });
        webview.connect_closure("load-failed", false, closure);
    }
}

/// Pump the producer's `MainContext` until `state.finished`, returning an
/// error wrapping `state.failed` if the load failed.
pub(super) fn wait_for_load(
    ctx: &glib::MainContext,
    state: &Rc<RefCell<NavState>>,
    timeout: Duration,
) -> Result<(), WebSurfaceError> {
    let deadline = Instant::now() + timeout;
    super::headless::pump_until(ctx, deadline, || state.borrow().finished)?;
    if let Some(msg) = state.borrow().failed.clone() {
        return Err(WebSurfaceError::Platform(format!(
            "navigation failed: {msg}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NavigationEvent, WebSurfaceEvent};

    fn st() -> Rc<RefCell<NavState>> {
        Rc::new(RefCell::new(NavState::default()))
    }

    #[test]
    fn started_clears_flags_and_emits_starting() {
        let s = st();
        s.borrow_mut().finished = true;
        s.borrow_mut().failed = Some("old".into());
        let _ = s
            .borrow_mut()
            .on_load_changed(WEBKIT_LOAD_STARTED, "https://x".into());
        let st = s.borrow();
        assert!(!st.finished);
        assert!(st.failed.is_none());
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Starting { url }) if url == "https://x"));
    }

    #[test]
    fn committed_stores_uri_and_emits_source_changed() {
        let s = st();
        let _ = s
            .borrow_mut()
            .on_load_changed(WEBKIT_LOAD_COMMITTED, "https://y".into());
        let st = s.borrow();
        assert_eq!(st.committed_uri.as_deref(), Some("https://y"));
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::SourceChanged { url }) if url == "https://y"));
    }

    #[test]
    fn finished_sets_flag_and_emits_completed_success() {
        let s = st();
        let _ = s
            .borrow_mut()
            .on_load_changed(WEBKIT_LOAD_FINISHED, "https://z".into());
        let st = s.borrow();
        assert!(st.finished);
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Completed { url, success: true }) if url == "https://z"));
    }

    #[test]
    fn redirected_is_a_no_op_on_events() {
        let s = st();
        let _ = s
            .borrow_mut()
            .on_load_changed(WEBKIT_LOAD_REDIRECTED, "https://w".into());
        assert!(s.borrow().events.is_empty());
    }

    #[test]
    fn load_failed_sets_failed_and_finished_and_emits_completed_failure() {
        let s = st();
        let _ = s
            .borrow_mut()
            .on_load_failed("https://q".into(), "boom".into());
        let st = s.borrow();
        assert_eq!(st.failed.as_deref(), Some("boom"));
        assert!(st.finished);
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Completed { url, success: false }) if url == "https://q"));
    }

    #[test]
    fn arm_clears_flags_keeps_events() {
        let s = st();
        let _ = s
            .borrow_mut()
            .on_load_changed(WEBKIT_LOAD_FINISHED, "x".into());
        arm_navigation(&s);
        let st = s.borrow();
        assert!(!st.finished);
        assert!(st.failed.is_none());
        assert!(st.committed_uri.is_none());
        assert_eq!(
            st.events.len(),
            1,
            "events queue is drained by poll, not by arm"
        );
    }

    #[test]
    fn navigation_callback_value_can_be_mirrored_to_ordered_stream() {
        let state = st();
        let ordered = Rc::new(RefCell::new(VecDeque::new()));
        let event = state
            .borrow_mut()
            .on_load_changed(WEBKIT_LOAD_COMMITTED, "https://ordered.test".into())
            .expect("committed event");
        ordered
            .borrow_mut()
            .push_back(WebSurfaceEvent::Navigation(event));

        assert!(matches!(
            state.borrow_mut().events.pop_front(),
            Some(NavigationEvent::SourceChanged { url }) if url == "https://ordered.test"
        ));
        assert!(matches!(
            ordered.borrow_mut().pop_front(),
            Some(WebSurfaceEvent::Navigation(NavigationEvent::SourceChanged { url }))
                if url == "https://ordered.test"
        ));
    }
}

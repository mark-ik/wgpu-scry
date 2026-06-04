//! Navigation: `load_uri` / `load_html` with main-loop-pumped completion
//! waits. Direct port of `webkitgtk_producer/navigation.rs` — same
//! `WebKitLoadEvent` enum + `load-changed`/`load-failed` signals on the
//! WebKitWebView. Single-threaded model A: state is `Rc<RefCell<NavState>>`
//! mutated from glib signal closures on the construction thread.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{NavigationEvent, WebSurfaceError};

/// `WebKitLoadEvent` from `/usr/include/wpe-webkit-2.0/wpe/WebKitWebView.h`
/// (the int values land in the glib closure as `i32`).
pub(super) const WEBKIT_LOAD_STARTED: i32 = 0;
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
    pub fn on_load_changed(&mut self, event: i32, uri: String) {
        match event {
            WEBKIT_LOAD_STARTED => {
                self.finished = false;
                self.failed = None;
                self.events.push_back(NavigationEvent::Starting { url: uri });
            }
            WEBKIT_LOAD_COMMITTED => {
                self.committed_uri = Some(uri.clone());
                self.events.push_back(NavigationEvent::SourceChanged { url: uri });
            }
            WEBKIT_LOAD_FINISHED => {
                self.finished = true;
                self.events.push_back(NavigationEvent::Completed { url: uri, success: true });
            }
            // Redirected fires between Started and Committed — no scrying
            // variant for it, so skip (matches GTK).
            _ => {}
        }
    }

    /// Apply a `load-failed` event. `error` is the GError message.
    pub fn on_load_failed(&mut self, uri: String, error: String) {
        self.failed = Some(error);
        // Unblock waiters; trait-level navigate methods inspect `failed`.
        self.finished = true;
        self.events.push_back(NavigationEvent::Completed { url: uri, success: false });
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
pub(super) fn install_load_signals(webview: &glib::Object, state: &Rc<RefCell<NavState>>) {
    {
        let s = state.clone();
        webview.connect_closure(
            "load-changed",
            false,
            glib::closure_local!(move |view: glib::Object, event: i32| {
                let uri = view
                    .property::<String>("uri");
                s.borrow_mut().on_load_changed(event, uri);
            }),
        );
    }
    {
        let s = state.clone();
        webview.connect_closure(
            "load-failed",
            false,
            glib::closure_local!(move |_view: glib::Object, _event: i32, failing_uri: String, error: glib::Error| {
                s.borrow_mut().on_load_failed(failing_uri, error.message().to_string());
            }),
        );
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
        return Err(WebSurfaceError::Platform(format!("navigation failed: {msg}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NavigationEvent;

    fn st() -> Rc<RefCell<NavState>> { Rc::new(RefCell::new(NavState::default())) }

    #[test]
    fn started_clears_flags_and_emits_starting() {
        let s = st();
        s.borrow_mut().finished = true;
        s.borrow_mut().failed = Some("old".into());
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_STARTED, "https://x".into());
        let st = s.borrow();
        assert!(!st.finished);
        assert!(st.failed.is_none());
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Starting { url }) if url == "https://x"));
    }

    #[test]
    fn committed_stores_uri_and_emits_source_changed() {
        let s = st();
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_COMMITTED, "https://y".into());
        let st = s.borrow();
        assert_eq!(st.committed_uri.as_deref(), Some("https://y"));
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::SourceChanged { url }) if url == "https://y"));
    }

    #[test]
    fn finished_sets_flag_and_emits_completed_success() {
        let s = st();
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_FINISHED, "https://z".into());
        let st = s.borrow();
        assert!(st.finished);
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Completed { url, success: true }) if url == "https://z"));
    }

    #[test]
    fn redirected_is_a_no_op_on_events() {
        let s = st();
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_REDIRECTED, "https://w".into());
        assert!(s.borrow().events.is_empty());
    }

    #[test]
    fn load_failed_sets_failed_and_finished_and_emits_completed_failure() {
        let s = st();
        s.borrow_mut().on_load_failed("https://q".into(), "boom".into());
        let st = s.borrow();
        assert_eq!(st.failed.as_deref(), Some("boom"));
        assert!(st.finished);
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Completed { url, success: false }) if url == "https://q"));
    }

    #[test]
    fn arm_clears_flags_keeps_events() {
        let s = st();
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_FINISHED, "x".into());
        arm_navigation(&s);
        let st = s.borrow();
        assert!(!st.finished);
        assert!(st.failed.is_none());
        assert!(st.committed_uri.is_none());
        assert_eq!(st.events.len(), 1, "events queue is drained by poll, not by arm");
    }
}

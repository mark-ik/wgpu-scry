// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! JS messaging via `WebKitUserContentManager`.
//!
//! Page → host: pages call
//! `window.webkit.messageHandlers.scry.postMessage(msg)` — the
//! registered `"scry"` handler delivers the message to a host-side
//! FIFO drained by [`crate::WebSurfaceProducer::poll_web_message`].
//!
//! Host → page: [`crate::WebSurfaceProducer::post_web_message`] runs
//! JavaScript that invokes `window.chrome.webview.__scryDispatch(msg)`.
//! The producer injects a small `window.chrome.webview` shim at
//! document-start so pages can use the same
//! `addEventListener('message', cb)` / `postMessage(msg)` API surface
//! the Windows + macOS producers expose (mirrors WebView2's
//! `window.chrome.webview` convention).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use javascriptcore::ValueExt;
use webkit2gtk::{
    UserContentInjectedFrames, UserContentManager, UserContentManagerExt, UserScript,
    UserScriptInjectionTime,
};

use super::producer::WebKitGtkProducer;

/// Handler name registered with `register_script_message_handler` and
/// used on the page side as
/// `window.webkit.messageHandlers.scry.postMessage(...)`.
pub(crate) const SCRY_HANDLER_NAME: &str = "scry";

const CHROME_WEBVIEW_SHIM: &str = r#"
(function() {
    if (window.chrome && window.chrome.webview && window.chrome.webview.__scryInstalled) {
        return;
    }
    var listeners = new Set();
    window.chrome = window.chrome || {};
    window.chrome.webview = {
        __scryInstalled: true,
        postMessage: function(msg) {
            window.webkit.messageHandlers.scry.postMessage(String(msg));
        },
        addEventListener: function(type, cb) {
            if (type === 'message') { listeners.add(cb); }
        },
        removeEventListener: function(type, cb) {
            if (type === 'message') { listeners.delete(cb); }
        },
        __scryDispatch: function(data) {
            listeners.forEach(function(cb) {
                try { cb({ data: data }); } catch (e) {}
            });
        }
    };
})();
"#;

/// Register the `scry` script-message handler, inject the
/// `window.chrome.webview` shim, and wire the signal handler that
/// pushes incoming page messages onto `queue`.
pub(crate) fn install(ucm: &UserContentManager, queue: &Rc<RefCell<VecDeque<String>>>) {
    // `register_script_message_handler` returns false only on a
    // name collision. Producers create their own UCM (one per
    // WebView), so collisions can't happen in practice.
    let _ = ucm.register_script_message_handler(SCRY_HANDLER_NAME);

    let script = UserScript::new(
        CHROME_WEBVIEW_SHIM,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    ucm.add_script(&script);

    let q = queue.clone();
    ucm.connect_script_message_received(Some(SCRY_HANDLER_NAME), move |_ucm, result| {
        if let Some(value) = result.js_value() {
            q.borrow_mut().push_back(value.to_str().to_string());
        }
    });
}

impl WebKitGtkProducer {
    /// Pump the GTK main loop until a page → host message arrives or
    /// `timeout` elapses. Returns `None` on timeout.
    ///
    /// Useful for hosts that don't drive their own GTK main loop but
    /// need to block briefly waiting for a JS-side `postMessage` to
    /// land — runtime smokes, scripted tests, request/response
    /// patterns. Non-blocking callers should use
    /// [`crate::WebSurfaceProducer::poll_web_message`].
    pub fn wait_for_web_message(&self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(msg) = self.web_messages.borrow_mut().pop_front() {
                return Some(msg);
            }
            if Instant::now() >= deadline {
                return None;
            }
            gtk::main_iteration_do(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Escape a string for embedding inside a double-quoted JS literal.
/// Same escape rules as JSON strings — sufficient for the
/// `__scryDispatch(...)` payload `post_web_message` builds.
pub(crate) fn escape_for_js(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

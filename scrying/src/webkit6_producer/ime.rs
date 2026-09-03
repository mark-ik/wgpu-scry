// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! JS-side observability for IME state on the WebKitGTK 6.0 producer.
//!
//! Port of [`crate::webkitgtk_producer::ime`] adapted to the
//! `webkit6 = "0.6"` binding set (same shape as
//! [`super::script_message`]: high-level `UserContentManager` API, no
//! hand-rolled FFI). Registers a dedicated `scryIme` script-message
//! handler on the producer's `UserContentManager` (the same UCM the
//! `scry` bridge from A.1 uses — both handlers live side-by-side) and
//! injects a user script that watches `focusin` / `focusout` /
//! `input` / `selectionchange` on editable elements and posts a
//! pipe-delimited state payload. Each payload becomes a
//! [`crate::NavigationEvent::TextInputFocused`] /
//! [`TextInputChanged`] / [`TextInputBlurred`] on the producer's
//! [`super::navigation::NavState`] event queue, drained by
//! `poll_navigation_event`.

use std::cell::RefCell;
use std::rc::Rc;

use webkit6::{
    UserContentInjectedFrames, UserContentManager, UserScript, UserScriptInjectionTime,
};

use crate::{NavigationEvent, TextInputRect, TextInputState};

use super::navigation::NavState;

const IME_HANDLER_NAME: &str = "scryIme";

const IME_USER_SCRIPT: &str = r#"
(function() {
    if (window.__scryImeInstalled) return;
    window.__scryImeInstalled = true;

    function isEditable(el) {
        if (!el) return false;
        var tag = el.tagName;
        return tag === 'INPUT' || tag === 'TEXTAREA' || el.isContentEditable;
    }

    function reportFocus(el, kind /* 'focus' | 'change' */) {
        var tag = el.tagName ? el.tagName.toLowerCase() : '';
        var inputType = (el.type || '').toLowerCase();
        var inputMode = el.getAttribute ? (el.getAttribute('inputmode') || '') : '';
        var autocomplete = el.getAttribute ? (el.getAttribute('autocomplete') || '') : '';
        var isPassword = inputType === 'password';
        var isMultiline = tag === 'textarea' || (el.isContentEditable && true);
        var selStart = (typeof el.selectionStart === 'number') ? el.selectionStart : 0;
        var selEnd = (typeof el.selectionEnd === 'number') ? el.selectionEnd : 0;
        var rect = el.getBoundingClientRect ? el.getBoundingClientRect() : { left:0, top:0, width:0, height:0 };
        // Pipe-delimited payload: easier to parse host-side than
        // JSON without pulling a JSON dep into scrying.
        var payload = [
            kind,
            tag,
            inputType,
            inputMode,
            autocomplete,
            isPassword ? '1' : '0',
            isMultiline ? '1' : '0',
            String(selStart | 0),
            String(selEnd | 0),
            String(rect.left | 0),
            String(rect.top | 0),
            String(rect.width | 0),
            String(rect.height | 0),
        ].join('|');
        window.webkit.messageHandlers.scryIme.postMessage(payload);
    }

    document.addEventListener('focusin', function(e) {
        if (isEditable(e.target)) reportFocus(e.target, 'focus');
    }, true);

    document.addEventListener('focusout', function(e) {
        if (isEditable(e.target)) {
            window.webkit.messageHandlers.scryIme.postMessage('blur');
        }
    }, true);

    document.addEventListener('input', function(e) {
        if (isEditable(e.target)) reportFocus(e.target, 'change');
    }, true);

    document.addEventListener('selectionchange', function() {
        var el = document.activeElement;
        if (isEditable(el)) reportFocus(el, 'change');
    });
})();
"#;

/// Register the `scryIme` script-message handler on `ucm`, inject the
/// IME observer user script, and wire the signal handler that
/// translates incoming pipe-delimited payloads into `NavigationEvent`s
/// pushed onto `state.events`.
///
/// Mirrors [`super::script_message::install`]'s structure; both
/// handlers coexist on the same `UserContentManager`. The webkit6
/// binding's `register_script_message_handler` takes
/// `(name, world_name: Option<&str>)` — `None` selects the default
/// JavaScript world (unlike the GTK 3 / webkit2gtk binding, which has
/// the single-arg variant).
pub(crate) fn install(ucm: &UserContentManager, state: &Rc<RefCell<NavState>>) {
    let _ = ucm.register_script_message_handler(IME_HANDLER_NAME, None);

    let script = UserScript::new(
        IME_USER_SCRIPT,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    ucm.add_script(&script);

    let s = state.clone();
    ucm.connect_script_message_received(Some(IME_HANDLER_NAME), move |_ucm, value| {
        // `value` is `&javascriptcore::Value`; `to_str` is inherent on
        // the `javascriptcore6` binding (see `script_message::install`
        // for the same call site).
        let payload = value.to_str().to_string();
        if let Some(event) = parse_event(&payload) {
            s.borrow_mut().events.push_back(event);
        }
    });
}

/// Parse a pipe-delimited payload posted by the IME observer script
/// into a [`NavigationEvent`]. Verbatim port of the GTK 3 / WPE
/// precedents' `parse_event`.
fn parse_event(payload: &str) -> Option<NavigationEvent> {
    if payload == "blur" {
        return Some(NavigationEvent::TextInputBlurred);
    }
    let mut parts = payload.split('|');
    let kind = parts.next()?;
    let element_kind = parts.next()?.to_string();
    let input_type = parts.next()?.to_string();
    let input_mode = parts.next()?.to_string();
    let autocomplete = parts.next()?.to_string();
    let is_password = parts.next()? == "1";
    let is_multiline = parts.next()? == "1";
    let selection_start: u32 = parts.next()?.parse().ok()?;
    let selection_end: u32 = parts.next()?.parse().ok()?;
    let x: f64 = parts.next()?.parse().ok()?;
    let y: f64 = parts.next()?.parse().ok()?;
    let width: f64 = parts.next()?.parse().ok()?;
    let height: f64 = parts.next()?.parse().ok()?;
    let state = TextInputState {
        element_kind,
        input_type,
        input_mode,
        autocomplete,
        is_multiline,
        is_password,
        selection_start,
        selection_end,
        caret_rect: TextInputRect {
            x,
            y,
            width,
            height,
        },
    };
    match kind {
        "focus" => Some(NavigationEvent::TextInputFocused { state }),
        "change" => Some(NavigationEvent::TextInputChanged { state }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_blur() {
        match parse_event("blur") {
            Some(NavigationEvent::TextInputBlurred) => {}
            other => panic!("expected TextInputBlurred, got {other:?}"),
        }
    }

    #[test]
    fn parse_focus_full_payload() {
        // kind | tag | inputType | inputMode | autocomplete | isPassword
        // | isMultiline | selStart | selEnd | x | y | width | height
        let payload = "focus|input|text|email|username|0|0|3|7|10|20|200|24";
        let event = parse_event(payload).expect("focus payload should parse");
        match event {
            NavigationEvent::TextInputFocused { state } => {
                assert_eq!(state.element_kind, "input");
                assert_eq!(state.input_type, "text");
                assert_eq!(state.input_mode, "email");
                assert_eq!(state.autocomplete, "username");
                assert!(!state.is_password);
                assert!(!state.is_multiline);
                assert_eq!(state.selection_start, 3);
                assert_eq!(state.selection_end, 7);
                assert_eq!(state.caret_rect.x, 10.0);
                assert_eq!(state.caret_rect.y, 20.0);
                assert_eq!(state.caret_rect.width, 200.0);
                assert_eq!(state.caret_rect.height, 24.0);
            }
            other => panic!("expected TextInputFocused, got {other:?}"),
        }
    }

    #[test]
    fn parse_change_full_payload() {
        let payload = "change|textarea|||off|0|1|0|0|0|0|400|120";
        let event = parse_event(payload).expect("change payload should parse");
        match event {
            NavigationEvent::TextInputChanged { state } => {
                assert_eq!(state.element_kind, "textarea");
                assert_eq!(state.input_type, "");
                assert_eq!(state.input_mode, "");
                assert_eq!(state.autocomplete, "off");
                assert!(!state.is_password);
                assert!(state.is_multiline);
                assert_eq!(state.caret_rect.width, 400.0);
                assert_eq!(state.caret_rect.height, 120.0);
            }
            other => panic!("expected TextInputChanged, got {other:?}"),
        }
    }

    #[test]
    fn parse_password_focus() {
        let payload = "focus|input|password|||1|0|0|0|0|0|180|24";
        let event = parse_event(payload).expect("password focus should parse");
        match event {
            NavigationEvent::TextInputFocused { state } => {
                assert!(state.is_password);
                assert_eq!(state.input_type, "password");
            }
            other => panic!("expected TextInputFocused, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_returns_none() {
        // Wrong kind.
        assert!(parse_event("garbage|input|text|||0|0|0|0|0|0|0|0").is_none());
        // Truncated.
        assert!(parse_event("focus|input|text").is_none());
        // Non-numeric selection.
        assert!(parse_event("focus|input|text|||0|0|abc|0|0|0|0|0").is_none());
        // Empty.
        assert!(parse_event("").is_none());
    }
}

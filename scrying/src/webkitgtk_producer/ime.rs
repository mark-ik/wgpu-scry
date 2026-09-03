// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! JS-side observability for IME state: focus / blur / selection
//! changes on editable elements. The producer registers a dedicated
//! `scryIme` script-message handler and injects a user script that
//! watches `focusin` / `focusout` / `input` / `selectionchange` and
//! posts a pipe-delimited state payload. Each payload becomes a
//! [`crate::NavigationEvent::TextInputFocused`] /
//! [`TextInputChanged`] / [`TextInputBlurred`] on the nav-event
//! queue.
//!
//! The trust gap closed by Phase 2c's native input dispatch means
//! programmatic focus via JS / via native click both surface the
//! same focused-element metadata, so a host can wire its own IME UI
//! (winit's `set_ime_cursor_area` with the caret rect, etc.) the
//! same way it does on macOS / Windows.

use std::cell::RefCell;
use std::rc::Rc;

use javascriptcore::ValueExt;
use webkit2gtk::{
    UserContentInjectedFrames, UserContentManager, UserContentManagerExt, UserScript,
    UserScriptInjectionTime,
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

pub(crate) fn install(ucm: &UserContentManager, state: &Rc<RefCell<NavState>>) {
    let _ = ucm.register_script_message_handler(IME_HANDLER_NAME);
    let script = UserScript::new(
        IME_USER_SCRIPT,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    ucm.add_script(&script);

    let s = state.clone();
    ucm.connect_script_message_received(Some(IME_HANDLER_NAME), move |_ucm, result| {
        let payload = match result.js_value() {
            Some(v) => v.to_str().to_string(),
            None => return,
        };
        let event = parse_event(&payload);
        if let Some(e) = event {
            s.borrow_mut().events.push_back(e);
        }
    });
}

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

//! JS-side observability for IME state on the WPE producer.
//!
//! Direct port of `webkitgtk_producer/ime.rs`. The producer registers
//! a dedicated `scryIme` script-message handler on the WebView's
//! auto-created `WebKitUserContentManager` and injects a user script
//! that watches `focusin` / `focusout` / `input` / `selectionchange`
//! on editable elements, posting a pipe-delimited state payload each
//! time. Each payload becomes a
//! [`crate::NavigationEvent::TextInputFocused`] /
//! [`TextInputChanged`] / [`TextInputBlurred`] on the producer's
//! nav-event queue, drained by `poll_navigation_event`.
//!
//! Built on the 4c.5.a script-message bridge: a second handler name
//! on the same UCM. The signal extraction uses the same
//! `RustClosure::new_local` + `jsc_value_to_string` pattern as
//! `script_message::install` because `closure_local!` rejects
//! `JSCValue*` (not a registered GType in its arg-type tables).

use std::cell::RefCell;
use std::rc::Rc;

use crate::{NavigationEvent, TextInputRect, TextInputState};

use super::ffi;
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

/// Register the `scryIme` script-message handler on the WebView's
/// auto-created `WebKitUserContentManager`, wire the signal closure that
/// translates incoming payloads into `NavigationEvent`s on `nav_state`,
/// and inject the IME observer script.
///
/// Connect ordering mirrors `script_message::install`: connect the
/// signal first, then register the handler name (per the WebKit doc's
/// race-condition guidance), then add the user script.
///
/// The signal closure uses `RustClosure::new_local` over
/// `&[glib::Value]` rather than `closure_local!` for the same reason
/// `script_message::install` does — `JSCValue*` is not a registered
/// GType in the macro's arg-type tables, so the macro's type check
/// rejects it.
pub(super) fn install(webview: &glib::Object, nav_state: Rc<RefCell<NavState>>) {
    use glib::prelude::*;
    use glib::translate::ToGlibPtr;

    let raw_view: *mut ffi::WebKitWebView =
        ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(webview).0 as *mut _;
    // SAFETY: raw_view is borrowed from the owned `webview`; the UCM
    // is transfer-none (owned by the webview).
    let ucm_raw = unsafe { ffi::webkit_web_view_get_user_content_manager(raw_view) };
    debug_assert!(!ucm_raw.is_null(), "WebKitWebView auto-creates a default UCM");

    // Wrap the UCM as a glib::Object so we can `connect_closure` on it.
    // SAFETY: ucm_raw is a borrowed (transfer-none) GObject pointer
    // from the webview; `from_glib_none` adds a ref the wrapper
    // releases on drop. The wrapper lives only inside this function;
    // the UCM stays alive transitively through the webview for the
    // producer's lifetime.
    let ucm_obj: glib::Object =
        unsafe { glib::translate::from_glib_none(ucm_raw as *mut glib::gobject_ffi::GObject) };

    // 1. Connect first (per WebKit doc race-condition guidance).
    {
        let state = nav_state.clone();
        let closure = glib::RustClosure::new_local(move |values| {
            // values: [ucm: GObject, value: JSCValue (boxed as GObject)]
            let Some(value) = values.get(1) else {
                return None;
            };
            let raw_value: *mut ffi::JSCValue = match value.get::<glib::Object>() {
                Ok(obj) => {
                    ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&obj).0
                        as *mut ffi::JSCValue
                }
                Err(_) => std::ptr::null_mut(),
            };
            if raw_value.is_null() {
                return None;
            }
            // SAFETY: `raw_value` is a borrowed JSCValue* from the
            // signal arg, valid for the duration of this closure body.
            // `jsc_value_to_string` returns a g_malloc'd C string the
            // caller must release with `g_free`. The page script's
            // `postMessage(...)` always sends a JS string, so
            // `to_string` returns the message text directly.
            let c_str = unsafe { ffi::jsc_value_to_string(raw_value) };
            if !c_str.is_null() {
                let s = unsafe { std::ffi::CStr::from_ptr(c_str) }
                    .to_string_lossy()
                    .into_owned();
                // SAFETY: jsc_value_to_string returns a g_malloc'd
                // string; releasing through g_free matches its
                // allocator.
                unsafe {
                    glib::ffi::g_free(c_str as *mut std::ffi::c_void);
                }
                if let Some(event) = parse_event(&s) {
                    state.borrow_mut().events.push_back(event);
                }
            }
            None
        });
        ucm_obj.connect_closure(
            &format!("script-message-received::{IME_HANDLER_NAME}"),
            false,
            closure,
        );
    }

    // 2. Register the "scryIme" handler in the default world.
    let c_name = std::ffi::CString::new(IME_HANDLER_NAME).expect("static str without NUL");
    // SAFETY: ucm_raw + c_name valid for the call; world_name=NULL is
    // the documented "default world" sentinel.
    let registered = unsafe {
        ffi::webkit_user_content_manager_register_script_message_handler(
            ucm_raw,
            c_name.as_ptr(),
            std::ptr::null(),
        )
    };
    debug_assert!(
        registered != 0,
        "scryIme handler name collision on a freshly-created UCM is impossible",
    );

    // 3. Inject the IME observer script at document-start, in all frames.
    let c_script = std::ffi::CString::new(IME_USER_SCRIPT).expect("static str without NUL");
    // SAFETY: c_script outlives the call; webkit_user_script_new copies
    // the source string. allow_list/block_list NULL means "match all".
    let script = unsafe {
        ffi::webkit_user_script_new(
            c_script.as_ptr(),
            ffi::WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
            ffi::WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if !script.is_null() {
        // SAFETY: add_script transfers ownership of one ref to the
        // UCM. The transfer-full ref `webkit_user_script_new` returned
        // is consumed; we don't need to unref ourselves.
        unsafe {
            ffi::webkit_user_content_manager_add_script(ucm_raw, script);
        }
    }
}

/// Parse a pipe-delimited payload posted by the IME observer script
/// into a [`NavigationEvent`]. Verbatim port of the GTK precedent's
/// `parse_event`.
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

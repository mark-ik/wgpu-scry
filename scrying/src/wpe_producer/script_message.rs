// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! JS messaging bridge for the WPE producer.
//!
//! Port of `webkitgtk_producer/script_message.rs`. Foundation for
//! IME observability (4c.5.e) and any future page↔host message
//! protocol. The page side uses the `window.chrome.webview` shim
//! we inject (`postMessage`/`addEventListener('message',...)`),
//! mirroring the Windows + macOS producers' conventions; the host
//! side drains via `poll_web_message` (trait method) or pumps via
//! `wait_for_web_message` (inherent — both land in Task 4 of this
//! plan).
//!
//! Task 2 of the 4c.5.a plan: this file ships `SCRY_HANDLER_NAME`,
//! the `CHROME_WEBVIEW_SHIM` JS, `escape_for_js`, and a stub
//! `install()` that registers the handler + adds the shim but
//! does NOT wire the signal closure yet (Task 3 does that, where
//! the empirical JSCValue→String extraction lives).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use super::ffi;
use super::producer::{WebSurfaceEventQueue, WpeProducer};
use crate::{WebRequestId, WebSurfaceError, WebSurfaceEvent};

/// Handler name registered with `register_script_message_handler` and
/// used on the page side as
/// `window.webkit.messageHandlers.scry.postMessage(...)`.
pub(super) const SCRY_HANDLER_NAME: &str = "scry";

/// `window.chrome.webview` JS shim — mirrors the Windows + macOS
/// producers' `chrome.webview.postMessage` / `addEventListener('message',
/// cb)` surface so page code is portable across all three. Verbatim
/// copy from the GTK precedent.
pub(super) const CHROME_WEBVIEW_SHIM: &str = r#"
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

/// Escape a string for embedding inside a double-quoted JS literal.
/// Same escape rules as JSON strings — sufficient for the
/// `__scryDispatch(...)` payload `post_web_message` builds. Verbatim
/// from the GTK precedent.
pub(super) fn escape_for_js(s: &str) -> String {
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

/// Keep the legacy message poller and the authoritative event stream in
/// lockstep at the callback boundary. The ordered queue is the source of
/// truth for new callers; the legacy queue remains a compatibility mirror.
fn enqueue_page_message(
    legacy: &Rc<RefCell<VecDeque<String>>>,
    ordered: &WebSurfaceEventQueue,
    message: String,
) {
    legacy.borrow_mut().push_back(message.clone());
    ordered
        .borrow_mut()
        .push_back(WebSurfaceEvent::WebMessage(message));
}

/// Callback-owned state for an accepted result-bearing JavaScript request.
/// Holding a strong webview ref means the matching `_finish` call is valid
/// even if a caller drops the producer before WebKit completes its work.
struct ScriptOperation {
    webview: glib::Object,
    id: WebRequestId,
    ordered_events: WebSurfaceEventQueue,
}

impl ScriptOperation {
    fn raw_webview(&self) -> *mut ffi::WebKitWebView {
        glib::translate::ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&self.webview)
            .0
            .cast()
    }
}

fn callback_ref(operation: &Rc<ScriptOperation>) -> *mut std::ffi::c_void {
    Rc::into_raw(Rc::clone(operation)).cast_mut().cast()
}

/// SAFETY: `user_data` is one unique callback reference made by
/// [`callback_ref`] for this exact WebKit request.
unsafe fn take_callback_ref(user_data: *mut std::ffi::c_void) -> Rc<ScriptOperation> {
    debug_assert!(!user_data.is_null());
    unsafe { Rc::from_raw(user_data.cast()) }
}

/// Start native asynchronous JavaScript evaluation. Synchronous acceptance
/// errors (for example an interior NUL) return an error and deliberately emit
/// no completion event. Once accepted, WebKit's one-shot callback appends
/// exactly one event with the caller-provided id.
pub(super) fn request_script_result(
    producer: &WpeProducer,
    id: WebRequestId,
    script: &str,
) -> Result<(), WebSurfaceError> {
    // Normalize to WebView2's JSON-shaped result. Returning the string
    // produced by JSON.stringify also avoids backend-specific JSCValue
    // coercion for strings, objects, and undefined.
    let script = format!(
        "(() => {{ const value = (0, eval)({}); const json = JSON.stringify(value); return json === undefined ? 'null' : json; }})()",
        escape_for_js(script)
    );
    let script = std::ffi::CString::new(script).map_err(|_| {
        WebSurfaceError::Platform("request_script_result: normalized script contained NUL".into())
    })?;
    let operation = Rc::new(ScriptOperation {
        webview: producer.handles.webview.clone(),
        id,
        ordered_events: producer.web_surface_events.clone(),
    });
    let raw_view = operation.raw_webview();
    let user_data = callback_ref(&operation);
    // SAFETY: `operation` owns a strong WebKitWebView reference until the
    // callback reclaims `user_data`; WebKit copies the script during this
    // call; its GAsync callback is documented as one-shot.
    unsafe {
        ffi::webkit_web_view_evaluate_javascript(
            raw_view,
            script.as_ptr(),
            -1,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            Some(script_result_trampoline),
            user_data,
        );
    }
    Ok(())
}

unsafe extern "C" fn script_result_trampoline(
    _source: *mut glib::gobject_ffi::GObject,
    result: *mut ffi::GAsyncResult,
    user_data: *mut std::ffi::c_void,
) {
    // SAFETY: WebKit invokes this callback exactly once for the callback ref
    // created above. Reclaiming it here balances that ownership.
    let operation = unsafe { take_callback_ref(user_data) };
    let mut error: *mut glib::ffi::GError = std::ptr::null_mut();
    // SAFETY: `operation` owns the view required by the matching finish call.
    let value = unsafe {
        ffi::webkit_web_view_evaluate_javascript_finish(operation.raw_webview(), result, &mut error)
    };
    let completion = if !error.is_null() {
        // SAFETY: GError from the finish function is transfer-full.
        Err(unsafe {
            glib::translate::from_glib_full::<*mut glib::ffi::GError, glib::Error>(error)
        }
        .message()
        .to_string())
    } else if value.is_null() {
        Err("WPE JavaScript evaluation completed without a result".into())
    } else {
        // SAFETY: `_finish` returns one owned JSCValue reference. Its text is
        // copied before that reference is released below.
        let result_text = {
            // WebKit returns an owned JSCValue even when the script threw.
            // Inspect its context before stringifying so the portable result
            // keeps the error/success distinction instead of treating an
            // exception's display text as a successful script value.
            let context = unsafe { ffi::jsc_value_get_context(value) };
            let exception = if context.is_null() {
                std::ptr::null_mut()
            } else {
                unsafe { ffi::jsc_context_get_exception(context) }
            };
            if !exception.is_null() {
                let message = unsafe { ffi::jsc_exception_get_message(exception) };
                if message.is_null() {
                    Err("WPE JavaScript evaluation raised an exception".into())
                } else {
                    Err(unsafe { std::ffi::CStr::from_ptr(message) }
                        .to_string_lossy()
                        .into_owned())
                }
            } else {
                let raw_text = unsafe { ffi::jsc_value_to_string(value) };
                if raw_text.is_null() {
                    Err("WPE could not stringify the JavaScript result".into())
                } else {
                    let text = unsafe { std::ffi::CStr::from_ptr(raw_text) }
                        .to_string_lossy()
                        .into_owned();
                    // SAFETY: `jsc_value_to_string` allocates with GLib.
                    unsafe { glib::ffi::g_free(raw_text.cast()) };
                    Ok(text)
                }
            }
        };
        // SAFETY: JSCValue is GObject-derived and `_finish` transfers one
        // owned reference to this callback.
        unsafe { glib::gobject_ffi::g_object_unref(value.cast()) };
        result_text
    };
    operation
        .ordered_events
        .borrow_mut()
        .push_back(WebSurfaceEvent::ScriptCompleted {
            id: operation.id,
            result: completion,
        });
}

/// Register the `scry` script-message handler on the WebView's
/// auto-created `WebKitUserContentManager`, wire the signal closure that
/// drains incoming page messages into `queue`, and inject the
/// `chrome.webview` shim.
///
/// Connect ordering matters: per the WebKit doc on
/// `register_script_message_handler`, "to avoid race conditions between
/// registering the handler name, and starting to receive the signals,
/// it is recommended to connect to the signal *before* registering the
/// handler name". We follow that order here.
///
/// The signal closure follows the `RustClosure::new_local` pattern from
/// `navigation.rs::install_load_signals` rather than
/// `glib::closure_local!` — `JSCValue*` is not a registered GType in the
/// arg-type tables of `closure_local!`, so the macro's compile-time
/// type check rejects it (and at runtime emits a ValueTypeMismatchError
/// panic). Going through `&[glib::Value]` and casting the raw GObject
/// pointer bypasses the marshalling check; the underlying GValue still
/// holds a real `JSCValue*` we can hand to `jsc_value_to_string`.
pub(super) fn install(
    webview: &glib::Object,
    queue: Rc<RefCell<VecDeque<String>>>,
    ordered_events: WebSurfaceEventQueue,
) {
    use glib::prelude::*;
    use glib::translate::ToGlibPtr;

    let raw_view: *mut ffi::WebKitWebView =
        ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(webview).0 as *mut _;
    // SAFETY: raw_view is borrowed from the owned `webview`; the UCM
    // is transfer-none (owned by the webview).
    let ucm_raw = unsafe { ffi::webkit_web_view_get_user_content_manager(raw_view) };
    debug_assert!(
        !ucm_raw.is_null(),
        "WebKitWebView auto-creates a default UCM"
    );

    // Wrap the UCM as a glib::Object so we can `connect_closure` on it.
    // SAFETY: ucm_raw is a borrowed (transfer-none) GObject pointer from
    // the webview; `from_glib_none` adds a ref that the wrapper releases
    // on drop. The wrapper lives only inside this function; the UCM
    // stays alive transitively through the webview for the producer's
    // lifetime.
    let ucm_obj: glib::Object =
        unsafe { glib::translate::from_glib_none(ucm_raw as *mut glib::gobject_ffi::GObject) };

    // 1. Connect first (per WebKit doc race-condition guidance).
    {
        let q = queue.clone();
        let ordered = ordered_events.clone();
        let closure = glib::RustClosure::new_local(move |values| {
            // values: [ucm: GObject, value: JSCValue (boxed as GObject)]
            let Some(value) = values.get(1) else {
                return None;
            };
            // Extract the JSCValue* from the GValue without involving
            // glib's typed `get::<T>()` path (JSCValue isn't a Rust
            // wrapper type here). The underlying storage IS a pointer
            // to a GObject-derived JSCValue.
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
            // SAFETY: `raw_value` is a borrowed JSCValue* from the signal
            // arg, valid for the duration of this closure body.
            // `jsc_value_to_string` returns a heap-allocated C string the
            // caller must release with `g_free`. The page shim's
            // `postMessage(String(msg))` always sends a JS string, so
            // `to_string` returns the message text directly.
            let c_str = unsafe { ffi::jsc_value_to_string(raw_value) };
            if !c_str.is_null() {
                let s = unsafe { std::ffi::CStr::from_ptr(c_str) }
                    .to_string_lossy()
                    .into_owned();
                // SAFETY: jsc_value_to_string returns a g_malloc'd string;
                // releasing through g_free matches its allocator.
                unsafe {
                    glib::ffi::g_free(c_str as *mut std::ffi::c_void);
                }
                enqueue_page_message(&q, &ordered, s);
            }
            None
        });
        ucm_obj.connect_closure(
            &format!("script-message-received::{SCRY_HANDLER_NAME}"),
            false,
            closure,
        );
    }

    // 2. Now register the "scry" handler in the default world. WebKit
    //    will start emitting the signal after this returns.
    let c_name = std::ffi::CString::new(SCRY_HANDLER_NAME).expect("static str without NUL");
    // SAFETY: ucm_raw + c_name valid for the call; world_name=NULL is the
    // documented "default world" sentinel.
    let registered = unsafe {
        ffi::webkit_user_content_manager_register_script_message_handler(
            ucm_raw,
            c_name.as_ptr(),
            std::ptr::null(),
        )
    };
    debug_assert!(
        registered != 0,
        "scry handler name collision on a freshly-created UCM is impossible",
    );

    // 3. Inject the chrome.webview shim at document-start, in all frames.
    let c_shim = std::ffi::CString::new(CHROME_WEBVIEW_SHIM).expect("static str without NUL");
    // SAFETY: c_shim outlives the call; webkit_user_script_new copies
    // the source string. allow_list/block_list NULL means "match all".
    let script = unsafe {
        ffi::webkit_user_script_new(
            c_shim.as_ptr(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_plain_ascii() {
        assert_eq!(escape_for_js("hello"), "\"hello\"");
    }

    #[test]
    fn escape_double_quote() {
        assert_eq!(escape_for_js("a\"b"), r#""a\"b""#);
    }

    #[test]
    fn escape_newline() {
        assert_eq!(escape_for_js("a\nb"), r#""a\nb""#);
    }

    #[test]
    fn escape_control_byte() {
        // \x01 is a control byte below 0x20 — must hex-escape.
        assert_eq!(escape_for_js("\x01"), "\"\\u0001\"");
    }

    #[test]
    fn escape_empty_string() {
        assert_eq!(escape_for_js(""), "\"\"");
    }

    #[test]
    fn page_message_is_mirrored_without_changing_ordered_payload() {
        let legacy = Rc::new(RefCell::new(VecDeque::new()));
        let ordered = Rc::new(RefCell::new(VecDeque::new()));
        enqueue_page_message(&legacy, &ordered, "from-page".into());

        assert_eq!(
            legacy.borrow_mut().pop_front().as_deref(),
            Some("from-page")
        );
        assert!(matches!(
            ordered.borrow_mut().pop_front(),
            Some(WebSurfaceEvent::WebMessage(message)) if message == "from-page"
        ));
    }
}

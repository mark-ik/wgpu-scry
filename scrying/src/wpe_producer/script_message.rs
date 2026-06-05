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

/// Register the `scry` script-message handler on the WebView's
/// auto-created `WebKitUserContentManager` and inject the
/// `chrome.webview` shim.
///
/// **Task 2 stub:** this function performs the registration + shim
/// injection but does NOT wire the signal closure. Task 3 of the
/// 4c.5.a plan adds the `script-message-received::scry` signal
/// connection (the empirical JSCValue → String marshalling).
/// `queue` is plumbed through so Task 3 can capture it without
/// changing the public signature.
pub(super) fn install(
    webview: &glib::Object,
    queue: Rc<RefCell<VecDeque<String>>>,
) {
    let _ = queue; // consumed by the signal closure in Task 3
    use glib::translate::ToGlibPtr;

    let raw_view: *mut ffi::WebKitWebView =
        ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(webview).0 as *mut _;
    // SAFETY: raw_view is borrowed from the owned `webview`; the UCM
    // is transfer-none (owned by the webview).
    let ucm = unsafe { ffi::webkit_web_view_get_user_content_manager(raw_view) };
    debug_assert!(!ucm.is_null(), "WebKitWebView auto-creates a default UCM");

    // Register the "scry" handler in the default world (NULL world_name).
    let c_name = std::ffi::CString::new(SCRY_HANDLER_NAME).expect("static str without NUL");
    // SAFETY: ucm + c_name valid for the call; world_name=NULL is the
    // documented "default world" sentinel.
    let registered = unsafe {
        ffi::webkit_user_content_manager_register_script_message_handler(
            ucm,
            c_name.as_ptr(),
            std::ptr::null(),
        )
    };
    debug_assert!(
        registered != 0,
        "scry handler name collision on a freshly-created UCM is impossible",
    );

    // Inject the chrome.webview shim at document-start, in all frames.
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
        unsafe { ffi::webkit_user_content_manager_add_script(ucm, script); }
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
}

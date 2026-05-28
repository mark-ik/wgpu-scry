//! Empirical binding spike (Phase 4c.2 Task 2): construct a headless
//! `WPEDisplay` and a `WebKitWebView` bound to it, then verify the binding
//! at runtime via the ignored test below.
//!
//! GObject construct properties live in WebKit's C sources (not the public
//! headers), so the runtime test is the oracle for *how* the WebView binds
//! to a display. The resolved approach: construct the WebView with its
//! `display` construct property set to our headless `WPEDisplay` via the
//! variadic `g_object_new`.

use super::ffi;
use crate::WebSurfaceError;
use glib::translate::{IntoGlib, from_glib, from_glib_full};
use std::os::raw::c_char;

/// Construct a headless `WPEDisplay` and a `WebKitWebView` bound to it.
///
/// Returns the WebView (owned as a [`glib::Object`]) and the raw headless
/// display pointer it was bound to (so callers can assert the binding /
/// reuse the display). The display is owned by the WebView once bound; the
/// returned raw pointer is a borrow valid for the WebView's lifetime.
pub(super) fn build_headless_webview()
-> Result<(glib::Object, *mut ffi::WPEDisplay), WebSurfaceError> {
    // 1. Self-owned headless display (no compositor surface).
    let display = unsafe { ffi::wpe_display_headless_new() };
    if display.is_null() {
        return Err(WebSurfaceError::Platform(
            "wpe_display_headless_new() returned null — WPEPlatform headless \
             backend unavailable (needs a DRM render node)"
                .into(),
        ));
    }

    // 2. Construct the WebView with its `display` construct property bound to
    //    our headless display. The webview GType is fetched at runtime so we
    //    don't depend on a compile-time type registration.
    let webview_gtype: glib::Type = unsafe { from_glib(ffi::webkit_web_view_get_type()) };

    // An explicit ephemeral network session avoids WebKit auto-creating a
    // persistent default WebsiteDataStore (whose destructor asserts during
    // WebKit's atexit teardown). Not required for the display binding, but it
    // keeps process teardown clean and is the session a headless renderer
    // wants anyway.
    let network_session = unsafe { ffi::webkit_network_session_new_ephemeral() };

    // Variadic g_object_new is the most version-robust path under glib 0.18:
    // it sidesteps building a GValue array by hand and lets the property
    // setter transform-check the display pointer. Rust can call C variadics.
    let raw_obj = unsafe {
        glib::gobject_ffi::g_object_new(
            webview_gtype.into_glib(),
            c"display".as_ptr(),
            display,
            c"network-session".as_ptr(),
            network_session,
            std::ptr::null::<c_char>(),
        )
    };
    if raw_obj.is_null() {
        return Err(WebSurfaceError::Platform(
            "g_object_new(WebKitWebView, display) returned null".into(),
        ));
    }

    // 3. Take ownership of the freshly-constructed (floating-free) object.
    let webview: glib::Object = unsafe { from_glib_full(raw_obj) };
    Ok((webview, display))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::translate::ToGlibPtr;

    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn headless_webview_binds_display() {
        let (webview, display) = build_headless_webview().expect("build headless webview");
        let raw: *mut ffi::WebKitWebView =
            ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&webview).0 as *mut _;
        let bound = unsafe { ffi::webkit_web_view_get_display(raw) };
        assert_eq!(
            bound, display,
            "webview must be bound to our headless display"
        );
        let view = unsafe { ffi::webkit_web_view_get_wpe_view(raw) };
        assert!(!view.is_null(), "webview must expose a WPEView");
    }
}

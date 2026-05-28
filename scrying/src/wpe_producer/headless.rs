//! Headless WPE display + WebView construction (Phase 4c.2).
//!
//! Constructs a self-owned `WPEDisplayHeadless` and a `WebKitWebView` bound
//! to it via the variadic `g_object_new` construct-property path — the most
//! version-robust approach under glib 0.18, as confirmed by the Task-2 spike.
//!
//! An ephemeral `WebKitNetworkSession` is supplied at construction time so
//! WebKit does not auto-create a persistent on-disk `WebsiteDataStore` whose
//! destructor asserts during process teardown (`atexit`).

use super::ffi;
use crate::WebSurfaceError;
use glib::translate::{IntoGlib, from_glib, from_glib_full};
use std::os::raw::c_char;

/// Construct a headless `WPEDisplay` and a `WebKitWebView` bound to it.
///
/// Returns the owned WebView (as a [`glib::Object`]) and the raw [`ffi::WPEView`]
/// pointer borrowed from that webview. The `WPEView` pointer is valid for the
/// lifetime of the returned `glib::Object`.
///
/// The transfer-full display and network-session construction refs are released
/// inside this function after `g_object_new` takes its own references on each
/// construct-property object. The `WebView` retains its own refs, keeping both
/// alive for its lifetime.
pub(super) fn build_producer_view()
    -> Result<(glib::Object, *mut ffi::WPEView), WebSurfaceError>
{
    // 1. Self-owned headless display (no compositor surface).
    let display = unsafe { ffi::wpe_display_headless_new() };
    if display.is_null() {
        return Err(WebSurfaceError::Platform(
            "wpe_display_headless_new() returned null; no headless WPE display available".into(),
        ));
    }

    // 2. Ephemeral network session — avoids WebKit auto-creating a persistent
    //    default WebsiteDataStore (whose destructor asserts during atexit teardown).
    let network_session = unsafe { ffi::webkit_network_session_new_ephemeral() };

    // 3. Fetch the WebKitWebView GType at runtime (most version-robust path).
    let webview_gtype: glib::Type = unsafe { from_glib(ffi::webkit_web_view_get_type()) };

    // 4. Construct the WebView with its `display` and `network-session` construct
    //    properties. Variadic g_object_new sidesteps building a GValue array by
    //    hand and lets the property setter transform-check the display pointer.
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
            "g_object_new returned null for WebKitWebView".into(),
        ));
    }

    // 5. Take ownership of the freshly-constructed object.
    let webview: glib::Object = unsafe { from_glib_full(raw_obj) };

    // 6. Release the transfer-full refs we received from the *_new constructors.
    //    g_object_new took its own ref on each construct-property object, so the
    //    WebView retains both display and network_session for its lifetime. We
    //    must release our transfer-full refs or they leak.
    //    NOTE: capture `display` for the binding check (step 7) BEFORE unreffing;
    //    the pointer remains valid to compare because the WebView holds a ref.
    unsafe {
        // SAFETY: display and network_session are valid GObject pointers obtained
        // from transfer-full constructors above. The WebView has incremented their
        // refcounts via g_object_new, so unreffing here is safe and correct.
        glib::gobject_ffi::g_object_unref(display as *mut glib::gobject_ffi::GObject);
        glib::gobject_ffi::g_object_unref(network_session as *mut glib::gobject_ffi::GObject);
    }

    // 7. Get the raw WebKitWebView pointer (borrowed from the owned `webview`).
    // SAFETY: `webview` is a valid GObject of type WebKitWebView; the cast is
    // safe because we just constructed it with webkit_web_view_get_type().
    let raw_webview: *mut ffi::WebKitWebView =
        glib::translate::ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&webview).0
            as *mut _;

    // 8. Guard: verify the WebView actually bound to our headless display.
    // SAFETY: raw_webview is a borrowed pointer valid for the lifetime of `webview`.
    let bound = unsafe { ffi::webkit_web_view_get_display(raw_webview) };
    if bound != display {
        return Err(WebSurfaceError::Platform(
            "WebView did not bind to the headless display".into(),
        ));
    }

    // 9. Get the WPEView from the WebView.
    // SAFETY: raw_webview is a borrowed pointer valid for the lifetime of `webview`.
    let view = unsafe { ffi::webkit_web_view_get_wpe_view(raw_webview) };
    if view.is_null() {
        return Err(WebSurfaceError::Platform(
            "WebView exposed no WPEView".into(),
        ));
    }

    Ok((webview, view))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn headless_webview_binds_display() {
        let (_webview, view) = build_producer_view().expect("build producer view");
        assert!(!view.is_null(), "webview must expose a WPEView");
    }
}

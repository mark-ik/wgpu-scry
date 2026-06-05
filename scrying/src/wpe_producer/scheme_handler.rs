//! Custom URL scheme handlers for the WPE producer.
//!
//! Direct port of `webkitgtk_producer/scheme_handler.rs`. Each registered
//! `(scheme, handler)` pair causes WebKit to route page-side fetches of
//! `<scheme>://...` URIs through the host's closure, which returns a
//! [`UrlSchemeResponse`](crate::UrlSchemeResponse) (status + headers +
//! body). The handler is wrapped into a [`ffi::WebKitURISchemeRequestCallback`]
//! that builds a [`ffi::WebKitURISchemeResponse`] backed by a
//! [`glib::Bytes`]-owned `GMemoryInputStream`, attaches the host's headers
//! through soup3's `MessageHeaders`, and calls
//! `webkit_uri_scheme_request_finish_with_response`.
//!
//! ## Why hand-rolled FFI rather than higher-level bindings
//!
//! The wpe feature set currently exposes only `glib` + `soup3` from the
//! gtk-rs family; pulling in `gio` to get `MemoryInputStream` would
//! widen the dep surface for two FFI calls. We hand-roll
//! `g_memory_input_stream_new_from_bytes` instead — it lives in
//! libgio-2.0 which libwpe-webkit-2.0 already pulls in transitively, so
//! the symbol is reachable at link time without an extra Rust crate.
//!
//! ## Lifetime of registered handlers
//!
//! `webkit_web_context_register_uri_scheme` takes a `GDestroyNotify`
//! that runs when the WebContext is finalized (i.e. when the producer
//! drops). Each registered handler is boxed at registration time and
//! reclaimed by the destroy-notify trampoline — the box is NOT consumed
//! by the per-request callback, which fires repeatedly across the
//! producer's lifetime.

use std::collections::HashMap;

use glib::translate::ToGlibPtr;
use soup::{MessageHeaders, MessageHeadersType};

use super::ffi;
use crate::{UrlSchemeHandlerFn, UrlSchemeResponse};

/// Per-scheme trampoline payload. `scheme` is captured for the
/// fallback URI returned when WebKit ever hands us a NULL `get_uri`
/// (defensive — mirrors the GTK precedent's `unwrap_or_else`).
struct HandlerPayload {
    scheme: String,
    handler: UrlSchemeHandlerFn,
}

/// Register each `(scheme, handler)` pair on `context`. Called from
/// the producer's `new_with_url_schemes` constructor BEFORE the
/// `WebView` is built so the very first navigation can already
/// resolve custom-scheme URIs.
///
/// The WebContext takes ownership of each handler payload through the
/// `GDestroyNotify` we pass; the box is released when the WebContext is
/// finalized (which happens when the producer's WebView is dropped and
/// it releases its construct-property ref on the context).
pub(crate) fn register_all(
    context: *mut ffi::WebKitWebContext,
    schemes: HashMap<String, UrlSchemeHandlerFn>,
) {
    for (scheme, handler) in schemes {
        let c_scheme = match std::ffi::CString::new(scheme.as_str()) {
            Ok(s) => s,
            // A scheme name containing an interior NUL is structurally
            // impossible (URI schemes are alpha-numeric per RFC 3986).
            // If a caller somehow injects one, skip it rather than
            // panicking — registration failure is non-fatal.
            Err(_) => continue,
        };
        let payload = Box::new(HandlerPayload { scheme, handler });
        let raw_payload = Box::into_raw(payload) as *mut std::ffi::c_void;
        // SAFETY: `context` is a valid WebKitWebContext* owned by the
        // caller for the duration of this call (and longer — the
        // WebView, built after this returns, takes its own ref through
        // the `web-context` construct property). `c_scheme` outlives
        // the call (registration copies the string). `raw_payload` is
        // a fresh `Box::into_raw` allocation matched by the
        // `destroy_payload` trampoline below; webkit will invoke the
        // destroy-notify exactly once when the context is finalized.
        unsafe {
            ffi::webkit_web_context_register_uri_scheme(
                context,
                c_scheme.as_ptr(),
                scheme_request_trampoline,
                raw_payload,
                Some(destroy_payload),
            );
        }
    }
}

/// C-side callback invoked once per page-side fetch of a registered
/// scheme. Forwards into the boxed Rust handler without consuming the
/// box — see [`destroy_payload`] for the cleanup side.
unsafe extern "C" fn scheme_request_trampoline(
    request: *mut ffi::WebKitURISchemeRequest,
    user_data: *mut std::ffi::c_void,
) {
    // SAFETY: `user_data` was produced by `Box::into_raw` on a
    // `Box<HandlerPayload>` in `register_all`; it lives until the
    // matching `destroy_payload` fires when the WebContext is
    // finalized. We borrow through `&*` rather than `Box::from_raw`
    // because this callback runs MANY times per registration — the
    // box must not be consumed here.
    let payload: &HandlerPayload = unsafe { &*(user_data as *const HandlerPayload) };
    handle_request(payload, request);
}

/// `GDestroyNotify` paired with [`scheme_request_trampoline`]. Runs
/// exactly once per registration when the owning WebContext is
/// finalized. Reclaims the boxed payload.
unsafe extern "C" fn destroy_payload(user_data: *mut std::ffi::c_void) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `user_data` was the matching `Box::into_raw` allocation
    // for a `Box<HandlerPayload>` in `register_all`. WebKit guarantees
    // exactly one destroy-notify call per registration, so this takes
    // back the single allocation exactly once.
    let _boxed: Box<HandlerPayload> =
        unsafe { Box::from_raw(user_data as *mut HandlerPayload) };
    // _boxed drops here, releasing the Arc inside the UrlSchemeHandlerFn.
}

fn handle_request(payload: &HandlerPayload, request: *mut ffi::WebKitURISchemeRequest) {
    // 1. Extract the request URI (transfer-none). Fall back to
    //    `<scheme>:` if WebKit ever hands us a NULL — mirrors the GTK
    //    precedent's defensive default.
    let uri = unsafe {
        let raw = ffi::webkit_uri_scheme_request_get_uri(request);
        if raw.is_null() {
            format!("{}:", payload.scheme)
        } else {
            std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned()
        }
    };

    // 2. Invoke the host handler. This may run arbitrary user code —
    //    panics would unwind across the C boundary, which is UB. The
    //    UrlSchemeHandlerFn contract documents that handlers should
    //    return rather than panic; we don't catch_unwind here because
    //    that would mask programmer errors in tests. Matches the GTK
    //    precedent's posture.
    let response: UrlSchemeResponse = (payload.handler)(&uri);

    // 3. Wrap the body in a GBytes → GMemoryInputStream pair. WebKit
    //    refs the stream internally; we drop our ref after handing it
    //    to `webkit_uri_scheme_response_new`.
    let body_len = response.body.len() as i64;
    let bytes = glib::Bytes::from_owned(response.body);
    // SAFETY: `bytes` is a valid GBytes* (transfer-none borrow via
    // to_glib_none). g_memory_input_stream_new_from_bytes takes a ref
    // on the bytes; the GBytes wrapper releases its own ref on Drop.
    let stream: *mut ffi::GInputStream = unsafe {
        ffi::g_memory_input_stream_new_from_bytes(
            ToGlibPtr::<*mut glib::ffi::GBytes>::to_glib_none(&bytes).0,
        )
    };
    if stream.is_null() {
        // Construction failure is unrecoverable here — the request
        // will hang on the page side until WebKit times it out, but
        // that's preferable to dereffing NULL. Diagnostic eprintln so
        // a runtime smoke surfaces it.
        eprintln!(
            "wpe_producer::scheme_handler: g_memory_input_stream_new_from_bytes returned NULL"
        );
        return;
    }

    // SAFETY: `stream` is a transfer-full GInputStream* from the
    // constructor above. webkit_uri_scheme_response_new takes its own
    // ref, so we release ours after the call (matches the
    // construct-then-unref dance in `headless::build_producer_view`).
    let scheme_response: *mut ffi::WebKitURISchemeResponse = unsafe {
        ffi::webkit_uri_scheme_response_new(stream, body_len)
    };
    // Release our ref on the input stream; the response now owns it.
    unsafe {
        glib::gobject_ffi::g_object_unref(stream as *mut glib::gobject_ffi::GObject);
    }
    if scheme_response.is_null() {
        eprintln!("wpe_producer::scheme_handler: webkit_uri_scheme_response_new returned NULL");
        return;
    }

    // 4. Set content-type. `set_content_type` copies the string before
    //    returning, so the local CString can drop at end of scope.
    if let Ok(c_mime) = std::ffi::CString::new(response.mime_type.as_str()) {
        // SAFETY: scheme_response valid for the call; c_mime outlives it.
        unsafe {
            ffi::webkit_uri_scheme_response_set_content_type(scheme_response, c_mime.as_ptr());
        }
    }

    // 5. Attach extra HTTP headers (if any). Build a soup3
    //    `MessageHeaders` of type Response and append each
    //    `(name, value)` pair. `set_http_headers` takes a transfer-full
    //    SoupMessageHeaders*; we hand ownership of our local headers
    //    object over by `into_glib_ptr` and drop our Rust wrapper
    //    without unref'ing.
    if !response.headers.is_empty() {
        let headers = MessageHeaders::new(MessageHeadersType::Response);
        for (name, value) in &response.headers {
            headers.append(name, value);
        }
        // `to_glib_full` returns a transfer-full SoupMessageHeaders*;
        // the WebKit response now owns the reference. (`MessageHeaders`
        // is a boxed type in soup3 — `to_glib_full` copies it for us.)
        use glib::translate::ToGlibPtr;
        let raw_headers: *mut soup::ffi::SoupMessageHeaders =
            ToGlibPtr::<*mut soup::ffi::SoupMessageHeaders>::to_glib_full(&headers);
        // SAFETY: raw_headers is a valid transfer-full SoupMessageHeaders*;
        // set_http_headers adopts ownership.
        unsafe {
            ffi::webkit_uri_scheme_response_set_http_headers(
                scheme_response,
                raw_headers as *mut std::ffi::c_void,
            );
        }
    }

    // 6. Finish the request. WebKit takes its own ref on the response;
    //    we release our transfer-full ref afterwards.
    // SAFETY: both pointers valid for the call; finish_with_response
    // does not adopt the response, so we still own one ref to release.
    unsafe {
        ffi::webkit_uri_scheme_request_finish_with_response(request, scheme_response);
        glib::gobject_ffi::g_object_unref(
            scheme_response as *mut glib::gobject_ffi::GObject,
        );
    }
}

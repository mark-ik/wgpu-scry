//! Custom URL scheme handlers, registered against the producer's
//! `WebContext`.
//!
//! Port of [`crate::webkitgtk_producer::scheme_handler`] adapted to
//! the `webkit6 = "0.6"` + `soup3 = "0.9"` + `glib 0.22` binding set.
//!
//! Same conceptual surface as the GTK 3 precedent and the macOS
//! WkWebView producer: each handler is a closure
//! `Fn(&str) -> UrlSchemeResponse` invoked when the WebView fetches
//! a URI whose scheme has been registered. The producer wraps it
//! into a WebKit `URISchemeRequest` callback that builds a
//! `URISchemeResponse` (content-type + extra headers from
//! `UrlSchemeResponse`) and calls `finish_with_response`, so the
//! page sees a real HTTP-shaped response (status, headers, body)
//! rather than a raw byte blob.
//!
//! ## Differences vs the GTK 3 precedent
//!
//! - API namespace `webkit6::*`. `WebContext::register_uri_scheme`
//!   stays on `WebContext` under the 2022 GLib API — unlike cookies
//!   (which moved to `NetworkSession`) or downloads
//!   (`download-started` moved to `NetworkSession`), scheme
//!   registration is still process-/context-wide.
//! - `soup3 = 0.9` is reached via `webkit6::soup::*` (mirrors the
//!   import style used in `cookies.rs`).
//! - `webkit6::gio::MemoryInputStream` replaces
//!   `webkit2gtk::gio::MemoryInputStream`.

use std::collections::HashMap;

use webkit6::soup::{MessageHeaders, MessageHeadersType};
use webkit6::{URISchemeRequest, URISchemeResponse, WebContext};

use crate::{UrlSchemeHandlerFn, UrlSchemeResponse};

/// Register each `(scheme, handler)` pair on `context`. Called from
/// the producer's `new_with_url_schemes` constructor before the
/// `WebView` is built so the very first navigation can already
/// resolve custom-scheme URIs.
pub(crate) fn register_all(context: &WebContext, schemes: HashMap<String, UrlSchemeHandlerFn>) {
    for (scheme, handler) in schemes {
        let scheme_clone = scheme.clone();
        let handler = handler.clone();
        context.register_uri_scheme(&scheme, move |request| {
            handle_request(&scheme_clone, &handler, request);
        });
    }
}

fn handle_request(scheme: &str, handler: &UrlSchemeHandlerFn, request: &URISchemeRequest) {
    let uri = request
        .uri()
        .map(|g| g.to_string())
        .unwrap_or_else(|| format!("{scheme}:"));
    let response: UrlSchemeResponse = handler(&uri);

    // Wrap the body in a memory input stream — WebKit consumes the
    // bytes from this stream asynchronously, so the glib::Bytes /
    // MemoryInputStream pair must outlive the call. WebKit refs them
    // internally; the handles drop when WebKit releases them.
    let body_len = response.body.len() as i64;
    let bytes = webkit6::glib::Bytes::from_owned(response.body);
    let stream = webkit6::gio::MemoryInputStream::from_bytes(&bytes);

    let scheme_response = URISchemeResponse::new(&stream, body_len);
    scheme_response.set_content_type(&response.mime_type);

    if !response.headers.is_empty() {
        let headers = MessageHeaders::new(MessageHeadersType::Response);
        for (name, value) in &response.headers {
            headers.append(name, value);
        }
        scheme_response.set_http_headers(headers);
    }

    request.finish_with_response(&scheme_response);
}

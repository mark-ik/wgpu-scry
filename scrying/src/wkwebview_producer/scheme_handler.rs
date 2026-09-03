// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Custom URL-scheme handler. WebKit only accepts a
//! `WKURLSchemeHandler` registered on the
//! `WKWebViewConfiguration` *before* the WKWebView is initialized;
//! [`super::WkWebViewProducer::new_with_url_schemes`] feeds them in
//! at construction time.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{
    MainThreadMarker, NSData, NSDictionary, NSHTTPURLResponse, NSObject, NSObjectProtocol,
    NSString, NSURL, NSURLResponse,
};
use objc2_web_kit::{WKURLSchemeHandler, WKURLSchemeTask, WKWebView};

pub use crate::{UrlSchemeHandlerFn, UrlSchemeResponse};

// `WKURLSchemeHandler` delegate class. Holds one handler closure
// keyed to one scheme; multiple schemes get multiple delegate
// instances. WebKit calls `webView:startURLSchemeTask:` on the main
// thread when a request for the registered scheme starts; we invoke
// the handler synchronously and feed the result back through the
// task's `didReceiveResponse:` / `didReceiveData:` / `didFinish`
// callbacks.
define_class!(
    // SAFETY:
    // - Superclass NSObject has no subclassing requirements.
    // - `SchemeHandler` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = UrlSchemeHandlerFn]
    pub(super) struct SchemeHandler;

    unsafe impl NSObjectProtocol for SchemeHandler {}

    unsafe impl WKURLSchemeHandler for SchemeHandler {
        #[unsafe(method(webView:startURLSchemeTask:))]
        fn start_task(
            &self,
            _webview: &WKWebView,
            url_scheme_task: &ProtocolObject<dyn WKURLSchemeTask>,
        ) {
            let request = unsafe { url_scheme_task.request() };
            let url = request.URL();
            let url_str = url
                .as_ref()
                .and_then(|u| u.absoluteString())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let response = (self.ivars())(&url_str);
            let url_for_response = match url {
                Some(u) => u,
                // Pathological — WebKit always supplies a URL. Drop
                // silently rather than over-engineer the error path
                // (NSError construction with userInfo would force
                // extra Foundation types into our dependency
                // surface).
                None => return,
            };
            let ns_response = build_http_response(&url_for_response, &response);
            // SAFETY: `bytes` outlives the `dataWithBytes_length`
            // call; NSData copies the bytes into its own buffer.
            let data = unsafe {
                NSData::dataWithBytes_length(
                    response.body.as_ptr() as *mut std::ffi::c_void,
                    response.body.len(),
                )
            };
            unsafe {
                url_scheme_task.didReceiveResponse(&ns_response);
                url_scheme_task.didReceiveData(&data);
                url_scheme_task.didFinish();
            }
        }

        #[unsafe(method(webView:stopURLSchemeTask:))]
        fn stop_task(
            &self,
            _webview: &WKWebView,
            _url_scheme_task: &ProtocolObject<dyn WKURLSchemeTask>,
        ) {
            // We complete tasks synchronously inside `start_task`,
            // so by the time `stopURLSchemeTask:` arrives there's
            // nothing to cancel. No-op.
        }
    }
);

impl SchemeHandler {
    pub(super) fn new(mtm: MainThreadMarker, handler: UrlSchemeHandlerFn) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(handler);
        unsafe { msg_send![super(this), init] }
    }
}

/// Build an `NSHTTPURLResponse` (or fall back to a plain
/// `NSURLResponse` if Apple's parser rejects the synthesized
/// header set). Status `200`, `Content-Type` from
/// `response.mime_type`, `Content-Length` from `response.body.len()`,
/// plus any caller-supplied headers (e.g. `Content-Disposition`).
///
/// The `NSHTTPURLResponse` path is what unlocks
/// `Content-Disposition: attachment` → WebKit's automatic
/// promote-to-download flow; before this slice the scheme handler
/// used a plain `NSURLResponse` which doesn't carry headers, so
/// served octet-stream payloads failed the navigation instead of
/// becoming downloads.
fn build_http_response(url: &NSURL, response: &UrlSchemeResponse) -> Retained<NSURLResponse> {
    let mut keys: Vec<Retained<NSString>> = Vec::with_capacity(2 + response.headers.len());
    let mut values: Vec<Retained<NSString>> = Vec::with_capacity(2 + response.headers.len());
    keys.push(NSString::from_str("Content-Type"));
    values.push(NSString::from_str(&response.mime_type));
    keys.push(NSString::from_str("Content-Length"));
    values.push(NSString::from_str(&response.body.len().to_string()));
    for (k, v) in &response.headers {
        keys.push(NSString::from_str(k));
        values.push(NSString::from_str(v));
    }
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let value_refs: Vec<&NSString> = values.iter().map(|v| &**v).collect();
    let header_dict = NSDictionary::from_slices(&key_refs, &value_refs);
    let http_version = NSString::from_str("HTTP/1.1");

    if let Some(http_response) = NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(
        NSHTTPURLResponse::alloc(),
        url,
        200,
        Some(&http_version),
        Some(&header_dict),
    ) {
        // SAFETY: `NSHTTPURLResponse` is an `NSURLResponse`
        // subclass per Apple's class hierarchy; objc2's
        // `cast_unchecked` upcasts via the runtime class chain.
        return unsafe { Retained::cast_unchecked(http_response) };
    }

    // Fallback: build a plain `NSURLResponse` if the HTTP
    // construction failed (shouldn't happen with the inputs above,
    // but the API returns Optional so we honor it).
    let mime_ns = NSString::from_str(&response.mime_type);
    let utf8 = NSString::from_str("utf-8");
    let length = response.body.len() as isize;
    NSURLResponse::initWithURL_MIMEType_expectedContentLength_textEncodingName(
        NSURLResponse::alloc(),
        url,
        Some(&mime_ns),
        length,
        Some(&utf8),
    )
}

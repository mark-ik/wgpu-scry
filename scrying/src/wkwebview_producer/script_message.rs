// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! `WKScriptMessageHandler` for the JS → host message bridge plus the
//! companion user script that exposes
//! `window.chrome.webview.postMessage` / `addEventListener('message',
//! ...)` shims (matching the WebView2 JS API) on top of WebKit's
//! `window.webkit.messageHandlers.<NAME>` machinery.
//!
//! Also home to the context-menu interception path: a separate user
//! script + `WKScriptMessageHandler` pair that captures
//! `contextmenu` events, suppresses WebKit's default `NSMenu`, and
//! pushes a [`crate::NavigationEvent::ContextMenuRequested`] event
//! onto the producer's nav-event queue. Apple's
//! `webView:contextMenuConfigurationForElement:completionHandler:`
//! is iOS-only — the macOS `WKUIDelegate` has no public hook for
//! context menus, so we go through the JS layer to stay clear of
//! `_WK*` SPI.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString};
use objc2_web_kit::{WKScriptMessage, WKScriptMessageHandler, WKUserContentController};

use super::nav_delegate::NavState;
use crate::{NavigationEvent, WebSurfaceEvent};

use super::nav_delegate::WebSurfaceEventQueue;

/// JS bridge handler name. JS-side code reaches the host via
/// `window.webkit.messageHandlers.<NAME>.postMessage(...)`. The shim
/// installed in [`HOST_BRIDGE_USER_SCRIPT`] wraps that under a
/// WebView2-compatible `window.chrome.webview` API so consumers can
/// write portable code.
pub(super) const HOST_BRIDGE_HANDLER_NAME: &str = "scryingHostBridge";

/// User-script injected at document start that builds a
/// `window.chrome.webview` shim around
/// `window.webkit.messageHandlers.scryingHostBridge.postMessage`.
///
/// The shim mirrors the WebView2 JS API:
///
/// - JS → host: `window.chrome.webview.postMessage(s)`.
/// - Host → JS: handlers registered via
///   `window.chrome.webview.addEventListener('message', cb)` are
///   invoked with `{data: payload}`. The host calls
///   `window.chrome.webview._dispatchHostMessage(payload)` via
///   `evaluateJavaScript:` (see [`super::WkWebViewProducer::post_web_message`]).
///
/// Idempotent: re-runs (e.g. after a same-document navigation) skip
/// if the shim is already present.
pub(super) const HOST_BRIDGE_USER_SCRIPT: &str = r#"(function() {
    if (window.chrome && window.chrome.webview && window.chrome.webview._dispatchHostMessage) return;
    var listeners = [];
    window.chrome = window.chrome || {};
    window.chrome.webview = {
        postMessage: function(msg) {
            window.webkit.messageHandlers.scryingHostBridge.postMessage(msg);
        },
        addEventListener: function(type, handler) {
            if (type === 'message') listeners.push(handler);
        },
        removeEventListener: function(type, handler) {
            if (type === 'message') {
                var i = listeners.indexOf(handler);
                if (i >= 0) listeners.splice(i, 1);
            }
        }
    };
    Object.defineProperty(window.chrome.webview, '_dispatchHostMessage', {
        value: function(payload) {
            var event = { data: payload, source: window.chrome.webview };
            for (var i = 0; i < listeners.length; i++) {
                try { listeners[i](event); } catch (e) { console.error(e); }
            }
        },
        enumerable: false,
        writable: false,
        configurable: false
    });
})();
"#;

pub(super) struct ScriptMessageHandlerState {
    pub(super) messages: Arc<Mutex<VecDeque<String>>>,
    pub(super) web_surface_events: WebSurfaceEventQueue,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `ScriptMessageHandler` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ScriptMessageHandlerState]
    pub(super) struct ScriptMessageHandler;

    unsafe impl NSObjectProtocol for ScriptMessageHandler {}

    // SAFETY: signature matches Apple's `WKScriptMessageHandler` protocol.
    unsafe impl WKScriptMessageHandler for ScriptMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _ucc: &WKUserContentController, message: &WKScriptMessage) {
            let body = unsafe { message.body() };
            // The trait contract is "string messages." JS senders that
            // need to pass structured payloads should `JSON.stringify`
            // host-side; non-string bodies are dropped here rather
            // than coerced to lossy string forms.
            if let Some(ns_string) = body.downcast_ref::<NSString>() {
                let message = ns_string.to_string();
                if let Ok(mut queue) = self.ivars().messages.lock() {
                    queue.push_back(message.clone());
                }
                self.ivars()
                    .web_surface_events
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push_back(WebSurfaceEvent::WebMessage(message));
            }
        }
    }
);

impl ScriptMessageHandler {
    pub(super) fn new(
        mtm: MainThreadMarker,
        messages: Arc<Mutex<VecDeque<String>>>,
        web_surface_events: WebSurfaceEventQueue,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ScriptMessageHandlerState {
            messages,
            web_surface_events,
        });
        // SAFETY: NSObject's `init` returns a valid initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Handler name for the context-menu intercept channel. The
/// companion user script in [`CONTEXT_MENU_USER_SCRIPT`] posts to
/// `window.webkit.messageHandlers.scryingContextMenu`.
pub(super) const CONTEXT_MENU_HANDLER_NAME: &str = "scryingContextMenu";

/// User script injected at document start that captures
/// `contextmenu` events at the capture phase, walks the click
/// target's ancestor chain to recover the closest enclosing
/// `<a href>` and `<img src>`, posts a NUL-delimited 5-field
/// payload to the [`CONTEXT_MENU_HANDLER_NAME`] handler, and
/// — by default — suppresses WebKit's engine-default `NSMenu`
/// via `event.preventDefault()`.
///
/// **Suppression is gated** on `window.__scryingSuppressContextMenu`:
///
/// - `undefined` (default at document start) → suppressed
/// - `false` → engine default menu is allowed to show alongside
///   the host event
/// - any other truthy value → suppressed
///
/// `WebSurfaceSettings::default_context_menus_enabled = Some(true)`
/// flips the flag to `false` via `evaluateJavaScript:` after
/// `apply_settings`. Observability (the message-handler payload)
/// fires regardless — the host always learns about the right-click,
/// only WebKit's default UI is conditional.
///
/// Payload format (NUL bytes between fields, no quoting): `page_url
/// \0 x \0 y \0 link_url \0 image_url`. Empty link / image fields
/// arrive as zero-length strings and decode to `Option::None` on
/// the host side. NUL is not a legal character in URLs or in
/// JavaScript number-to-string output, so the delimiter is
/// unambiguous without escaping — the matching parser lives in
/// [`ContextMenuMessageHandler::did_receive`].
///
/// Idempotent: re-runs (e.g. after a same-document navigation) do
/// nothing.
pub(super) const CONTEXT_MENU_USER_SCRIPT: &str = r#"(function() {
    if (window.__scryingContextMenuInstalled) return;
    window.__scryingContextMenuInstalled = true;
    document.addEventListener('contextmenu', function(e) {
        var linkUrl = '';
        var imageUrl = '';
        var node = e.target;
        while (node && node !== document) {
            if (!linkUrl && node.tagName === 'A' && node.href) {
                linkUrl = node.href;
            }
            if (!imageUrl && node.tagName === 'IMG' && node.src) {
                imageUrl = node.src;
            }
            if (linkUrl && imageUrl) break;
            node = node.parentNode;
        }
        var payload = [
            window.location.href,
            String(e.clientX),
            String(e.clientY),
            linkUrl,
            imageUrl,
        ].join('\u0000');
        try {
            window.webkit.messageHandlers.scryingContextMenu.postMessage(payload);
        } catch (_) { /* handler not yet wired */ }
        if (window.__scryingSuppressContextMenu !== false) {
            e.preventDefault();
        }
    }, true);
})();
"#;

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `ContextMenuMessageHandler` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = Arc<Mutex<NavState>>]
    pub(super) struct ContextMenuMessageHandler;

    unsafe impl NSObjectProtocol for ContextMenuMessageHandler {}

    // SAFETY: signature matches Apple's `WKScriptMessageHandler` protocol.
    unsafe impl WKScriptMessageHandler for ContextMenuMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(
            &self,
            _ucc: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            let body = unsafe { message.body() };
            let Some(ns_string) = body.downcast_ref::<NSString>() else {
                return;
            };
            let raw = ns_string.to_string();
            let parts: Vec<&str> = raw.split('\0').collect();
            if parts.len() != 5 {
                return;
            }
            let Ok(x) = parts[1].parse::<f64>() else { return; };
            let Ok(y) = parts[2].parse::<f64>() else { return; };
            let event = NavigationEvent::ContextMenuRequested {
                page_url: parts[0].to_string(),
                x,
                y,
                link_url: (!parts[3].is_empty()).then(|| parts[3].to_string()),
                image_url: (!parts[4].is_empty()).then(|| parts[4].to_string()),
            };
            if let Ok(mut state) = self.ivars().lock() {
                state.push_navigation_event(event);
            }
        }
    }
);

impl ContextMenuMessageHandler {
    pub(super) fn new(mtm: MainThreadMarker, nav_state: Arc<Mutex<NavState>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(nav_state);
        // SAFETY: NSObject's `init` returns a valid initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Handler name for the WebRTC capture-lifecycle channel.
pub(super) const MEDIA_CAPTURE_HANDLER_NAME: &str = "scryingMediaCapture";

/// User script that monkey-patches
/// `navigator.mediaDevices.getUserMedia` so the host can observe
/// when the page is actively capturing audio / video. WebKit fires
/// no public-API "camera in use" callback; the only hooks are the
/// permission-grant moment (already exposed via
/// [`crate::PermissionRequest`]) and inspecting active streams
/// from JS.
///
/// On each successful `getUserMedia` call the wrapper increments
/// per-kind track counters and posts the new totals. When a track
/// fires `ended` (user revoked permission, page called `track.stop()`,
/// device disconnected) the counters decrement and a fresh post
/// fires. The native side receives `audio:N,video:M` strings and
/// emits a [`crate::NavigationEvent::MediaCaptureStateChanged`]
/// event.
///
/// Caveats:
///
/// - Pages that replace `navigator.mediaDevices` or
///   `getUserMedia` *before* this script runs (we inject at
///   `AtDocumentStart`) escape the wrap. Document-end injection
///   would be more robust against late page-side replacement but
///   would miss the very-first capture call.
/// - Counters reset to zero on each top-level navigation
///   (the user-script re-runs and a new closure is installed).
pub(super) const MEDIA_CAPTURE_USER_SCRIPT: &str = r#"(function() {
    if (window.__scryingMediaCaptureInstalled) return;
    if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) return;
    window.__scryingMediaCaptureInstalled = true;
    var audioCount = 0, videoCount = 0;
    function notify() {
        try {
            window.webkit.messageHandlers.scryingMediaCapture.postMessage(
                'audio:' + audioCount + ',video:' + videoCount
            );
        } catch (_) { /* handler not yet wired */ }
    }
    var origGUM = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    navigator.mediaDevices.getUserMedia = function(constraints) {
        return origGUM(constraints).then(function(stream) {
            stream.getTracks().forEach(function(track) {
                if (track.kind === 'audio') audioCount++;
                else if (track.kind === 'video') videoCount++;
                track.addEventListener('ended', function() {
                    if (track.kind === 'audio') audioCount = Math.max(0, audioCount - 1);
                    else if (track.kind === 'video') videoCount = Math.max(0, videoCount - 1);
                    notify();
                });
            });
            notify();
            return stream;
        });
    };
})();
"#;

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `MediaCaptureMessageHandler` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = Arc<Mutex<NavState>>]
    pub(super) struct MediaCaptureMessageHandler;

    unsafe impl NSObjectProtocol for MediaCaptureMessageHandler {}

    // SAFETY: signature matches Apple's `WKScriptMessageHandler` protocol.
    unsafe impl WKScriptMessageHandler for MediaCaptureMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(
            &self,
            _ucc: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            let body = unsafe { message.body() };
            let Some(ns_string) = body.downcast_ref::<NSString>() else {
                return;
            };
            // Format: "audio:<u32>,video:<u32>" — fixed, generated
            // by the user script above. Bail on any deviation.
            let raw = ns_string.to_string();
            let mut audio: Option<u32> = None;
            let mut video: Option<u32> = None;
            for pair in raw.split(',') {
                let mut parts = pair.splitn(2, ':');
                let key = parts.next();
                let val = parts.next().and_then(|s| s.parse::<u32>().ok());
                match (key, val) {
                    (Some("audio"), Some(v)) => audio = Some(v),
                    (Some("video"), Some(v)) => video = Some(v),
                    _ => {}
                }
            }
            let (Some(audio), Some(video)) = (audio, video) else {
                return;
            };
            if let Ok(mut state) = self.ivars().lock() {
                state.push_navigation_event(NavigationEvent::MediaCaptureStateChanged {
                    audio_active_tracks: audio,
                    video_active_tracks: video,
                });
            }
        }
    }
);

impl MediaCaptureMessageHandler {
    pub(super) fn new(mtm: MainThreadMarker, nav_state: Arc<Mutex<NavState>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(nav_state);
        // SAFETY: NSObject's `init` returns a valid initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Handler name for the drag-and-drop observability channel.
pub(super) const DROP_HANDLER_NAME: &str = "scryingDrop";

/// User script that observes `drop` events at the capture phase
/// and reports drops with external content (files, URLs, images)
/// to the host. *Does not* call `event.preventDefault()` — the
/// page still gets the standard JS `drop` event and WKWebView's
/// default behavior (e.g. file → navigate) still runs. Pure
/// observability.
///
/// Drops originating from inside the page (e.g. a list-reorder
/// drag, where `dataTransfer` only carries internal types) are
/// filtered out so the host doesn't drown in intra-page noise.
/// The heuristic: at least one of (a) `files.length > 0`,
/// (b) `types` includes `text/uri-list`, (c) `types` has an
/// `image/*` MIME.
///
/// Payload (NUL-delimited 4 fields): `x \0 y \0 file_count \0
/// primary_url`. Empty primary_url field decodes to `None`. Idempotent
/// across same-document navigations.
pub(super) const DROP_USER_SCRIPT: &str = r#"(function() {
    if (window.__scryingDropInstalled) return;
    window.__scryingDropInstalled = true;
    document.addEventListener('drop', function(e) {
        if (!e.dataTransfer) return;
        var types = Array.from(e.dataTransfer.types || []);
        var hasFiles = e.dataTransfer.files && e.dataTransfer.files.length > 0;
        var hasUriList = types.indexOf('text/uri-list') !== -1;
        var hasImage = types.some(function(t) { return t.indexOf('image/') === 0; });
        if (!hasFiles && !hasUriList && !hasImage) return;
        var primaryUrl = '';
        var uriList = e.dataTransfer.getData('text/uri-list') || '';
        if (uriList) {
            var lines = uriList.split(/\r?\n/);
            for (var i = 0; i < lines.length; i++) {
                var line = lines[i];
                if (line && line.charAt(0) !== '#') {
                    primaryUrl = line;
                    break;
                }
            }
        }
        if (!primaryUrl) {
            // Some sources (Safari, some image drags) put the URL
            // in text/plain instead of text/uri-list.
            primaryUrl = e.dataTransfer.getData('text/plain') || '';
        }
        var payload = [
            String(e.clientX),
            String(e.clientY),
            String(e.dataTransfer.files ? e.dataTransfer.files.length : 0),
            primaryUrl,
        ].join('\u0000');
        try {
            window.webkit.messageHandlers.scryingDrop.postMessage(payload);
        } catch (_) { /* handler not yet wired */ }
    }, true);
})();
"#;

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `DropMessageHandler` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = Arc<Mutex<NavState>>]
    pub(super) struct DropMessageHandler;

    unsafe impl NSObjectProtocol for DropMessageHandler {}

    // SAFETY: signature matches Apple's `WKScriptMessageHandler` protocol.
    unsafe impl WKScriptMessageHandler for DropMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(
            &self,
            _ucc: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            let body = unsafe { message.body() };
            let Some(ns_string) = body.downcast_ref::<NSString>() else {
                return;
            };
            let raw = ns_string.to_string();
            let parts: Vec<&str> = raw.split('\0').collect();
            if parts.len() != 4 {
                return;
            }
            let Ok(x) = parts[0].parse::<f64>() else { return; };
            let Ok(y) = parts[1].parse::<f64>() else { return; };
            let Ok(file_count) = parts[2].parse::<u32>() else { return; };
            let event = NavigationEvent::DropDetected {
                x,
                y,
                file_count,
                primary_url: (!parts[3].is_empty()).then(|| parts[3].to_string()),
            };
            if let Ok(mut state) = self.ivars().lock() {
                state.push_navigation_event(event);
            }
        }
    }
);

impl DropMessageHandler {
    pub(super) fn new(mtm: MainThreadMarker, nav_state: Arc<Mutex<NavState>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(nav_state);
        // SAFETY: NSObject's `init` returns a valid initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}

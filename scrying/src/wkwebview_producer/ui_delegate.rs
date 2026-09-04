// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! `WKUIDelegate` implementation. Routes engine-level popups (item 2:
//! `NewWindowRequested`) into the producer's nav-event FIFO and
//! consults a host-registered permission handler for camera /
//! microphone / device-orientation requests.

use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol};
use objc2_web_kit::{
    WKFrameInfo, WKMediaCaptureType, WKNavigationAction, WKPermissionDecision, WKSecurityOrigin,
    WKUIDelegate, WKWebView, WKWebViewConfiguration, WKWindowFeatures,
};

use crate::{NavigationEvent, PermissionDecision, PermissionKind, PermissionRequest};

use super::nav_delegate::NavState;

/// Host-registered permission handler. Invoked synchronously inside
/// the UI delegate's `requestMediaCapturePermission*` /
/// `requestDeviceOrientationAndMotionPermission*` callbacks. `None`
/// makes the producer respond with `WKPermissionDecisionPrompt` so
/// WebKit / the OS shows its default UI.
pub type PermissionHandlerFn =
    Box<dyn Fn(PermissionRequest) -> PermissionDecision + Send + Sync + 'static>;

/// `UiDelegate`'s ivar struct. Holds the shared nav-event FIFO (for
/// `NewWindowRequested` events) and the optional permission handler.
pub(super) struct UiDelegateIvars {
    pub(super) state: Arc<Mutex<NavState>>,
    pub(super) permission_handler: Arc<Mutex<Option<PermissionHandlerFn>>>,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `UiDelegate` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = UiDelegateIvars]
    pub(super) struct UiDelegate;

    unsafe impl NSObjectProtocol for UiDelegate {}

    // SAFETY: signatures match Apple's `WKUIDelegate` protocol.
    // The `createWebView...` selector returns an autoreleased
    // `WKWebView *` object pointer; null means "cancel the
    // navigation" per Apple's docs. We model that as a `*mut WKWebView`
    // here because objc2's `define_class!` doesn't accept
    // `Option<Retained<...>>` as a method return type — the autorelease
    // semantics live on the Apple side, and we never hand back a
    // non-null pointer.
    unsafe impl WKUIDelegate for UiDelegate {
        /// Browser-shape consumers route popups through their own
        /// tab strip; we suppress the engine-level popup
        /// unconditionally and emit a `NewWindowRequested` nav
        /// event so the host can observe and decide. Returning a
        /// null pointer cancels the navigation.
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_webview(
            &self,
            _webview: &WKWebView,
            _configuration: &WKWebViewConfiguration,
            navigation_action: &WKNavigationAction,
            _window_features: &WKWindowFeatures,
        ) -> *mut WKWebView {
            let url = unsafe {
                navigation_action
                    .request()
                    .URL()
                    .and_then(|u| u.absoluteString())
            }
            .map(|s| s.to_string())
            .unwrap_or_default();
            if let Ok(mut state) = self.ivars().state.lock() {
                state.push_navigation_event(NavigationEvent::NewWindowRequested { url });
            }
            std::ptr::null_mut()
        }

        /// Camera / microphone / camera-and-microphone access
        /// request. Browser-shape consumers can drive the
        /// disposition by registering a permission handler via
        /// [`super::WkWebViewProducer::set_permission_handler`].
        /// With no handler we respond with
        /// `WKPermissionDecisionPrompt`, letting WebKit / the OS
        /// show its default UI.
        #[unsafe(method(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:))]
        fn request_media_permission(
            &self,
            _web_view: &WKWebView,
            origin: &WKSecurityOrigin,
            frame: &WKFrameInfo,
            r#type: WKMediaCaptureType,
            decision_handler: &block2::DynBlock<dyn Fn(WKPermissionDecision)>,
        ) {
            let kind = match r#type {
                WKMediaCaptureType::Camera => PermissionKind::Camera,
                WKMediaCaptureType::Microphone => PermissionKind::Microphone,
                WKMediaCaptureType::CameraAndMicrophone => PermissionKind::CameraAndMicrophone,
                _ => PermissionKind::CameraAndMicrophone,
            };
            let decision = self.consult_permission_handler(origin, frame, kind);
            decision_handler.call((decision,));
        }

        /// `DeviceMotionEvent` / `DeviceOrientationEvent`
        /// permission request. Same handler shape as media capture.
        #[unsafe(method(webView:requestDeviceOrientationAndMotionPermissionForOrigin:initiatedByFrame:decisionHandler:))]
        fn request_orientation_permission(
            &self,
            _web_view: &WKWebView,
            origin: &WKSecurityOrigin,
            frame: &WKFrameInfo,
            decision_handler: &block2::DynBlock<dyn Fn(WKPermissionDecision)>,
        ) {
            let decision =
                self.consult_permission_handler(origin, frame, PermissionKind::DeviceOrientation);
            decision_handler.call((decision,));
        }
    }
);

impl UiDelegate {
    pub(super) fn new(
        mtm: MainThreadMarker,
        nav_state: Arc<Mutex<NavState>>,
        permission_handler: Arc<Mutex<Option<PermissionHandlerFn>>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(UiDelegateIvars {
            state: nav_state,
            permission_handler,
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Translate a (origin, frame, kind) triple into a
    /// `WKPermissionDecision` by consulting the host's permission
    /// handler if one is registered. No handler → Prompt (default).
    fn consult_permission_handler(
        &self,
        origin: &WKSecurityOrigin,
        frame: &WKFrameInfo,
        kind: PermissionKind,
    ) -> WKPermissionDecision {
        let request_origin = security_origin_to_string(origin);
        let frame_url = unsafe { frame.request().URL() }
            .and_then(|u| u.absoluteString())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let host_decision = self
            .ivars()
            .permission_handler
            .lock()
            .ok()
            .and_then(|guard| {
                guard.as_ref().map(|f| {
                    f(PermissionRequest {
                        origin: request_origin,
                        frame_url,
                        kind,
                    })
                })
            });
        match host_decision {
            Some(PermissionDecision::Grant) => WKPermissionDecision::Grant,
            Some(PermissionDecision::Deny) => WKPermissionDecision::Deny,
            None | Some(PermissionDecision::Prompt) => WKPermissionDecision::Prompt,
        }
    }
}

/// Render a `WKSecurityOrigin` as a `scheme://host[:port]` string,
/// matching what JS-side code sees in `window.location.origin`.
fn security_origin_to_string(origin: &WKSecurityOrigin) -> String {
    let scheme = unsafe { origin.protocol() }.to_string();
    let host = unsafe { origin.host() }.to_string();
    let port = unsafe { origin.port() };
    if scheme.is_empty() && host.is_empty() {
        String::new()
    } else if port == 0 {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    }
}

// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! KVO observer registered against the WKWebView's `title` key path
//! so we can synthesize [`crate::NavigationEvent::TitleChanged`] events
//! whenever the page mutates `document.title` (the navigation
//! delegate's `didFinishNavigation:` only fires once per top-level
//! load).

use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{
    MainThreadMarker, NSDictionary, NSKeyValueChangeKey, NSObject, NSObjectProtocol, NSString,
};
use objc2_web_kit::WKWebView;

use crate::NavigationEvent;

use super::nav_delegate::NavState;

/// State the [`TitleObserver`] needs to look up a fresh title and
/// publish a `NavigationEvent`.
///
/// The observer holds a strong [`Retained<WKWebView>`] so the KVO
/// callback can read `webview.title()` directly without rebinding the
/// `object` parameter through `AnyObject` downcasts. The retain cycle
/// (WkWebViewProducer → TitleObserver → WKWebView) is broken in
/// `WkWebViewProducer::Drop` by calling `removeObserver:` before any
/// reference cascades.
pub(super) struct TitleObserverIvars {
    pub(super) nav_state: Arc<Mutex<NavState>>,
    pub(super) webview: Retained<WKWebView>,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `TitleObserver` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TitleObserverIvars]
    pub(super) struct TitleObserver;

    unsafe impl NSObjectProtocol for TitleObserver {}

    // SAFETY: KVO is invoked on the main thread because the WKWebView
    // is registered on the main thread (`addObserver:` is called
    // there) and AppKit / WebKit only mutate observable properties on
    // the main thread. Both the ivar `Arc<Mutex<...>>` and the
    // `Retained<WKWebView>` are therefore accessed from a single
    // thread.
    impl TitleObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            key_path: Option<&NSString>,
            _object: Option<&AnyObject>,
            _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
            _context: *mut std::ffi::c_void,
        ) {
            // Defensive in case the observer is ever registered for
            // multiple key paths. KVO fires rarely (once per title
            // change), so the small allocation here is fine.
            if key_path.map(|k| k.to_string()).as_deref() != Some("title") {
                return;
            }
            let ivars = self.ivars();
            let title = unsafe { ivars.webview.title() }
                .map(|s| s.to_string())
                .unwrap_or_default();
            if let Ok(mut state) = ivars.nav_state.lock() {
                state
                    .events
                    .push_back(NavigationEvent::TitleChanged { title });
            }
        }
    }
);

impl TitleObserver {
    pub(super) fn new(
        mtm: MainThreadMarker,
        nav_state: Arc<Mutex<NavState>>,
        webview: Retained<WKWebView>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TitleObserverIvars { nav_state, webview });
        unsafe { msg_send![super(this), init] }
    }
}

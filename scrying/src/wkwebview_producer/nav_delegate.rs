// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! `WKNavigationDelegate` implementation. Owns the nav-event FIFO
//! ([`NavState`]) shared with [`super::TitleObserver`] /
//! [`super::DownloadHandler`] / [`super::UiDelegate`], and the
//! optional host-driven auth-challenge handler.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{
    MainThreadMarker, NSError, NSObject, NSObjectProtocol, NSString, NSURLAuthenticationChallenge,
    NSURLCredential, NSURLCredentialPersistence, NSURLProtectionSpace,
    NSURLSessionAuthChallengeDisposition,
};
use objc2_web_kit::{
    WKDownload, WKNavigation, WKNavigationAction, WKNavigationDelegate, WKNavigationResponse,
    WKNavigationResponsePolicy, WKWebView,
};

use crate::{AuthChallenge, AuthDisposition, NavigationEvent, WebSurfaceEvent};

use super::download_handler::DownloadHandler;

#[derive(Default)]
pub(super) struct NavState {
    /// `Some(Ok(()))` on `didFinishNavigation:`, `Some(Err(message))`
    /// on `didFailNavigation:` / `didFailProvisionalNavigation:`,
    /// `None` while a navigation is in flight or before any has been
    /// started. Reset to `None` at the start of each
    /// `navigate_to_string` / `navigate_to_url` call.
    pub(super) result: Option<Result<(), String>>,
    /// FIFO of [`NavigationEvent`]s observed by [`NavDelegate`] but
    /// not yet drained by `poll_navigation_event`.
    pub(super) events: VecDeque<NavigationEvent>,
    /// The authoritative callback-order stream. The legacy navigation FIFO
    /// deliberately retains its own copy for existing consumers.
    pub(super) web_surface_events: WebSurfaceEventQueue,
}

pub(super) type WebSurfaceEventQueue = Arc<Mutex<VecDeque<WebSurfaceEvent>>>;

impl NavState {
    /// Record one native navigation callback in both public streams while the
    /// callback is still executing, preserving WebKit's delivery order.
    pub(super) fn push_navigation_event(&mut self, event: NavigationEvent) {
        self.events.push_back(event.clone());
        self.web_surface_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(WebSurfaceEvent::Navigation(event));
    }
}

/// Host-registered auth-challenge handler (item 6, option B).
/// Invoked synchronously inside the navigation delegate's
/// `webView:didReceiveAuthenticationChallenge:` callback. `None`
/// falls back to `PerformDefaultHandling` (option A).
pub type AuthHandlerFn = Box<dyn Fn(AuthChallenge) -> AuthDisposition + Send + Sync + 'static>;

/// `NavDelegate`'s ivar struct. The shared `NavState` is the
/// FIFO + completion-signal pair; `download_handler` is the strong
/// reference handed to each `WKDownload` we receive (WebKit holds
/// downloads' delegates weakly, so the strong ref has to live on the
/// producer's side); `auth_handler` is the optional host-registered
/// closure that drives auth-challenge dispositions (option B).
pub(super) struct NavDelegateIvars {
    pub(super) state: Arc<Mutex<NavState>>,
    pub(super) download_handler: Retained<DownloadHandler>,
    pub(super) auth_handler: Arc<Mutex<Option<AuthHandlerFn>>>,
}

/// Read the WKWebView's current committed URL as a String, falling
/// back to the empty string if WebKit hasn't populated `URL` yet
/// (e.g. inline-HTML loads with no `baseURL`).
pub(super) fn webview_url_string(web_view: &WKWebView) -> String {
    unsafe { web_view.URL() }
        .and_then(|url| url.absoluteString())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `NavDelegate` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = NavDelegateIvars]
    pub(super) struct NavDelegate;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for NavDelegate {}

    // SAFETY: `WKNavigationDelegate` requires only that the method
    // signatures match Apple's protocol. All callbacks land on the
    // main thread.
    unsafe impl WKNavigationDelegate for NavDelegate {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            let url = webview_url_string(web_view);
            if let Ok(mut state) = self.ivars().state.lock() {
                state.push_navigation_event(NavigationEvent::Starting { url });
            }
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            let url = webview_url_string(web_view);
            if let Ok(mut state) = self.ivars().state.lock() {
                state.push_navigation_event(NavigationEvent::SourceChanged { url });
            }
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            let url = webview_url_string(web_view);
            if let Ok(mut state) = self.ivars().state.lock() {
                state.push_navigation_event(NavigationEvent::Completed { url, success: true });
                state.result = Some(Ok(()));
            }
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(
            &self,
            web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let url = webview_url_string(web_view);
            let message = error.localizedDescription().to_string();
            if let Ok(mut state) = self.ivars().state.lock() {
                state.push_navigation_event(NavigationEvent::Completed {
                    url,
                    success: false,
                });
                state.result = Some(Err(message));
            }
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            let url = webview_url_string(web_view);
            let message = error.localizedDescription().to_string();
            if let Ok(mut state) = self.ivars().state.lock() {
                state.push_navigation_event(NavigationEvent::Completed {
                    url,
                    success: false,
                });
                state.result = Some(Err(message));
            }
        }

        /// The web content process backing this WebView terminated
        /// (typically a content-side crash). Surface this through
        /// the nav-event FIFO so the host can update its UI and
        /// trigger reload via `producer.reload()`.
        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn process_terminated(&self, _web_view: &WKWebView) {
            if let Ok(mut state) = self.ivars().state.lock() {
                state.push_navigation_event(NavigationEvent::ContentProcessTerminated);
            }
        }

        /// Convert non-displayable responses into downloads. Apple
        /// only fires `webView:navigationResponse:didBecomeDownload:`
        /// when *something* has decided the navigation should
        /// download — and with no host override, that "something"
        /// never fires for plain `application/octet-stream` /
        /// non-displayable MIME types. We bridge the gap by
        /// returning `WKNavigationResponsePolicyDownload` when
        /// `canShowMIMEType` is `false`, matching the behavior a
        /// browser-shape consumer would default to anyway.
        ///
        /// Pages WebKit *can* render (HTML, images, PDFs, etc.)
        /// flow through with the standard `Allow` policy.
        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn decide_policy_for_response(
            &self,
            _web_view: &WKWebView,
            navigation_response: &WKNavigationResponse,
            decision_handler: &block2::DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
        ) {
            let can_show = unsafe { navigation_response.canShowMIMEType() };
            let policy = if can_show {
                WKNavigationResponsePolicy::Allow
            } else {
                WKNavigationResponsePolicy::Download
            };
            decision_handler.call((policy,));
        }

        /// A navigation became a download (Content-Disposition,
        /// MIME type unsupported by WebKit, etc.). Hook the
        /// shared `DownloadHandler` as the download's delegate so
        /// `decideDestinationUsingResponse:` etc. fire.
        #[unsafe(method(webView:navigationResponse:didBecomeDownload:))]
        fn nav_response_did_become_download(
            &self,
            _web_view: &WKWebView,
            _response: &WKNavigationResponse,
            download: &WKDownload,
        ) {
            unsafe {
                download.setDelegate(Some(ProtocolObject::from_ref(
                    &*self.ivars().download_handler,
                )));
            }
        }

        /// `Cmd+click` save-as / download-link (rare). Same
        /// delegate hookup as the response-driven variant.
        #[unsafe(method(webView:navigationAction:didBecomeDownload:))]
        fn nav_action_did_become_download(
            &self,
            _web_view: &WKWebView,
            _action: &WKNavigationAction,
            download: &WKDownload,
        ) {
            unsafe {
                download.setDelegate(Some(ProtocolObject::from_ref(
                    &*self.ivars().download_handler,
                )));
            }
        }

        /// HTTP basic / server-trust / client-cert auth challenge.
        ///
        /// Two paths:
        ///
        /// - **Option A** (default, no handler registered): emit
        ///   `NavigationEvent::AuthChallenged` for logging and
        ///   respond with `PerformDefaultHandling` — WebKit falls
        ///   back to the system Keychain / interactive prompts.
        /// - **Option B** (handler registered via
        ///   `set_auth_handler`): invoke the handler closure
        ///   synchronously and translate its
        ///   [`AuthDisposition`] into the matching
        ///   `NSURLSessionAuthChallengeDisposition` +
        ///   `NSURLCredential`. The event is still emitted so the
        ///   host can log alongside its own decision.
        ///
        /// Calling the completion handler is mandatory — failing
        /// to invoke it would stall the load indefinitely.
        #[unsafe(method(webView:didReceiveAuthenticationChallenge:completionHandler:))]
        fn did_receive_auth_challenge(
            &self,
            web_view: &WKWebView,
            challenge: &NSURLAuthenticationChallenge,
            completion_handler: &block2::DynBlock<
                dyn Fn(NSURLSessionAuthChallengeDisposition, *mut NSURLCredential),
            >,
        ) {
            let url = webview_url_string(web_view);
            // WebKit hands us a `WKNSURLAuthenticationChallenge`
            // forwarding-proxy subclass that does NOT register
            // `protectionSpace` in its dispatch table — Apple's
            // `class_getInstanceMethod(...)` returns null for it,
            // which trips objc2's debug-build verification check on
            // both the typed `protectionSpace()` accessor and the
            // `msg_send!` macro. The selector still routes correctly
            // via `respondsToSelector:` + dynamic forwarding at
            // runtime, so we bypass the static-table check by
            // calling `objc_msgSend` directly. If the proxy
            // genuinely doesn't respond to the selector (defensive
            // case for future macOS changes), we report empty
            // protection-space fields and fall through to default
            // handling rather than panicking.
            let (host, auth_method, realm) = match read_protection_space(challenge) {
                Some(ps) => {
                    let host = ps.host().to_string();
                    let auth_method = ps.authenticationMethod().to_string();
                    let realm = ps.realm().map(|r| r.to_string()).unwrap_or_default();
                    (host, auth_method, realm)
                }
                None => (String::new(), String::new(), String::new()),
            };
            if let Ok(mut state) = self.ivars().state.lock() {
                state.push_navigation_event(NavigationEvent::AuthChallenged {
                    url: url.clone(),
                    host: host.clone(),
                    auth_method: auth_method.clone(),
                    source: crate::AuthSource::Page,
                });
            }

            // Option B path: consult the host's handler if registered.
            // The handler is called while holding the auth-handler
            // mutex, which serializes auth decisions and prevents the
            // host from racing a `set_auth_handler` against an
            // in-flight challenge.
            let disposition = if let Ok(guard) = self.ivars().auth_handler.lock() {
                guard.as_ref().map(|f| {
                    f(AuthChallenge {
                        url,
                        host,
                        auth_method,
                        realm,
                        source: crate::AuthSource::Page,
                    })
                })
            } else {
                None
            };

            match disposition {
                None | Some(AuthDisposition::PerformDefault) => {
                    completion_handler.call((
                        NSURLSessionAuthChallengeDisposition::PerformDefaultHandling,
                        std::ptr::null_mut(),
                    ));
                }
                Some(AuthDisposition::Cancel) => {
                    completion_handler.call((
                        NSURLSessionAuthChallengeDisposition::CancelAuthenticationChallenge,
                        std::ptr::null_mut(),
                    ));
                }
                Some(AuthDisposition::RejectProtectionSpace) => {
                    completion_handler.call((
                        NSURLSessionAuthChallengeDisposition::RejectProtectionSpace,
                        std::ptr::null_mut(),
                    ));
                }
                Some(AuthDisposition::UseCredential { username, password }) => {
                    let user_ns = NSString::from_str(&username);
                    let pass_ns = NSString::from_str(&password);
                    let credential = NSURLCredential::initWithUser_password_persistence(
                        NSURLCredential::alloc(),
                        &user_ns,
                        &pass_ns,
                        NSURLCredentialPersistence::ForSession,
                    );
                    completion_handler.call((
                        NSURLSessionAuthChallengeDisposition::UseCredential,
                        Retained::as_ptr(&credential) as *mut _,
                    ));
                }
            }
        }
    }
);

impl NavDelegate {
    pub(super) fn new(
        mtm: MainThreadMarker,
        state: Arc<Mutex<NavState>>,
        download_handler: Retained<DownloadHandler>,
        auth_handler: Arc<Mutex<Option<AuthHandlerFn>>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NavDelegateIvars {
            state,
            download_handler,
            auth_handler,
        });
        // SAFETY: NSObject's `init` returns a valid initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Read `[challenge protectionSpace]` while bypassing objc2's
/// debug-build static-method-table verification check.
///
/// `WKNSURLAuthenticationChallenge` (the WebKit-private subclass we
/// receive in the auth callback) doesn't list `protectionSpace` in
/// its own dispatch table. `class_getInstanceMethod` returns null,
/// which makes objc2's typed `protectionSpace()` accessor and the
/// `msg_send!` macro both panic in debug builds. The selector still
/// resolves correctly via Apple's runtime lookup at message-send
/// time, so we call `objc_msgSend` directly.
///
/// Returns `None` if either the runtime lookup fails (proxy
/// genuinely doesn't respond) or the call returns null. Caller
/// should treat both as "auth info unavailable, default-handle the
/// challenge."
pub(super) fn read_protection_space(
    challenge: &NSURLAuthenticationChallenge,
) -> Option<Retained<NSURLProtectionSpace>> {
    use objc2::ffi;
    use objc2::runtime::{AnyObject, Sel};
    use std::ptr::NonNull;

    let sel = objc2::sel!(protectionSpace);

    // Defensive: confirm the runtime says the receiver responds
    // before sending. `respondsToSelector:` walks Apple's normal
    // forwarding chain, so a `true` here promises the message-send
    // will reach an IMP.
    let responds: bool = unsafe { msg_send![challenge, respondsToSelector: sel] };
    if !responds {
        return None;
    }

    // Apple requires casting `objc_msgSend` to a function pointer of
    // the actual method's prototype before invoking — the symbol's
    // declared `()` signature is a stand-in for the real C-variadic
    // dispatcher.
    type Imp = unsafe extern "C" fn(*const AnyObject, Sel) -> *mut NSURLProtectionSpace;
    // SAFETY: `protectionSpace` returns an `NSURLProtectionSpace*` at
    // +0; the function pointer we transmute to matches that
    // signature. The receiver is a live `&NSURLAuthenticationChallenge`
    // bridged through `AnyObject` since objc_msgSend's first
    // argument is the generic `id` type.
    let imp: Imp = unsafe { std::mem::transmute(ffi::objc_msgSend as *const ()) };
    let receiver = challenge as *const NSURLAuthenticationChallenge as *const AnyObject;
    let raw = unsafe { imp(receiver, sel) };

    let non_null = NonNull::new(raw)?;
    // SAFETY: the result of `protectionSpace` is +0 (Apple's getter
    // convention for non-`init`/`copy`/`new`/`mutableCopy` methods),
    // so we retain to take ownership of a reference balanced against
    // our `Retained` Drop.
    unsafe { Retained::retain(non_null.as_ptr()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_callback_keeps_legacy_and_ordered_copies() {
        let mut state = NavState::default();
        state.push_navigation_event(NavigationEvent::Starting {
            url: "https://example.test".into(),
        });

        assert!(matches!(
            state.events.pop_front(),
            Some(NavigationEvent::Starting { url }) if url == "https://example.test"
        ));
        assert!(matches!(
            state.web_surface_events.lock().unwrap().pop_front(),
            Some(WebSurfaceEvent::Navigation(NavigationEvent::Starting { url }))
                if url == "https://example.test"
        ));
    }
}

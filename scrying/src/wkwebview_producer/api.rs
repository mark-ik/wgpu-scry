//! Inherent (non-trait, non-capture) public surface for the macOS
//! producer. Loads, async snapshots and a CPU-snapshot fallback,
//! find-in-page, PDF rendering, host-driven auth and permission
//! handler setters, cookie-store API, and `interactionState`
//! round-trip for tab restoration.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{
    MainThreadMarker, NSArray, NSData, NSError, NSHTTPCookie, NSKeyedArchiver, NSKeyedUnarchiver,
    NSString, NSURL, NSURLRequest,
};
use objc2_web_kit::{
    WKContentRuleList, WKContentRuleListStore, WKDownload, WKFindConfiguration, WKFindResult,
    WKHTTPCookieStore, WKPDFConfiguration,
};

use crate::{
    AuthChallenge, AuthDisposition, ColorPipeline, Cookie, DownloadDecision,
    DownloadDestinationRequest, PermissionDecision, PermissionRequest, WebSurfaceError,
};

use super::cookies::{cookie_from_ns, ns_cookie_from};
use super::producer::WkWebViewProducer;

/// Options for [`WkWebViewProducer::find_in_page`]. Mirrors the
/// fields of `WKFindConfiguration`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FindOptions {
    pub case_sensitive: bool,
    pub backwards: bool,
    pub wraps: bool,
}

impl WkWebViewProducer {
    /// Non-blocking variant of `navigate_to_url`. Invokes
    /// `WKWebView::loadRequest:` and returns immediately — the load
    /// completes asynchronously and surfaces through
    /// [`Self::poll_navigation_event`].
    ///
    /// **Use this** instead of [`navigate_to_url`](crate::WebSurfaceProducer::navigate_to_url)
    /// when calling from inside a host event-loop callback (e.g.
    /// winit's `resumed` / `window_event`). The blocking variant
    /// pumps the main `NSRunLoop` to wait for completion, which
    /// re-enters the event loop and panics under winit's
    /// "no nested event handling" guard.
    pub fn load_url(&self, url: &str) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "load_url must be called on the main thread".into(),
            ));
        }
        let url_ns = NSString::from_str(url);
        let ns_url = NSURL::URLWithString(&url_ns)
            .ok_or_else(|| WebSurfaceError::Platform(format!("could not parse URL: {url}")))?;
        let request = NSURLRequest::requestWithURL(&ns_url);
        unsafe { self.webview().loadRequest(&request) };
        Ok(())
    }

    /// Non-blocking variant of `navigate_to_string`. Invokes
    /// `WKWebView::loadHTMLString:` and returns immediately.
    /// Completion arrives through [`Self::poll_navigation_event`].
    ///
    /// See [`Self::load_url`] for when to prefer this over the
    /// blocking trait method.
    pub fn load_html(&self, html: &str) -> Result<(), WebSurfaceError> {
        self.load_html_inner(html, None)
    }

    /// Like [`Self::load_html`] but takes a base URL string. The
    /// inline HTML loads with that URL as its document origin —
    /// required for `document.cookie` (and any same-origin
    /// JavaScript API) to behave the same way it would for a real
    /// network load. Useful for cookie / storage persistence
    /// testing where the inline HTML wants to interact with the
    /// per-profile [`objc2_web_kit::WKWebsiteDataStore`].
    pub fn load_html_with_base_url(
        &self,
        html: &str,
        base_url: &str,
    ) -> Result<(), WebSurfaceError> {
        let url_ns = NSString::from_str(base_url);
        let parsed = NSURL::URLWithString(&url_ns).ok_or_else(|| {
            WebSurfaceError::Platform(format!("could not parse base URL: {base_url}"))
        })?;
        self.load_html_inner(html, Some(&parsed))
    }

    fn load_html_inner(&self, html: &str, base_url: Option<&NSURL>) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "load_html must be called on the main thread".into(),
            ));
        }
        let html_ns = NSString::from_str(html);
        unsafe { self.webview().loadHTMLString_baseURL(&html_ns, base_url) };
        Ok(())
    }

    /// Search the current document for `query` (non-blocking). The
    /// completion arrives as a `bool` (matched / didn't match)
    /// drained via [`Self::poll_find_match`]. WebKit's `findString:`
    /// is a navigation-and-highlight, not a match-counter — only one
    /// bit of information comes back. Browser chrome that wants
    /// `n of m` indicators would have to layer on JS-side counting.
    pub fn find_in_page(
        &mut self,
        query: &str,
        options: FindOptions,
    ) -> Result<(), WebSurfaceError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            WebSurfaceError::Platform("find_in_page must be called on the main thread".into())
        })?;
        let query_ns = NSString::from_str(query);
        let config = unsafe {
            let cfg = WKFindConfiguration::new(mtm);
            cfg.setBackwards(options.backwards);
            cfg.setCaseSensitive(options.case_sensitive);
            cfg.setWraps(options.wraps);
            cfg
        };
        let slot = Arc::clone(&self.pending_find);
        let block = RcBlock::new(move |result: NonNull<WKFindResult>| {
            let matched = unsafe { result.as_ref().matchFound() };
            if let Ok(mut s) = slot.lock() {
                *s = Some(matched);
            }
        });
        unsafe {
            self.webview()
                .findString_withConfiguration_completionHandler(&query_ns, Some(&config), &block);
        }
        Ok(())
    }

    /// Drain the most recent [`Self::find_in_page`] result.
    /// `Some(true)` if WebKit found at least one match,
    /// `Some(false)` if it didn't, `None` until the completion fires.
    pub fn poll_find_match(&mut self) -> Option<bool> {
        self.pending_find.lock().ok().and_then(|mut s| s.take())
    }

    /// Render the current document to PDF (non-blocking). The PDF
    /// bytes arrive via [`Self::poll_pdf`] when WebKit's
    /// `createPDFWithConfiguration:` completes. Useful for "save as
    /// PDF" / "export" / mere's print-preview path.
    pub fn request_pdf(&mut self) -> Result<(), WebSurfaceError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            WebSurfaceError::Platform("request_pdf must be called on the main thread".into())
        })?;
        let pdf_config = unsafe { WKPDFConfiguration::new(mtm) };
        let slot = Arc::clone(&self.pending_pdf);
        let block = RcBlock::new(move |data: *mut NSData, err: *mut NSError| {
            let result = if !err.is_null() {
                Err(unsafe { (*err).localizedDescription().to_string() })
            } else if data.is_null() {
                Err("PDF completion handler returned null data with no error".into())
            } else {
                let data: &NSData = unsafe { &*data };
                Ok(data.to_vec())
            };
            if let Ok(mut s) = slot.lock() {
                *s = Some(result);
            }
        });
        unsafe {
            self.webview()
                .createPDFWithConfiguration_completionHandler(Some(&pdf_config), &block);
        }
        Ok(())
    }

    /// Drain the most recent [`Self::request_pdf`] result. `Ok` is
    /// the encoded PDF bytes; `Err` is the localized error message.
    pub fn poll_pdf(&mut self) -> Option<Result<Vec<u8>, String>> {
        self.pending_pdf.lock().ok().and_then(|mut s| s.take())
    }

    /// Register a host-driven auth-challenge handler (browser-class
    /// item 6, option B). The closure runs synchronously on the
    /// main thread inside the navigation delegate's
    /// `webView:didReceiveAuthenticationChallenge:` callback; its
    /// returned [`AuthDisposition`] is translated into the matching
    /// `NSURLSessionAuthChallengeDisposition` + `NSURLCredential`.
    ///
    /// Replaces any previously-registered handler. While a handler
    /// is registered, [`crate::NavigationEvent::AuthChallenged`] events
    /// are still emitted alongside (so hosts can log every
    /// challenge regardless of how it was resolved).
    ///
    /// The handler must do its work quickly — it blocks WebKit's
    /// load pipeline until the closure returns. UI prompts that
    /// require interactive user input belong on a different
    /// surface (e.g. emit the event, suppress the engine prompt
    /// with `Cancel`, drive a separate UI, then re-trigger via
    /// JS). True async dispositions would require buffering the
    /// completion handler in an `RcBlock<...>` slot and making
    /// the host call back into the producer when the user has
    /// decided — out of scope for this slice.
    pub fn set_auth_handler<F>(&mut self, handler: F)
    where
        F: Fn(AuthChallenge) -> AuthDisposition + Send + Sync + 'static,
    {
        if let Ok(mut h) = self.auth_handler.lock() {
            *h = Some(Box::new(handler));
        }
    }

    /// Drop any registered auth handler, reverting to option-A
    /// default-handling behavior.
    pub fn clear_auth_handler(&mut self) {
        if let Ok(mut h) = self.auth_handler.lock() {
            *h = None;
        }
    }

    /// Register a permission handler for camera / microphone /
    /// device-orientation requests. The closure runs synchronously
    /// on the main thread inside the UI delegate's
    /// `requestMediaCapturePermission*` /
    /// `requestDeviceOrientationAndMotionPermission*` callbacks; its
    /// returned [`PermissionDecision`] is translated into the
    /// matching `WKPermissionDecision`.
    ///
    /// With no handler registered the producer responds with
    /// `Prompt` so WebKit / the OS shows the standard system UI.
    pub fn set_permission_handler<F>(&mut self, handler: F)
    where
        F: Fn(PermissionRequest) -> PermissionDecision + Send + Sync + 'static,
    {
        if let Ok(mut h) = self.permission_handler.lock() {
            *h = Some(Box::new(handler));
        }
    }

    /// Drop the registered permission handler, reverting all
    /// requests to the default `Prompt` disposition.
    pub fn clear_permission_handler(&mut self) {
        if let Ok(mut h) = self.permission_handler.lock() {
            *h = None;
        }
    }

    /// Register a host-driven destination handler for downloads.
    /// The closure runs synchronously on the main thread inside
    /// the WKDownload `decideDestination` callback; its returned
    /// [`DownloadDecision`] either accepts the download to a
    /// specific path or cancels it.
    ///
    /// With no handler registered, downloads land at
    /// `<config.download_dir>/<suggested_filename>` (with `-N`
    /// suffixing on collision).
    ///
    /// The handler must do its work quickly — it blocks WebKit's
    /// download pipeline until the closure returns. UI prompts
    /// (a Save As dialog) belong on a different surface: you'd
    /// register a handler that always returns `Cancel`, observe
    /// the resulting `DownloadCancelled` event, drive the host UI
    /// asynchronously, and re-trigger the download with the
    /// chosen path through whatever your re-fetch mechanism is.
    pub fn set_download_handler<F>(&mut self, handler: F)
    where
        F: Fn(DownloadDestinationRequest) -> DownloadDecision + Send + Sync + 'static,
    {
        if let Ok(mut h) = self.download_host_handler.lock() {
            *h = Some(Box::new(handler));
        }
    }

    /// Drop the registered download destination handler. Future
    /// downloads use the default `<config.download_dir>/<name>`
    /// policy.
    pub fn clear_download_handler(&mut self) {
        if let Ok(mut h) = self.download_host_handler.lock() {
            *h = None;
        }
    }

    /// Register a host-driven cursor-change handler. The closure
    /// runs synchronously on the main thread inside
    /// [`Self::send_mouse_input`] / [`Self::send_pointer_input`]
    /// every time `NSCursor.currentSystemCursor` reports a
    /// different shape from the previous observation.
    ///
    /// The pull-model [`Self::poll_cursor_shape`] keeps working
    /// regardless — both surfaces fire on the same change, so a
    /// host can mix and match. Useful for hosts that prefer
    /// callbacks for cursor changes (e.g., to set the host
    /// window's cursor immediately) but still want to drain other
    /// per-frame state via polling.
    pub fn set_cursor_handler<F>(&mut self, handler: F)
    where
        F: Fn(crate::CursorShape) + Send + Sync + 'static,
    {
        if let Ok(mut h) = self.cursor_handler.lock() {
            *h = Some(Box::new(handler));
        }
    }

    /// Drop the registered cursor handler. Future cursor changes
    /// will only surface through [`Self::poll_cursor_shape`].
    pub fn clear_cursor_handler(&mut self) {
        if let Ok(mut h) = self.cursor_handler.lock() {
            *h = None;
        }
    }

    /// Programmatically start a download from a URL, bypassing the
    /// usual navigation→download promotion path. Useful for browser
    /// chrome that wants to expose a "Download Linked File" / "Save
    /// Image" affordance without navigating the WKWebView.
    ///
    /// Auth challenges hit the *download-level* delegate callback
    /// (`WKDownloadDelegate::download:didReceiveAuthenticationChallenge:`)
    /// rather than the page-level one — there's no page navigation
    /// to challenge against. The host's
    /// [`Self::set_auth_handler`] applies to both paths, so a
    /// single registered handler covers programmatic and
    /// promotion-driven downloads identically.
    ///
    /// Returns immediately. The download begins asynchronously;
    /// the eventual `WKDownload` is wired to the same delegate as
    /// promotion-driven downloads, so `DownloadStarted` /
    /// `DownloadProgress` / `DownloadFinished` /
    /// `DownloadCancelled` events fire normally and a fresh
    /// [`crate::DownloadId`] is allocated when `decideDestination`
    /// runs.
    pub fn start_download(&mut self, url: &str) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "start_download must be called on the main thread".into(),
            ));
        }
        let url_ns = NSString::from_str(url);
        let ns_url = NSURL::URLWithString(&url_ns)
            .ok_or_else(|| WebSurfaceError::Platform(format!("could not parse URL: {url}")))?;
        let request = NSURLRequest::requestWithURL(&ns_url);

        // Wrap the `Retained<DownloadHandler>` so the closure can
        // satisfy `Send + 'static`. The completion handler runs on
        // the main thread (Apple's WKWebView APIs do), and the
        // delegate is `MainThreadOnly`, so the cross-thread
        // assertion is trivially satisfied — the wrapper exists
        // only to satisfy the conservative trait bounds on
        // `RcBlock`.
        struct SendDelegate(Retained<super::download_handler::DownloadHandler>);
        // SAFETY: see comment above.
        unsafe impl Send for SendDelegate {}
        let delegate = SendDelegate(self.download_handler_strong.clone());

        let block = RcBlock::new(move |download: NonNull<WKDownload>| {
            // SAFETY: WebKit hands us a +0 reference valid for the
            // duration of the completion-handler call; `setDelegate:`
            // doesn't outlive the borrow because WebKit's internal
            // ref-keeping is what matters past this call.
            let download_ref = unsafe { download.as_ref() };
            unsafe {
                download_ref.setDelegate(Some(ProtocolObject::from_ref(&*delegate.0)));
            }
        });

        unsafe {
            self.webview()
                .startDownloadUsingRequest_completionHandler(&request, &block);
        }
        Ok(())
    }

    /// Resume a previously-cancelled download from the
    /// `resume_data` blob WebKit captured in the matching
    /// [`crate::NavigationEvent::DownloadCancelled`] event.
    ///
    /// Wraps `WKWebView::resumeDownloadFromResumeData:completionHandler:`.
    /// On success a fresh [`crate::DownloadId`] is allocated and
    /// the resumed transfer fires its own
    /// `DownloadStarted` / `DownloadProgress` / `DownloadFinished`
    /// events; on failure (corrupt blob, server no longer accepts
    /// the resume request, etc.) a `DownloadFinished` with
    /// `error: Some(_)` lands instead.
    ///
    /// Returns immediately. The resumed download is wired to the
    /// same [`super::download_handler::DownloadHandler`] as
    /// promotion-driven and `start_download`-initiated transfers,
    /// so the existing event surface covers it without any
    /// special casing.
    pub fn resume_download(
        &mut self,
        resume_data: &[u8],
        destination_path: std::path::PathBuf,
    ) -> Result<crate::DownloadId, WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "resume_download must be called on the main thread".into(),
            ));
        }
        if resume_data.is_empty() {
            return Err(WebSurfaceError::Platform(
                "resume_download called with empty resume_data".into(),
            ));
        }
        // SAFETY: `resume_data` outlives the `dataWithBytes_length`
        // call; NSData copies into its own buffer.
        let ns_data = unsafe {
            NSData::dataWithBytes_length(
                resume_data.as_ptr() as *mut std::ffi::c_void,
                resume_data.len(),
            )
        };

        // Allocate the resumed download's id up-front so the
        // caller can correlate the eventual `DownloadFinished`
        // event before WebKit creates its `WKDownload`. WebKit
        // skips `decideDestination` for resumed downloads (the
        // destination is already encoded in the resume_data
        // plist), so we register the entry here in
        // `resume_download` rather than in the delegate.
        let id = self.allocate_download_id();

        // Emit a `DownloadStarted` event synchronously so hosts
        // listening on the nav-event FIFO see the resume kick
        // off. `total_bytes_expected: None` because the resumed
        // request returns 206 Partial Content with a
        // remaining-bytes content-length, not the full file
        // size; expressing it as "we'll see when the bytes
        // land" matches WebKit's actual behavior.
        if let Ok(mut state) = self.nav_state.lock() {
            state
                .events
                .push_back(crate::NavigationEvent::DownloadStarted {
                    id,
                    url: String::new(),
                    suggested_filename: destination_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    destination_path: destination_path.clone(),
                    total_bytes_expected: None,
                });
        }

        struct SendBundle {
            delegate: Retained<super::download_handler::DownloadHandler>,
            registry: Arc<Mutex<super::download_handler::DownloadRegistry>>,
            destination_path: std::path::PathBuf,
            id: crate::DownloadId,
        }
        // SAFETY: see `start_download` for the same wrapper rationale.
        unsafe impl Send for SendBundle {}
        let bundle = SendBundle {
            delegate: self.download_handler_strong.clone(),
            registry: Arc::clone(&self.download_registry),
            destination_path,
            id,
        };

        let block = RcBlock::new(move |download: NonNull<WKDownload>| {
            // SAFETY: WebKit hands us a +0 reference valid for
            // the duration of the completion-handler call;
            // retaining extends the lifetime to match our
            // registry entry.
            let download_ref = unsafe { download.as_ref() };
            let download_strong = unsafe { Retained::retain(NonNull::from(download_ref).as_ptr()) }
                .expect("Retained::retain on resumed WKDownload returned None");

            let pointer_key = Retained::as_ptr(&download_strong) as usize;
            if let Ok(mut registry) = bundle.registry.lock() {
                registry.by_pointer.insert(pointer_key, bundle.id);
                registry.by_id.insert(
                    bundle.id,
                    super::download_handler::DownloadEntry {
                        id: bundle.id,
                        destination_path: bundle.destination_path.clone(),
                        // Resume's 206 Partial Content response
                        // reports remaining bytes, not full size.
                        // Resumed transfers don't have a clean
                        // upper bound exposed up-front; consumers
                        // see file-size-on-completion via the
                        // final progress event.
                        total_bytes_expected: None,
                        wk_download: download_strong,
                        last_progress_emit: std::time::Instant::now(),
                        last_progress_bytes: 0,
                        cancelled_by_host: false,
                    },
                );
            }

            unsafe {
                download_ref.setDelegate(Some(ProtocolObject::from_ref(&*bundle.delegate)));
            }
        });

        unsafe {
            self.webview()
                .resumeDownloadFromResumeData_completionHandler(&ns_data, &block);
        }
        Ok(id)
    }

    /// Allocate a fresh [`crate::DownloadId`] from the producer's
    /// shared atomic counter. Used by `resume_download` (which
    /// can't wait for `decideDestination` to do this for it,
    /// since WebKit skips that callback for resumed downloads).
    fn allocate_download_id(&self) -> crate::DownloadId {
        self.download_id_allocator.next()
    }

    /// Cancel an in-flight download by [`DownloadId`]. Returns
    /// `Ok(true)` if the ID matched an active download (a
    /// [`crate::NavigationEvent::DownloadCancelled`] event will
    /// follow shortly), `Ok(false)` if the ID is unknown — either
    /// because the download already completed / failed and was
    /// pruned from the registry, or because it was never issued.
    ///
    /// Cancellation drops any partial bytes; resume support is a
    /// future slice (would surface the `resumeData` from
    /// `WKDownload::cancel(_:)` and add a corresponding restart
    /// API).
    pub fn cancel_download(&mut self, id: crate::DownloadId) -> Result<bool, WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "cancel_download must be called on the main thread".into(),
            ));
        }
        let (download, destination_path) = {
            let mut registry = self
                .download_registry
                .lock()
                .map_err(|_| WebSurfaceError::Platform("download registry lock poisoned".into()))?;
            let Some(entry) = registry.by_id.get(&id) else {
                return Ok(false);
            };
            let download = entry.wk_download.clone();
            let dest = entry.destination_path.clone();
            // Remove the registry entry now. Apple's `cancel:` with
            // a non-nil completion block deliberately suppresses
            // `didFailWithError:resumeData:` on the delegate (per
            // their docs: "Once the cancel: completion block is
            // called, you don't receive any further messages
            // about the canceled download"), so the delegate's
            // `didFail` cleanup path won't fire for this entry —
            // we have to prune it here.
            let pointer_key = Retained::as_ptr(&download) as usize;
            registry.by_pointer.remove(&pointer_key);
            registry.by_id.remove(&id);
            (download, dest)
        };

        // The cancel completion block IS the path the
        // `DownloadCancelled` event flows through (didFail is
        // suppressed by Apple — see comment above). We capture
        // the producer's nav-event FIFO + the entry data into
        // the closure and emit the event when WebKit signals the
        // cancel-with-resume-data is ready.
        let nav_state = Arc::clone(&self.nav_state);
        let block = RcBlock::new(move |resume_data: *mut NSData| {
            let resume_bytes = if resume_data.is_null() {
                None
            } else {
                let data: &NSData = unsafe { &*resume_data };
                let v = data.to_vec();
                if v.is_empty() { None } else { Some(v) }
            };
            if let Ok(mut state) = nav_state.lock() {
                state
                    .events
                    .push_back(crate::NavigationEvent::DownloadCancelled {
                        id,
                        destination_path: destination_path.clone(),
                        resume_data: resume_bytes,
                    });
            }
        });
        unsafe { download.cancel(Some(&block)) };
        Ok(true)
    }

    /// Kick off an async fetch of every cookie in the
    /// `WKHTTPCookieStore` backing this producer's
    /// `WKWebsiteDataStore`. Drain via [`Self::poll_cookies`].
    /// Useful for browser-shape consumers running an "import from
    /// Safari" / "show all cookies" / "sync cookies to native
    /// password manager" flow.
    pub fn request_all_cookies(&mut self) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "request_all_cookies must be called on the main thread".into(),
            ));
        }
        let store = unsafe { self.cookie_store() };
        let slot = Arc::clone(&self.pending_cookies);
        let block = RcBlock::new(move |array_ptr: NonNull<NSArray<NSHTTPCookie>>| {
            let array = unsafe { array_ptr.as_ref() };
            let mut out = Vec::with_capacity(array.count());
            for i in 0..array.count() {
                let ns_cookie = array.objectAtIndex(i);
                out.push(cookie_from_ns(&ns_cookie));
            }
            if let Ok(mut s) = slot.lock() {
                *s = Some(out);
            }
        });
        unsafe { store.getAllCookies(&block) };
        Ok(())
    }

    /// Drain the most recent [`Self::request_all_cookies`] result.
    pub fn poll_cookies(&mut self) -> Option<Vec<Cookie>> {
        self.pending_cookies.lock().ok().and_then(|mut s| s.take())
    }

    /// Set / overwrite a cookie in the producer's data store.
    /// Fire-and-forget — Apple's API takes a completion handler with
    /// no value; we don't expose a poll for it. The cookie is
    /// visible to subsequent network requests once the data-store
    /// has committed it.
    pub fn set_cookie(&mut self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "set_cookie must be called on the main thread".into(),
            ));
        }
        if cookie.same_site.is_some() {
            return Err(WebSurfaceError::Unsupported(
                "WKHTTPCookieStore path does not expose SameSite setters in scrying yet",
            ));
        }
        if cookie.partitioned {
            return Err(WebSurfaceError::Unsupported(
                "WKHTTPCookieStore path does not expose Partitioned cookie setters in scrying yet",
            ));
        }
        let ns_cookie = ns_cookie_from(cookie).ok_or_else(|| {
            WebSurfaceError::Platform(
                "could not construct NSHTTPCookie — required field (name/value/domain/path) was rejected by the cookie parser".into(),
            )
        })?;
        let store = unsafe { self.cookie_store() };
        unsafe { store.setCookie_completionHandler(&ns_cookie, None) };
        Ok(())
    }

    /// Delete a cookie by name + domain + path. Constructs an
    /// `NSHTTPCookie` with the same identity (name/domain/path/value
    /// don't matter for the lookup beyond those three) and hands
    /// it to `WKHTTPCookieStore::deleteCookie:`. Fire-and-forget.
    pub fn delete_cookie(
        &mut self,
        name: &str,
        domain: &str,
        path: &str,
    ) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "delete_cookie must be called on the main thread".into(),
            ));
        }
        let placeholder = Cookie {
            name: name.to_string(),
            value: String::new(),
            domain: domain.to_string(),
            path: path.to_string(),
            expires_at: None,
            is_secure: false,
            is_http_only: false,
            same_site: None,
            partitioned: false,
        };
        let ns_cookie = ns_cookie_from(&placeholder).ok_or_else(|| {
            WebSurfaceError::Platform("could not construct NSHTTPCookie for deletion".into())
        })?;
        let store = unsafe { self.cookie_store() };
        unsafe { store.deleteCookie_completionHandler(&ns_cookie, None) };
        Ok(())
    }

    /// Register a callback that fires whenever *anything* mutates
    /// the producer's cookie store: page-side `document.cookie`
    /// writes, `Set-Cookie` response headers, host calls to
    /// [`Self::set_cookie`] / [`Self::delete_cookie`].
    ///
    /// Apple's `WKHTTPCookieStoreObserver::cookiesDidChangeInCookieStore:`
    /// protocol delivers no delta — the callback is a "go re-fetch"
    /// pulse. Pair with [`Self::request_all_cookies`] /
    /// [`Self::poll_cookies`] to observe the new state.
    ///
    /// Replaces any prior handler. Fires on the main thread.
    /// Browser-shape consumers use this to keep their own
    /// chrome / status indicators in sync with auth-flow cookie
    /// writes without polling.
    pub fn set_cookie_change_handler(
        &mut self,
        handler: super::cookie_observer::CookieChangeHandlerFn,
    ) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "set_cookie_change_handler must be called on the main thread".into(),
            ));
        }
        let mut slot = self
            .cookie_change_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("cookie_change_handler lock poisoned".into()))?;
        *slot = Some(handler);
        Ok(())
    }

    /// Compile a `WKContentRuleList` JSON rule list and attach it
    /// to the producer's `WKUserContentController`. Fire-and-forget:
    /// compile happens asynchronously on a WebKit-private queue,
    /// then the completion handler attaches the resulting list to
    /// the UCC on the main thread. Errors are logged to stderr —
    /// for a typical browser-class consumer, content blocking
    /// failing to load is non-fatal (rules are best-effort), and
    /// surfacing per-compile success / failure would clutter the
    /// API surface.
    ///
    /// `identifier` is a stable name for the rule list; Apple's
    /// store caches the *compiled* output on disk under it, so
    /// calling this method again with the same identifier and
    /// (likely) the same JSON is fast — WebKit re-loads the
    /// pre-compiled blob rather than re-parsing.
    ///
    /// `encoded_json` is the AdBlock-shape rule list (an array of
    /// `{"trigger": ..., "action": ...}` objects). See Apple's
    /// `WKContentRuleList` documentation for the schema. Invalid
    /// JSON or unsupported actions surface as a logged error and
    /// no list attachment.
    ///
    /// To replace an applied rule list, compile under the same
    /// identifier with new JSON; to remove all attached lists, use
    /// [`Self::clear_all_content_rule_lists`].
    pub fn compile_and_apply_content_rule_list(
        &mut self,
        identifier: &str,
        encoded_json: &str,
    ) -> Result<(), WebSurfaceError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            WebSurfaceError::Platform(
                "compile_and_apply_content_rule_list must be called on the main thread".into(),
            )
        })?;
        let store = unsafe { WKContentRuleListStore::defaultStore(mtm) }.ok_or_else(|| {
            WebSurfaceError::Platform("WKContentRuleListStore::defaultStore returned nil".into())
        })?;
        let identifier_owned = identifier.to_string();
        let identifier_ns = NSString::from_str(identifier);
        let json_ns = NSString::from_str(encoded_json);
        let ucc = unsafe { self.webview().configuration().userContentController() };
        let block = RcBlock::new(move |list: *mut WKContentRuleList, err: *mut NSError| {
            if !err.is_null() {
                let msg = unsafe { (*err).localizedDescription() }.to_string();
                eprintln!(
                    "scrying: WKContentRuleList compile failed for {identifier_owned:?}: {msg}"
                );
                return;
            }
            let Some(list_ptr) = std::ptr::NonNull::new(list) else {
                eprintln!(
                    "scrying: WKContentRuleList compile returned nil with no error for {identifier_owned:?}"
                );
                return;
            };
            // SAFETY: WebKit hands us a +0 borrow; retain so
            // the WKContentRuleList outlives this completion
            // (the UCC will hold its own retain through
            // `addContentRuleList`, but we don't strictly own
            // a strong ref otherwise).
            let Some(retained) = (unsafe { Retained::retain(list_ptr.as_ptr()) }) else {
                eprintln!(
                    "scrying: Retained::retain on WKContentRuleList returned None for {identifier_owned:?}"
                );
                return;
            };
            unsafe { ucc.addContentRuleList(&retained) };
        });
        unsafe {
            store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
                Some(&identifier_ns),
                Some(&json_ns),
                Some(&block),
            );
        }
        Ok(())
    }

    /// Switch the SCK capture path's color pipeline live. Updates
    /// `WkWebViewProducerConfig::color_pipeline`, then pushes a
    /// fresh `SCStreamConfiguration` through the same
    /// `update_capture_for_layout_change` path that resize / DPI-
    /// flip use. The capture-revision gate dropping in-flight
    /// pre-change samples means the consumer transitions cleanly:
    /// frames stop briefly, SCK's completion handler fires, then
    /// frames resume at the new color pipeline.
    ///
    /// No-op if the requested pipeline matches the current one.
    /// No-op when capture isn't live; the next
    /// `start_capture` / `start_capture_async` will pick up the
    /// new value from the config.
    pub fn set_color_pipeline(&mut self, pipeline: ColorPipeline) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "set_color_pipeline must be called on the main thread".into(),
            ));
        }
        if self.config.color_pipeline == pipeline {
            return Ok(());
        }
        self.config.color_pipeline = pipeline;
        // `update_capture_for_layout_change` is a misnomer at this
        // point — it's the generic "re-push SCStreamConfiguration
        // and bump the revision counter" path, used for resize,
        // DPI flips, and now color-pipeline changes. Renaming it
        // would churn touch sites for no semantic gain.
        self.update_capture_for_layout_change();
        Ok(())
    }

    /// Present the standard macOS print panel for this WebView's
    /// document and run the user's chosen print operation. Blocks
    /// the calling thread (which must be the main thread) until
    /// the user clicks Print or Cancel; returns `true` on actual
    /// print, `false` on cancel.
    ///
    /// This is the interactive `Cmd+P` equivalent — distinct from
    /// the headless [`Self::request_pdf`] / [`Self::poll_pdf`]
    /// path, which renders to a PDF blob without UI. Browser-shape
    /// consumers usually want both: `print` for the user-facing
    /// menu item, `request_pdf` for "Save as PDF" via a host-
    /// rendered file dialog.
    ///
    /// Uses `NSPrintInfo::sharedPrintInfo` for the default settings
    /// (paper size, margins). Hosts that need a customized
    /// `NSPrintInfo` can shadow this method by calling
    /// `webview.printOperationWithPrintInfo:` directly via the
    /// objc2-web-kit binding.
    pub fn print(&mut self) -> Result<bool, WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "print must be called on the main thread".into(),
            ));
        }
        let info = objc2_app_kit::NSPrintInfo::sharedPrintInfo();
        let op = unsafe { self.webview().printOperationWithPrintInfo(&info) };
        Ok(op.runOperation())
    }

    /// Detach every `WKContentRuleList` previously compiled and
    /// attached via [`Self::compile_and_apply_content_rule_list`].
    /// Synchronous — the WKUserContentController call returns
    /// immediately. Apple's content-rule-list *store* keeps the
    /// compiled blobs on disk regardless; this only undoes the
    /// per-WKWebView attachment.
    pub fn clear_all_content_rule_lists(&mut self) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "clear_all_content_rule_lists must be called on the main thread".into(),
            ));
        }
        let ucc = unsafe { self.webview().configuration().userContentController() };
        unsafe { ucc.removeAllContentRuleLists() };
        Ok(())
    }

    /// Clear the cookie-change callback. The
    /// `WKHTTPCookieStoreObserver` registration stays in place
    /// (cheap to keep around for the producer's lifetime); this
    /// just unsets the closure so subsequent
    /// `cookiesDidChangeInCookieStore:` callbacks are no-ops.
    pub fn clear_cookie_change_handler(&mut self) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "clear_cookie_change_handler must be called on the main thread".into(),
            ));
        }
        let mut slot = self
            .cookie_change_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("cookie_change_handler lock poisoned".into()))?;
        *slot = None;
        Ok(())
    }

    /// Serialize the WebView's interaction state — back-forward
    /// list, scroll position, form data, etc. — into an opaque
    /// blob. Round-trip via [`Self::restore_interaction_state`]
    /// to restore. Useful for browser-shape consumers persisting
    /// per-tab session state.
    ///
    /// Returns `None` if the WebView has no state to serialize
    /// (e.g. before any navigation). The blob format is private
    /// to WebKit; treat it as opaque bytes.
    pub fn serialize_interaction_state(&self) -> Option<Vec<u8>> {
        MainThreadMarker::new()?;
        let state = unsafe { self.webview().interactionState() }?;
        // The deprecated archiver pair (no `requiringSecureCoding`)
        // round-trips correctly for the WebKit-internal types in
        // the interaction state. The modern path requires
        // class-allowlist work that's brittle for opaque blobs.
        #[allow(deprecated)]
        let data = unsafe { NSKeyedArchiver::archivedDataWithRootObject(&state) };
        Some(data.to_vec())
    }

    /// Restore a previously-serialized interaction state. The blob
    /// must come from [`Self::serialize_interaction_state`] called
    /// on a `WkWebViewProducer` running compatible WebKit; cross-
    /// version restore is allowed but not guaranteed by Apple.
    pub fn restore_interaction_state(&mut self, bytes: &[u8]) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "restore_interaction_state must be called on the main thread".into(),
            ));
        }
        let data = unsafe { NSData::dataWithBytes_length(bytes.as_ptr() as *mut _, bytes.len()) };
        #[allow(deprecated)]
        let obj = unsafe { NSKeyedUnarchiver::unarchiveObjectWithData(&data) }
            .ok_or_else(|| {
                WebSurfaceError::Platform(
                    "could not unarchive interaction state — blob may be corrupt or from an incompatible WebKit version".into(),
                )
            })?;
        unsafe { self.webview().setInteractionState(Some(&*obj)) };
        Ok(())
    }

    /// Reach the producer's `WKHTTPCookieStore` via the live
    /// `WKWebsiteDataStore` on its configuration. The store is
    /// cheap to fetch each call (returns a wrapper around a shared
    /// underlying store) and we don't cache it on the producer to
    /// avoid lifetime entanglement.
    unsafe fn cookie_store(&self) -> Retained<WKHTTPCookieStore> {
        unsafe {
            let config = self.webview().configuration();
            let data_store = config.websiteDataStore();
            data_store.httpCookieStore()
        }
    }
}

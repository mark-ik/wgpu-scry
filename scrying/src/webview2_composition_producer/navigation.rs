// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

impl WebView2CompositionProducer {
    /// Navigate the underlying WebView2 to an inline HTML document and block
    /// until `NavigationCompleted` fires (or the configured timeout elapses).
    pub fn navigate_to_string(&self, html: &str, timeout: Duration) -> Result<(), WebSurfaceError> {
        let (tx, rx) = mpsc::channel::<()>();
        let mut navigation_token = 0;
        let handler = NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
            let _ = tx.send(());
            Ok(())
        }));

        unsafe {
            self.webview
                .add_NavigationCompleted(&handler, &mut navigation_token)
                .map_err(platform("add_NavigationCompleted"))?;
            let html = CoTaskMemPWSTR::from(html);
            self.webview
                .NavigateToString(*html.as_ref().as_pcwstr())
                .map_err(platform("NavigateToString"))?;
        }

        let result = pump_until(timeout, &rx);

        unsafe {
            let _ = self
                .webview
                .remove_NavigationCompleted(navigation_token)
                .map_err(webview2_com::Error::WindowsError);
        }

        result.map_err(|()| {
            WebSurfaceError::Platform(format!(
                "WebView2 navigation did not complete within {timeout:?}"
            ))
        })?;

        self.wait_for_render_tick()
    }

    fn wait_for_render_tick(&self) -> Result<(), WebSurfaceError> {
        let script = r#"(() => new Promise(resolve => {
            requestAnimationFrame(() => requestAnimationFrame(() => resolve("present")));
        }))()"#
            .to_string();
        execute_script_blocking(&self.webview, script)
    }

    /// Navigate the underlying WebView2 to a URL and block until
    /// `NavigationCompleted` fires (or the timeout elapses).
    pub fn navigate_to_url(&self, url: &str, timeout: Duration) -> Result<(), WebSurfaceError> {
        let (tx, rx) = mpsc::channel::<()>();
        let mut navigation_token = 0;
        let handler = NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
            let _ = tx.send(());
            Ok(())
        }));

        unsafe {
            self.webview
                .add_NavigationCompleted(&handler, &mut navigation_token)
                .map_err(platform("add_NavigationCompleted (navigate_to_url)"))?;
            let url = CoTaskMemPWSTR::from(url);
            self.webview
                .Navigate(*url.as_ref().as_pcwstr())
                .map_err(platform("Navigate"))?;
        }

        let result = pump_until(timeout, &rx);

        unsafe {
            let _ = self
                .webview
                .remove_NavigationCompleted(navigation_token)
                .map_err(webview2_com::Error::WindowsError);
        }

        result.map_err(|()| {
            WebSurfaceError::Platform(format!(
                "WebView2 navigation did not complete within {timeout:?}"
            ))
        })?;

        self.wait_for_render_tick()
    }

    /// Drain the next pending [`NavigationEvent`] from the producer's queue.
    pub fn poll_navigation_event(&self) -> Option<NavigationEvent> {
        self.nav_event_queue.lock().ok()?.pop_front()
    }

    /// Post a string message into `window.chrome.webview` for the page's
    /// `addEventListener("message", ...)` handlers to consume.
    pub fn post_web_message(&self, message: &str) -> Result<(), WebSurfaceError> {
        let message = CoTaskMemPWSTR::from(message);
        unsafe {
            self.webview
                .PostWebMessageAsString(*message.as_ref().as_pcwstr())
                .map_err(platform("PostWebMessageAsString"))
        }
    }

    /// Fire a Chrome DevTools Protocol method without waiting for its result.
    pub fn call_devtools_protocol_method(
        &self,
        method: &str,
        params_json: &str,
    ) -> Result<(), WebSurfaceError> {
        let method = CoTaskMemPWSTR::from(method);
        let params = CoTaskMemPWSTR::from(params_json);
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())));
        unsafe {
            self.webview
                .CallDevToolsProtocolMethod(
                    *method.as_ref().as_pcwstr(),
                    *params.as_ref().as_pcwstr(),
                    &handler,
                )
                .map_err(platform("CallDevToolsProtocolMethod"))
        }
    }

    /// Fire a Chrome DevTools Protocol method and block until WebView2 reports
    /// completion or `timeout` elapses.
    pub fn call_devtools_protocol_method_blocking(
        &self,
        method: &str,
        params_json: &str,
        timeout: Duration,
    ) -> Result<String, WebSurfaceError> {
        let method_name = method.to_string();
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
            move |result: windows::core::Result<()>, json_result: String| {
                let payload = result
                    .map(|()| json_result)
                    .map_err(|error| error.message().to_string());
                let _ = tx.send(payload);
                Ok(())
            },
        ));

        let method = CoTaskMemPWSTR::from(method);
        let params = CoTaskMemPWSTR::from(params_json);
        unsafe {
            self.webview
                .CallDevToolsProtocolMethod(
                    *method.as_ref().as_pcwstr(),
                    *params.as_ref().as_pcwstr(),
                    &handler,
                )
                .map_err(platform("CallDevToolsProtocolMethod"))?;
        }

        let deadline = Instant::now() + timeout;
        loop {
            match rx.try_recv() {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error)) => return Err(WebSurfaceError::Platform(error)),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(WebSurfaceError::Platform(
                        "CallDevToolsProtocolMethod completion channel disconnected".into(),
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(WebSurfaceError::Platform(format!(
                    "CallDevToolsProtocolMethod {method_name:?} did not complete within {timeout:?}"
                )));
            }
            pump_messages_for(Duration::from_millis(16));
        }
    }

    /// Execute JavaScript in the WebView2 page and block for its JSON result.
    pub fn execute_script_with_result(
        &self,
        script: &str,
        timeout: Duration,
    ) -> Result<String, WebSurfaceError> {
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        let handler = ExecuteScriptCompletedHandler::create(Box::new(
            move |result: windows::core::Result<()>, json_result: String| {
                let payload = result
                    .map(|()| json_result)
                    .map_err(|error| error.message().to_string());
                let _ = tx.send(payload);
                Ok(())
            },
        ));

        let script = CoTaskMemPWSTR::from(script);
        unsafe {
            self.webview
                .ExecuteScript(*script.as_ref().as_pcwstr(), &handler)
                .map_err(platform("ExecuteScript"))?;
        }

        let deadline = Instant::now() + timeout;
        loop {
            match rx.try_recv() {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error)) => return Err(WebSurfaceError::Platform(error)),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(WebSurfaceError::Platform(
                        "ExecuteScript completion channel disconnected".into(),
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(WebSurfaceError::Platform(format!(
                    "ExecuteScript did not complete within {timeout:?}"
                )));
            }
            pump_messages_for(Duration::from_millis(16));
        }
    }

    /// Drain the next pending message posted from JS via
    /// `window.chrome.webview.postMessage(...)`.
    pub fn poll_web_message(&self) -> Option<String> {
        self.web_message_queue.lock().ok()?.pop_front()
    }

    /// Reload the current page.
    pub fn reload(&self) -> Result<(), WebSurfaceError> {
        unsafe { self.webview.Reload() }.map_err(platform("Reload"))
    }

    /// Stop the current navigation, if any.
    pub fn stop(&self) -> Result<(), WebSurfaceError> {
        unsafe { self.webview.Stop() }.map_err(platform("Stop"))
    }

    /// Navigate one entry back in session history.
    pub fn go_back(&self) -> Result<bool, WebSurfaceError> {
        if !self.can_go_back() {
            return Ok(false);
        }
        unsafe { self.webview.GoBack() }.map_err(platform("GoBack"))?;
        Ok(true)
    }

    /// Navigate one entry forward in session history.
    pub fn go_forward(&self) -> Result<bool, WebSurfaceError> {
        if !self.can_go_forward() {
            return Ok(false);
        }
        unsafe { self.webview.GoForward() }.map_err(platform("GoForward"))?;
        Ok(true)
    }

    /// Whether the back stack currently has at least one entry.
    pub fn can_go_back(&self) -> bool {
        let mut value = windows::core::BOOL::default();
        unsafe { self.webview.CanGoBack(&mut value) }
            .ok()
            .map(|()| value.as_bool())
            .unwrap_or(false)
    }

    /// Whether the forward stack currently has at least one entry.
    pub fn can_go_forward(&self) -> bool {
        let mut value = windows::core::BOOL::default();
        unsafe { self.webview.CanGoForward(&mut value) }
            .ok()
            .map(|()| value.as_bool())
            .unwrap_or(false)
    }

    pub fn serialize_interaction_state(&self) -> Option<Vec<u8>> {
        None
    }

    pub fn restore_interaction_state(&mut self, _bytes: &[u8]) -> Result<(), WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WebView2 exposes navigation history controls but no opaque tab interaction-state blob equivalent to WKWebView",
        ))
    }

    pub fn load_url(&self, url: &str) -> Result<(), WebSurfaceError> {
        let url = CoTaskMemPWSTR::from(url);
        unsafe { self.webview.Navigate(*url.as_ref().as_pcwstr()) }
            .map_err(platform("Navigate (load_url)"))
    }

    pub fn load_html(&self, html: &str) -> Result<(), WebSurfaceError> {
        let html = CoTaskMemPWSTR::from(html);
        unsafe { self.webview.NavigateToString(*html.as_ref().as_pcwstr()) }
            .map_err(platform("NavigateToString (load_html)"))
    }
}

pub(super) fn register_persistent_handlers(
    webview: &ICoreWebView2,
    nav_queue: Arc<Mutex<VecDeque<NavigationEvent>>>,
    web_message_queue: Arc<Mutex<VecDeque<String>>>,
    cookie_change_handler: Arc<Mutex<Option<WebView2CookieChangeHandlerFn>>>,
) -> Result<(i64, i64, i64, i64, i64, i64, i64), WebSurfaceError> {
    let queue = nav_queue.clone();
    let nav_starting_handler = NavigationStartingEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let mut uri = PWSTR::null();
            if unsafe { args.Uri(&mut uri) }.is_ok() {
                let url = unsafe { consume_pwstr(uri) };
                if let Ok(mut q) = queue.lock() {
                    q.push_back(NavigationEvent::TextInputBlurred);
                    q.push_back(NavigationEvent::Starting { url });
                }
            }
        }
        Ok(())
    }));
    let mut nav_starting_token = 0i64;
    unsafe {
        webview
            .add_NavigationStarting(&nav_starting_handler, &mut nav_starting_token)
            .map_err(platform("add_NavigationStarting"))?;
    }

    let queue = nav_queue.clone();
    let webview_for_handler = webview.clone();
    let nav_completed_handler =
        NavigationCompletedEventHandler::create(Box::new(move |_, args| {
            let success = args
                .as_ref()
                .and_then(|a| {
                    let mut b = windows::core::BOOL::default();
                    unsafe { a.IsSuccess(&mut b) }.ok().map(|()| b.as_bool())
                })
                .unwrap_or(false);
            let mut source = PWSTR::null();
            let url = if unsafe { webview_for_handler.Source(&mut source) }.is_ok() {
                unsafe { consume_pwstr(source) }
            } else {
                String::new()
            };
            if let Ok(mut q) = queue.lock() {
                q.push_back(NavigationEvent::Completed { url, success });
            }
            Ok(())
        }));
    let mut nav_completed_token = 0i64;
    unsafe {
        webview
            .add_NavigationCompleted(&nav_completed_handler, &mut nav_completed_token)
            .map_err(platform("add_NavigationCompleted (persistent)"))?;
    }

    let queue = nav_queue.clone();
    let source_changed_handler = SourceChangedEventHandler::create(Box::new(move |sender, _| {
        let Some(webview) = sender else { return Ok(()) };
        let mut source = PWSTR::null();
        let url = if unsafe { webview.Source(&mut source) }.is_ok() {
            unsafe { consume_pwstr(source) }
        } else {
            String::new()
        };
        if let Ok(mut q) = queue.lock() {
            q.push_back(NavigationEvent::SourceChanged { url });
        }
        Ok(())
    }));
    let mut source_changed_token = 0i64;
    unsafe {
        webview
            .add_SourceChanged(&source_changed_handler, &mut source_changed_token)
            .map_err(platform("add_SourceChanged"))?;
    }

    let queue = nav_queue.clone();
    let title_changed_handler =
        DocumentTitleChangedEventHandler::create(Box::new(move |sender, _| {
            let Some(webview) = sender else { return Ok(()) };
            let mut title = PWSTR::null();
            let title = if unsafe { webview.DocumentTitle(&mut title) }.is_ok() {
                unsafe { consume_pwstr(title) }
            } else {
                String::new()
            };
            if let Ok(mut q) = queue.lock() {
                q.push_back(NavigationEvent::TitleChanged { title });
            }
            Ok(())
        }));
    let mut title_changed_token = 0i64;
    unsafe {
        webview
            .add_DocumentTitleChanged(&title_changed_handler, &mut title_changed_token)
            .map_err(platform("add_DocumentTitleChanged"))?;
    }

    let queue = nav_queue.clone();
    let new_window_handler = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let mut uri = PWSTR::null();
            if unsafe { args.Uri(&mut uri) }.is_ok() {
                let url = unsafe { consume_pwstr(uri) };
                if let Ok(mut q) = queue.lock() {
                    q.push_back(NavigationEvent::NewWindowRequested { url: url.clone() });
                }
            }
            unsafe { args.SetHandled(true)? };
        }
        Ok(())
    }));
    let mut new_window_requested_token = 0i64;
    unsafe {
        webview
            .add_NewWindowRequested(&new_window_handler, &mut new_window_requested_token)
            .map_err(platform("add_NewWindowRequested"))?;
    }

    let queue = nav_queue.clone();
    let process_failed_handler = ProcessFailedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND(0);
            if unsafe { args.ProcessFailedKind(&mut kind) }.is_ok()
                && is_content_process_failure(kind)
                && let Ok(mut q) = queue.lock()
            {
                q.push_back(NavigationEvent::TextInputBlurred);
                q.push_back(NavigationEvent::ContentProcessTerminated);
            }
        }
        Ok(())
    }));
    let mut process_failed_token = 0i64;
    unsafe {
        webview
            .add_ProcessFailed(&process_failed_handler, &mut process_failed_token)
            .map_err(platform("add_ProcessFailed"))?;
    }

    let queue = web_message_queue;
    let nav_queue_for_messages = nav_queue.clone();
    let cookie_handler = cookie_change_handler;
    let web_message_handler = WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let mut message = PWSTR::null();
            if unsafe { args.TryGetWebMessageAsString(&mut message) }.is_ok() {
                let s = unsafe { consume_pwstr(message) };
                if s == COOKIE_CHANGE_BRIDGE_MESSAGE {
                    if let Ok(slot) = cookie_handler.lock()
                        && let Some(handler) = slot.as_ref()
                    {
                        handler();
                    }
                    return Ok(());
                }
                if let Some(event) = browser::parse_context_menu_bridge_message(&s) {
                    if let Ok(mut q) = nav_queue_for_messages.lock() {
                        q.push_back(event);
                    }
                    return Ok(());
                }
                if let Some(event) = browser::parse_drop_detected_bridge_message(&s) {
                    if let Ok(mut q) = nav_queue_for_messages.lock() {
                        q.push_back(event);
                    }
                    return Ok(());
                }
                if let Some(event) = browser::parse_media_capture_bridge_message(&s) {
                    if let Ok(mut q) = nav_queue_for_messages.lock() {
                        q.push_back(event);
                    }
                    return Ok(());
                }
                if let Some(event) = browser::parse_text_input_bridge_message(&s) {
                    if let Ok(mut q) = nav_queue_for_messages.lock() {
                        q.push_back(event);
                    }
                    return Ok(());
                }
                if let Ok(mut q) = queue.lock() {
                    q.push_back(s);
                }
            }
        }
        Ok(())
    }));
    let mut web_message_token = 0i64;
    unsafe {
        webview
            .add_WebMessageReceived(&web_message_handler, &mut web_message_token)
            .map_err(platform("add_WebMessageReceived"))?;
    }

    Ok((
        nav_starting_token,
        nav_completed_token,
        source_changed_token,
        title_changed_token,
        new_window_requested_token,
        process_failed_token,
        web_message_token,
    ))
}

fn is_content_process_failure(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> bool {
    matches!(
        kind,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
            | COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE
            | COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED
            | COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED
    )
}

//! [`WebSurfaceProducer`] trait implementation. Bridges the
//! cross-platform trait surface onto the macOS-specific machinery in
//! [`super::producer::WkWebViewProducer`] / sibling submodules.

use std::time::Instant;

use dpi::PhysicalSize;
use objc2_app_kit::{NSEvent, NSEventType};
use objc2_foundation::{
    MainThreadMarker, NSDate, NSDefaultRunLoopMode, NSPoint, NSRunLoop, NSString, NSURL,
    NSURLRequest,
};

use crate::{
    CursorShape, DragInput, FocusReason, KeyEventKind, KeyboardInput, MouseEventKind, MouseInput,
    MouseVirtualKeys, NavigationEvent, PointerDevice, PointerEventKind, PointerInput,
    WebSurfaceCapabilities, WebSurfaceError, WebSurfaceFrame, WebSurfaceProducer,
    WebSurfaceSettings,
};

use super::helpers::{js_string_literal, key_modifier_flags};
use super::input::{MouseTarget, synthesize_mouse_event};
use super::producer::WkWebViewProducer;

impl WebSurfaceProducer for WkWebViewProducer {
    fn capabilities(&self) -> WebSurfaceCapabilities {
        self.capabilities.clone()
    }

    fn acquire_frame(&mut self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        if self.capture.is_none() {
            // Slice-A behavior: capture has not been started, so the
            // WKWebView remains a platform overlay child and the
            // consumer composites it itself.
            return Ok(WebSurfaceFrame::OverlayOnly);
        }

        // Capture is live: pump the run loop up to `frame_timeout`
        // waiting for the next sample buffer.
        let timeout = self.config.frame_timeout;
        let start = Instant::now();
        let run_loop = NSRunLoop::currentRunLoop();
        loop {
            if let Some(frame) = self.try_acquire_frame()? {
                return Ok(frame);
            }
            if start.elapsed() >= timeout {
                return Err(WebSurfaceError::NotReady(
                    "no SCStream sample arrived within frame_timeout",
                ));
            }
            let until = NSDate::dateWithTimeIntervalSinceNow(0.008);
            let _ = run_loop.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &until);
        }
    }

    fn try_acquire_frame(&mut self) -> Result<Option<WebSurfaceFrame>, WebSurfaceError> {
        WkWebViewProducer::try_acquire_frame(self)
    }

    fn load_html(&mut self, html: &str) -> Result<(), WebSurfaceError> {
        WkWebViewProducer::load_html(self, html)
    }

    fn load_url(&mut self, url: &str) -> Result<(), WebSurfaceError> {
        WkWebViewProducer::load_url(self, url)
    }

    fn navigate_to_string(
        &mut self,
        html: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "navigate_to_string must be called on the main thread".into(),
            ));
        }

        self.reset_nav_result()?;

        let html_ns = NSString::from_str(html);
        unsafe {
            self.webview().loadHTMLString_baseURL(&html_ns, None);
        }

        self.wait_for_nav_completion(timeout, "navigate_to_string")
    }

    fn navigate_to_url(
        &mut self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "navigate_to_url must be called on the main thread".into(),
            ));
        }

        let url_ns = NSString::from_str(url);
        let ns_url = NSURL::URLWithString(&url_ns)
            .ok_or_else(|| WebSurfaceError::Platform(format!("could not parse URL: {url}")))?;
        let request = NSURLRequest::requestWithURL(&ns_url);

        self.reset_nav_result()?;
        unsafe {
            self.webview().loadRequest(&request);
        }
        self.wait_for_nav_completion(timeout, "navigate_to_url")
    }

    fn set_cookie(&mut self, cookie: &crate::Cookie) -> Result<(), WebSurfaceError> {
        WkWebViewProducer::set_cookie(self, cookie)
    }

    fn move_focus(&mut self, _reason: FocusReason) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "move_focus must be called on the main thread".into(),
            ));
        }
        let window = self.webview().window().ok_or_else(|| {
            WebSurfaceError::Platform(
                "WKWebView is not in a window — move_focus requires the parent NSView to be embedded in an NSWindow".into(),
            )
        })?;
        // WKWebView ↘ NSView ↘ NSResponder. AppKit's `FocusReason`
        // distinctions (Programmatic / Next / Previous) don't have a
        // direct analog on macOS — `selectKeyViewFollowingView:` /
        // `selectPreviousKeyView:` exist but Cocoa's keyloop is wired
        // in the responder chain, not via WKWebView. Slice C treats
        // every `FocusReason` as a programmatic focus move and lets
        // the host handle keyloop separately.
        let made_first = window.makeFirstResponder(Some(self.webview()));
        if !made_first {
            return Err(WebSurfaceError::Platform(
                "NSWindow rejected makeFirstResponder for the WKWebView".into(),
            ));
        }
        Ok(())
    }

    fn poll_navigation_event(&mut self) -> Option<NavigationEvent> {
        self.nav_state
            .lock()
            .ok()
            .and_then(|mut state| state.events.pop_front())
    }

    fn reload(&mut self) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "reload must be called on the main thread".into(),
            ));
        }
        unsafe {
            self.webview().reload();
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "stop must be called on the main thread".into(),
            ));
        }
        unsafe {
            self.webview().stopLoading();
        }
        Ok(())
    }

    fn go_back(&mut self) -> Result<bool, WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "go_back must be called on the main thread".into(),
            ));
        }
        if !unsafe { self.webview().canGoBack() } {
            return Ok(false);
        }
        unsafe {
            self.webview().goBack();
        }
        Ok(true)
    }

    fn go_forward(&mut self) -> Result<bool, WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "go_forward must be called on the main thread".into(),
            ));
        }
        if !unsafe { self.webview().canGoForward() } {
            return Ok(false);
        }
        unsafe {
            self.webview().goForward();
        }
        Ok(true)
    }

    fn can_go_back(&self) -> bool {
        unsafe { self.webview().canGoBack() }
    }

    fn can_go_forward(&self) -> bool {
        unsafe { self.webview().canGoForward() }
    }

    fn set_visible(&mut self, visible: bool) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "set_visible must be called on the main thread".into(),
            ));
        }
        // `NSView::setHidden:` cascades to the page-side Page
        // Visibility API: WebKit observes the
        // `viewDidHide` / `viewDidUnhide` chain (or its KVO
        // equivalent on `isHidden`) and pushes `visibilitychange`
        // events with `document.hidden` flipped accordingly. No
        // `_WK*` SPI involved.
        self.webview().setHidden(!visible);
        Ok(())
    }

    fn apply_settings(&mut self, settings: &WebSurfaceSettings) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "apply_settings must be called on the main thread".into(),
            ));
        }
        if let Some(zoom) = settings.zoom_factor {
            unsafe {
                self.webview().setPageZoom(zoom);
            }
        }
        if let Some(ua) = settings.user_agent.as_ref() {
            let ua_ns = NSString::from_str(ua);
            unsafe {
                self.webview().setCustomUserAgent(Some(&ua_ns));
            }
        }
        if let Some(devtools) = settings.devtools_enabled {
            // `isInspectable` is the macOS 13.3+ API. Pre-13.3 the
            // inspector is enabled via SPI (`_setInspectable:`) which
            // is out of scope; on those OS versions the call is a
            // no-op as far as we're concerned.
            unsafe {
                self.webview().setInspectable(devtools);
            }
        }
        if let Some(js) = settings.javascript_enabled {
            unsafe {
                let config = self.webview().configuration();
                let prefs = config.preferences();
                #[allow(deprecated)]
                prefs.setJavaScriptEnabled(js);
            }
        }
        if let Some(policy) = settings.inactive_scheduling_policy {
            // `inactiveSchedulingPolicy` was added in macOS 14 /
            // iOS 17. Calling it on older OS versions sends an
            // unrecognized selector and crashes; gate via
            // `respondsToSelector:` rather than a runtime
            // version check (cheaper, equivalent semantically).
            // The selector is `setInactiveSchedulingPolicy:`.
            unsafe {
                use objc2::sel;
                let config = self.webview().configuration();
                let prefs = config.preferences();
                let prefs_obj: &objc2::runtime::AnyObject = prefs.as_ref();
                if prefs_obj
                    .class()
                    .responds_to(sel!(setInactiveSchedulingPolicy:))
                {
                    let value = match policy {
                        crate::InactiveSchedulingPolicy::Suspend => {
                            objc2_web_kit::WKInactiveSchedulingPolicy::Suspend
                        }
                        crate::InactiveSchedulingPolicy::Throttle => {
                            objc2_web_kit::WKInactiveSchedulingPolicy::Throttle
                        }
                        crate::InactiveSchedulingPolicy::None => {
                            objc2_web_kit::WKInactiveSchedulingPolicy::None
                        }
                    };
                    prefs.setInactiveSchedulingPolicy(value);
                }
                // Older macOS / iOS: silent no-op. Documented at
                // the field on `WebSurfaceSettings`.
            }
        }
        if let Some(menus_enabled) = settings.default_context_menus_enabled {
            // The producer's context-menu user-script
            // (CONTEXT_MENU_USER_SCRIPT) gates its
            // `event.preventDefault()` on
            // `window.__scryingSuppressContextMenu`. Default
            // (undefined) → suppressed; setting `false` lets
            // WebKit's engine-default `NSMenu` show. Observability
            // events fire either way.
            //
            // `evaluateJavaScript:` writes to *the current
            // page* — survives same-document navigations but
            // resets on top-level loads. The user-script's own
            // initialisation guard means a subsequent load gets
            // the default-suppress behavior again unless the
            // host re-applies the setting after `Completed`.
            let js = if menus_enabled {
                "window.__scryingSuppressContextMenu = false;"
            } else {
                "delete window.__scryingSuppressContextMenu;"
            };
            let js_ns = NSString::from_str(js);
            unsafe {
                self.webview()
                    .evaluateJavaScript_completionHandler(&js_ns, None);
            }
        }
        // `builtin_accelerator_keys_enabled` is still ignored —
        // accelerator keys are handled by the host's responder
        // chain before WebKit sees them. Silently ignore per the
        // trait's "ignore unsupported fields" contract.
        Ok(())
    }

    fn post_web_message(&mut self, message: &str) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "post_web_message must be called on the main thread".into(),
            ));
        }
        // The host script invokes the bridge's private dispatcher,
        // matching what JS-side `window.chrome.webview.addEventListener`
        // listeners observe.
        let js_payload = js_string_literal(message);
        let js_source = format!(
            "if (window.chrome && window.chrome.webview && window.chrome.webview._dispatchHostMessage) {{ window.chrome.webview._dispatchHostMessage({js_payload}); }}"
        );
        let js_ns = NSString::from_str(&js_source);
        unsafe {
            self.webview()
                .evaluateJavaScript_completionHandler(&js_ns, None);
        }
        Ok(())
    }

    fn poll_web_message(&mut self) -> Option<String> {
        self.web_messages
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front())
    }

    fn send_keyboard_input(&mut self, event: KeyboardInput) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "send_keyboard_input must be called on the main thread".into(),
            ));
        }
        let window = self.webview().window().ok_or_else(|| {
            WebSurfaceError::Platform(
                "WKWebView is not in a window — send_keyboard_input requires the parent NSView to be embedded in an NSWindow".into(),
            )
        })?;
        let window_number = window.windowNumber();

        let event_type = match event.kind {
            KeyEventKind::Down => NSEventType::KeyDown,
            KeyEventKind::Up => NSEventType::KeyUp,
            KeyEventKind::ModifiersChanged => NSEventType::FlagsChanged,
        };
        let modifier_flags = key_modifier_flags(event.modifiers);
        let characters = NSString::from_str(&event.characters);
        let characters_ignoring = NSString::from_str(&event.characters_ignoring_modifiers);

        // For `flagsChanged:` AppKit ignores the characters fields and
        // relies on `keyCode` + `modifierFlags` — match that behavior.
        // For key events, characters drive WebKit's text input
        // pipeline (and therefore IME composition for non-Latin
        // input — WebKit's `NSTextInputClient` impl reads these
        // fields).
        let ns_event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            event_type,
            NSPoint::new(0.0, 0.0),
            modifier_flags,
            0.0,
            window_number,
            None,
            &characters,
            &characters_ignoring,
            event.is_repeat,
            event.virtual_key_code as u16,
        )
        .ok_or_else(|| {
            WebSurfaceError::Platform(
                "NSEvent::keyEventWithType returned nil for the synthesized key event".into(),
            )
        })?;

        match event.kind {
            KeyEventKind::Down => self.webview().keyDown(&ns_event),
            KeyEventKind::Up => self.webview().keyUp(&ns_event),
            KeyEventKind::ModifiersChanged => self.webview().flagsChanged(&ns_event),
        }
        Ok(())
    }

    fn send_mouse_input(&mut self, event: MouseInput) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "send_mouse_input must be called on the main thread".into(),
            ));
        }
        let window = self.webview().window().ok_or_else(|| {
            WebSurfaceError::Platform(
                "WKWebView is not in a window — send_mouse_input requires the parent NSView to be embedded in an NSWindow".into(),
            )
        })?;

        let dispatch = synthesize_mouse_event(self.webview(), &window, event)?;
        let ns_event = dispatch.event;
        match dispatch.target {
            MouseTarget::MouseDown => self.webview().mouseDown(&ns_event),
            MouseTarget::MouseUp => self.webview().mouseUp(&ns_event),
            MouseTarget::MouseDragged => self.webview().mouseDragged(&ns_event),
            MouseTarget::MouseMoved => self.webview().mouseMoved(&ns_event),
            MouseTarget::RightMouseDown => self.webview().rightMouseDown(&ns_event),
            MouseTarget::RightMouseUp => self.webview().rightMouseUp(&ns_event),
            MouseTarget::RightMouseDragged => self.webview().rightMouseDragged(&ns_event),
            MouseTarget::OtherMouseDown => self.webview().otherMouseDown(&ns_event),
            MouseTarget::OtherMouseUp => self.webview().otherMouseUp(&ns_event),
            MouseTarget::OtherMouseDragged => self.webview().otherMouseDragged(&ns_event),
            MouseTarget::MouseExited => self.webview().mouseExited(&ns_event),
            MouseTarget::ScrollWheel => self.webview().scrollWheel(&ns_event),
        }
        self.observe_cursor_change();
        Ok(())
    }

    fn poll_cursor_shape(&mut self) -> Option<CursorShape> {
        self.cursor_shapes.pop_front()
    }

    /// Drag-and-drop forwarding on macOS.
    ///
    /// **Capture mode (post-`start_capture`)**: not feasible without
    /// platform SPI. `WKWebView` receives drags via the
    /// `NSDraggingDestination` protocol — `draggingEntered:`,
    /// `draggingUpdated:`, `performDragOperation:`, etc. — all of
    /// which require an `NSDraggingInfo` instance, an opaque object
    /// only AppKit's drag manager constructs. There is no public API
    /// to synthesize one. Slice K territory if a downstream consumer
    /// builds an SPI bridge; out of scope for the public surface.
    ///
    /// **Overlay mode (pre-`start_capture`)**: AppKit's drag manager
    /// targets the WKWebView directly through the responder chain —
    /// the host doesn't need to forward anything; WebKit gets the
    /// drag through standard `NSDraggingDestination` handling.
    /// `send_drag_input` is therefore unnecessary in this mode and
    /// returns the same `Unsupported` to make the no-op explicit.
    fn send_drag_input(&mut self, _event: DragInput) -> Result<(), WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "WkWebViewProducer::send_drag_input — capture-mode drag forwarding requires \
             NSDraggingInfo synthesis (SPI); overlay-mode drag works automatically through \
             AppKit's responder chain without producer involvement",
        ))
    }

    /// Forward a touch / pen / pointer event to the WebView.
    ///
    /// macOS doesn't have a touch screen as a first-class input
    /// surface (no public API for synthesizing direct-touch
    /// `NSEventTypeDirectTouch` events) and `NSEvent::mouseEventWithType`
    /// doesn't expose pen pressure / tilt fields. So scrying maps
    /// every `PointerInput` to a synthetic mouse-event dispatch via
    /// the same `synthesize_mouse_event` path used for
    /// `send_mouse_input`. Pressure / tilt / `pointer_id` are
    /// preserved at the API surface but **dropped at the WebKit
    /// layer**: WebKit's Pointer Events for the resulting JS-side
    /// event will report `pointerType: "mouse"` regardless of
    /// `event.device`.
    ///
    /// What works:
    /// - Hover / move / press / release / leave events flow through
    ///   to the WebView, drive Pointer Events on the JS side, and
    ///   trigger CSS `:hover` / `:active` and JS pointer handlers.
    /// - The `point` coordinates use the same physical-pixel,
    ///   webview-local origin as `send_mouse_input`.
    /// - Modifier-key state from `event.virtual_keys` is passed
    ///   through.
    ///
    /// What doesn't:
    /// - Multi-touch tracking via `pointer_id` (AppKit synthesizes
    ///   one cursor; multiple simultaneous pointers can't be
    ///   represented).
    /// - Pen pressure / tilt — synthesized events don't carry
    ///   tablet metadata. Real tablet hardware drivers deliver
    ///   actual `NSEventTypeTabletPoint` events through the
    ///   responder chain in overlay mode; the producer doesn't
    ///   need to (and can't usefully) re-synthesize them.
    fn send_pointer_input(&mut self, event: PointerInput) -> Result<(), WebSurfaceError> {
        // We treat all pointer devices uniformly; the tabletPoint
        // path on macOS doesn't accept synthesized events with
        // pressure metadata, and we'd lose information either way.
        let _ = event.device;
        let _ = event.pointer_id;
        let _ = event.pressure;
        let _ = event.tilt;

        let kind = match event.kind {
            PointerEventKind::Activate | PointerEventKind::Down => MouseEventKind::LeftButtonDown,
            PointerEventKind::Up => MouseEventKind::LeftButtonUp,
            PointerEventKind::Update | PointerEventKind::Enter => MouseEventKind::Move,
            PointerEventKind::Leave => MouseEventKind::Leave,
            PointerEventKind::CaptureChanged => {
                // No AppKit analog; treat as a no-op (the host
                // already knows its pointer-capture changed).
                return Ok(());
            }
        };
        // Touch-style "primary button held" state during dragging:
        // pen / touch interactions inherently have a single button,
        // so we set `left_button` for Down → Up transitions.
        let mut virtual_keys = MouseVirtualKeys::default();
        if matches!(
            event.kind,
            PointerEventKind::Down | PointerEventKind::Update | PointerEventKind::Activate
        ) && event.device != PointerDevice::Mouse
        {
            virtual_keys.left_button = true;
        }
        let synthetic = MouseInput {
            kind,
            virtual_keys,
            mouse_data: 0,
            point: event.point,
        };
        self.send_mouse_input(synthetic)
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "resize must be called on the main thread".into(),
            ));
        }
        // Clear any pending DPI-change flag — we're explicitly
        // re-applying size here.
        self.dpi_pending
            .store(false, std::sync::atomic::Ordering::Release);
        self.resize_internal(size)
    }

    fn set_offset(&mut self, x: f32, y: f32) -> Result<(), WebSurfaceError> {
        if MainThreadMarker::new().is_none() {
            return Err(WebSurfaceError::Platform(
                "set_offset must be called on the main thread".into(),
            ));
        }
        let scale = self.current_backing_scale();
        let ns_origin = NSPoint::new(f64::from(x) / scale, f64::from(y) / scale);
        self.webview().setFrameOrigin(ns_origin);
        self.config.offset = (x, y);
        // Webview moved → SCK source rect now points at the wrong
        // window region. Push a fresh configuration so the next
        // captured frame samples the new webview position.
        self.update_capture_for_layout_change();
        Ok(())
    }
}

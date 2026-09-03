// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

impl WebView2CompositionProducer {
    /// Forward a mouse / scroll event to the composition WebView2.
    ///
    /// `event.point` is in physical pixels relative to the webview's
    /// top-left corner (the same coordinate space the controller's
    /// `Bounds` uses).
    pub fn send_mouse_input(&self, event: MouseInput) -> Result<(), WebSurfaceError> {
        let kind = mouse_event_kind(event.kind);
        let virtual_keys =
            COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(virtual_keys_bits(event.virtual_keys) as i32);
        let point = POINT {
            x: event.point.0,
            y: event.point.1,
        };
        unsafe {
            self.composition_controller
                .SendMouseInput(kind, virtual_keys, event.mouse_data as u32, point)
                .map_err(platform("SendMouseInput"))
        }
    }

    /// Forward a touch / pen pointer event to the composition WebView2.
    ///
    /// Builds an `ICoreWebView2PointerInfo` from `event` and dispatches via
    /// `ICoreWebView2CompositionController::SendPointerInput`. Pen tilt is
    /// in radians on the public API; converted to degrees for WebView2.
    pub fn send_pointer_input(&self, event: crate::PointerInput) -> Result<(), WebSurfaceError> {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment3;
        let env3: ICoreWebView2Environment3 = self
            .environment
            .cast()
            .map_err(platform("environment cast to ICoreWebView2Environment3"))?;
        let info = unsafe { env3.CreateCoreWebView2PointerInfo() }
            .map_err(platform("CreateCoreWebView2PointerInfo"))?;

        let pointer_kind: u32 = match event.device {
            crate::PointerDevice::Touch => 2,
            crate::PointerDevice::Pen => 3,
            crate::PointerDevice::Mouse => 4,
        };
        let pointer_flags = pointer_flags_for(event.kind);
        let point = POINT {
            x: event.point.0,
            y: event.point.1,
        };
        let mut perf_count: i64 = 0;
        unsafe {
            windows::Win32::System::Performance::QueryPerformanceCounter(&mut perf_count)
                .map_err(platform("QueryPerformanceCounter"))?;
        }

        unsafe {
            info.SetPointerKind(pointer_kind)
                .map_err(platform("SetPointerKind"))?;
            info.SetPointerId(event.pointer_id)
                .map_err(platform("SetPointerId"))?;
            info.SetFrameId(0).map_err(platform("SetFrameId"))?;
            info.SetPointerFlags(pointer_flags)
                .map_err(platform("SetPointerFlags"))?;
            info.SetPixelLocation(point)
                .map_err(platform("SetPixelLocation"))?;
            info.SetPixelLocationRaw(point)
                .map_err(platform("SetPixelLocationRaw"))?;
            info.SetHimetricLocation(POINT { x: 0, y: 0 })
                .map_err(platform("SetHimetricLocation"))?;
            info.SetHimetricLocationRaw(POINT { x: 0, y: 0 })
                .map_err(platform("SetHimetricLocationRaw"))?;
            info.SetPerformanceCount(perf_count as u64)
                .map_err(platform("SetPerformanceCount"))?;
            info.SetHistoryCount(1)
                .map_err(platform("SetHistoryCount"))?;
            info.SetButtonChangeKind(0)
                .map_err(platform("SetButtonChangeKind"))?;

            match event.device {
                crate::PointerDevice::Touch => {
                    info.SetTouchMask(0x4).map_err(platform("SetTouchMask"))?;
                    let pressure = (event.pressure.clamp(0.0, 1.0) * 1024.0) as u32;
                    info.SetTouchPressure(pressure)
                        .map_err(platform("SetTouchPressure"))?;
                    let contact = RECT {
                        left: point.x - 1,
                        top: point.y - 1,
                        right: point.x + 1,
                        bottom: point.y + 1,
                    };
                    info.SetTouchContact(contact)
                        .map_err(platform("SetTouchContact"))?;
                    info.SetTouchContactRaw(contact)
                        .map_err(platform("SetTouchContactRaw"))?;
                }
                crate::PointerDevice::Pen => {
                    info.SetPenMask(0x1 | 0x4 | 0x8)
                        .map_err(platform("SetPenMask"))?;
                    let pressure = (event.pressure.clamp(0.0, 1.0) * 1024.0) as u32;
                    info.SetPenPressure(pressure)
                        .map_err(platform("SetPenPressure"))?;
                    let tilt_x_deg = event.tilt.0.to_degrees().clamp(-90.0, 90.0) as i32;
                    let tilt_y_deg = event.tilt.1.to_degrees().clamp(-90.0, 90.0) as i32;
                    info.SetPenTiltX(tilt_x_deg)
                        .map_err(platform("SetPenTiltX"))?;
                    info.SetPenTiltY(tilt_y_deg)
                        .map_err(platform("SetPenTiltY"))?;
                }
                crate::PointerDevice::Mouse => {}
            }
        }

        let event_kind = pointer_event_kind(event.kind);
        unsafe {
            self.composition_controller
                .SendPointerInput(event_kind, &info)
                .map_err(platform("SendPointerInput"))
        }
    }

    /// Forward a drag-enter event to the composition WebView2 with an
    /// `IDataObject` carrying the dragged content.
    pub fn drag_enter(
        &self,
        data_object: &windows::Win32::System::Com::IDataObject,
        key_state: u32,
        point: (i32, i32),
        effects: &mut u32,
    ) -> Result<(), WebSurfaceError> {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2CompositionController3;
        let cc3: ICoreWebView2CompositionController3 = self.composition_controller.cast().map_err(
            platform("composition_controller cast to ICoreWebView2CompositionController3"),
        )?;
        let p = POINT {
            x: point.0,
            y: point.1,
        };
        unsafe {
            cc3.DragEnter(data_object, key_state, p, effects as *mut u32)
                .map_err(platform("DragEnter"))
        }
    }

    /// Forward a drag-over event.
    pub fn drag_over(
        &self,
        key_state: u32,
        point: (i32, i32),
        effects: &mut u32,
    ) -> Result<(), WebSurfaceError> {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2CompositionController3;
        let cc3: ICoreWebView2CompositionController3 = self.composition_controller.cast().map_err(
            platform("composition_controller cast to ICoreWebView2CompositionController3"),
        )?;
        let p = POINT {
            x: point.0,
            y: point.1,
        };
        unsafe {
            cc3.DragOver(key_state, p, effects as *mut u32)
                .map_err(platform("DragOver"))
        }
    }

    /// Forward a drag-leave event.
    pub fn drag_leave(&self) -> Result<(), WebSurfaceError> {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2CompositionController3;
        let cc3: ICoreWebView2CompositionController3 = self.composition_controller.cast().map_err(
            platform("composition_controller cast to ICoreWebView2CompositionController3"),
        )?;
        unsafe { cc3.DragLeave() }.map_err(platform("DragLeave"))
    }

    /// Forward a drop event. Same `IDataObject` shape as
    /// [`drag_enter`](Self::drag_enter).
    pub fn drop_data(
        &self,
        data_object: &windows::Win32::System::Com::IDataObject,
        key_state: u32,
        point: (i32, i32),
        effects: &mut u32,
    ) -> Result<(), WebSurfaceError> {
        use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2CompositionController3;
        let cc3: ICoreWebView2CompositionController3 = self.composition_controller.cast().map_err(
            platform("composition_controller cast to ICoreWebView2CompositionController3"),
        )?;
        let p = POINT {
            x: point.0,
            y: point.1,
        };
        unsafe {
            cc3.Drop(data_object, key_state, p, effects as *mut u32)
                .map_err(platform("Drop"))
        }
    }

    /// Move keyboard focus into the WebView2.
    pub fn move_focus(&self, reason: FocusReason) -> Result<(), WebSurfaceError> {
        let reason = focus_reason(reason);
        unsafe {
            self.controller
                .MoveFocus(reason)
                .map_err(platform("MoveFocus"))
        }
    }

    /// Post one raw Win32 keyboard / IME message to the WebView2 parent HWND.
    ///
    /// This is a diagnostic escape hatch, not a supported browser-grade input
    /// route. Current WebView2 CompositionController runtime probes show that
    /// posted `WM_KEY*`, `WM_CHAR`, and `WM_IME*` messages do not produce DOM
    /// keyboard/text/IME input in the visual-hosted WebView.
    pub fn forward_keyboard_message(
        &self,
        message: u32,
        wparam: usize,
        lparam: isize,
    ) -> Result<(), WebSurfaceError> {
        if !is_webview2_keyboard_message(message) {
            return Err(WebSurfaceError::Unsupported(
                "WebView2CompositionProducer::forward_keyboard_message only accepts WM_KEY*, WM_CHAR, WM_DEADCHAR, and WM_IME* messages",
            ));
        }
        self.post_keyboard_message(message, wparam, lparam)
    }

    /// Forward a keyboard / modifier-state event through WebView2's public CDP
    /// bridge.
    ///
    /// WebView2 exposes public mouse, pointer, drag, focus, and cursor APIs for
    /// visual hosting, but not a CompositionController-specific keyboard API.
    /// `Input.dispatchKeyEvent` is the public route that can deliver synthetic
    /// keyboard DOM input to a visual-hosted WebView.
    pub fn send_keyboard_input(&self, event: KeyboardInput) -> Result<(), WebSurfaceError> {
        match event.kind {
            KeyEventKind::Down => {
                let key_down = cdp_dispatch_key_event_params("rawKeyDown", &event, None);
                self.call_devtools_protocol_method_blocking(
                    "Input.dispatchKeyEvent",
                    &key_down,
                    std::time::Duration::from_secs(2),
                )?;
                if !event.characters.is_empty() {
                    let char_event =
                        cdp_dispatch_key_event_params("char", &event, Some(&event.characters));
                    self.call_devtools_protocol_method_blocking(
                        "Input.dispatchKeyEvent",
                        &char_event,
                        std::time::Duration::from_secs(2),
                    )?;
                }
            }
            KeyEventKind::Up => {
                let key_up = cdp_dispatch_key_event_params("keyUp", &event, None);
                self.call_devtools_protocol_method_blocking(
                    "Input.dispatchKeyEvent",
                    &key_up,
                    std::time::Duration::from_secs(2),
                )?;
            }
            KeyEventKind::ModifiersChanged => {
                let event_type = if modifier_is_down(event.virtual_key_code, event.modifiers) {
                    "rawKeyDown"
                } else {
                    "keyUp"
                };
                let params = cdp_dispatch_key_event_params(event_type, &event, None);
                self.call_devtools_protocol_method_blocking(
                    "Input.dispatchKeyEvent",
                    &params,
                    std::time::Duration::from_secs(2),
                )?;
            }
        }
        Ok(())
    }

    /// Insert text through CDP `Input.insertText`.
    ///
    /// This is useful for host-controlled form fill and committed IME text.
    /// It is not the same as handing WebView2 the OS IME/candidate UI.
    pub fn insert_text(&self, text: &str) -> Result<(), WebSurfaceError> {
        let params = format!(r#"{{"text":{}}}"#, json_string(text));
        self.call_devtools_protocol_method_blocking(
            "Input.insertText",
            &params,
            std::time::Duration::from_secs(2),
        )?;
        Ok(())
    }

    /// Set the current IME composition through CDP `Input.imeSetComposition`.
    ///
    /// Hosts that own IME state can use this to mirror marked/preedit text into
    /// the page. WebView2 still does not provide native OS IME ownership for the
    /// visual-hosted CompositionController path.
    pub fn set_ime_composition(
        &self,
        text: &str,
        selection_start: i32,
        selection_end: i32,
        replacement_start: i32,
        replacement_end: i32,
    ) -> Result<(), WebSurfaceError> {
        let params = format!(
            r#"{{"text":{},"selectionStart":{selection_start},"selectionEnd":{selection_end},"replacementStart":{replacement_start},"replacementEnd":{replacement_end}}}"#,
            json_string(text)
        );
        self.call_devtools_protocol_method_blocking(
            "Input.imeSetComposition",
            &params,
            std::time::Duration::from_secs(2),
        )?;
        Ok(())
    }

    fn post_keyboard_message(
        &self,
        message: u32,
        wparam: usize,
        lparam: isize,
    ) -> Result<(), WebSurfaceError> {
        unsafe {
            PostMessageW(
                Some(self.parent_hwnd),
                message,
                WPARAM(wparam),
                LPARAM(lparam),
            )
        }
        .map_err(platform("PostMessageW keyboard message"))
    }

    /// Drain the next cursor-change request from the webview.
    pub fn poll_cursor_shape(&self) -> Option<CursorShape> {
        self.cursor_queue.lock().ok()?.pop_front()
    }
}

fn mouse_event_kind(kind: MouseEventKind) -> COREWEBVIEW2_MOUSE_EVENT_KIND {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL, COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOUBLE_CLICK,
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN,
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOUBLE_CLICK,
        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN,
        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOUBLE_CLICK,
        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN,
        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOUBLE_CLICK,
        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
    };
    match kind {
        MouseEventKind::LeftButtonDown => COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN,
        MouseEventKind::LeftButtonUp => COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
        MouseEventKind::LeftButtonDoubleClick => {
            COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOUBLE_CLICK
        }
        MouseEventKind::MiddleButtonDown => COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN,
        MouseEventKind::MiddleButtonUp => COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP,
        MouseEventKind::MiddleButtonDoubleClick => {
            COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOUBLE_CLICK
        }
        MouseEventKind::RightButtonDown => COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN,
        MouseEventKind::RightButtonUp => COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP,
        MouseEventKind::RightButtonDoubleClick => {
            COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOUBLE_CLICK
        }
        MouseEventKind::XButtonDown => COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN,
        MouseEventKind::XButtonUp => COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
        MouseEventKind::XButtonDoubleClick => COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOUBLE_CLICK,
        MouseEventKind::Move => COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
        MouseEventKind::Wheel => COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
        MouseEventKind::HorizontalWheel => COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
        MouseEventKind::Leave => COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
    }
}

fn virtual_keys_bits(keys: crate::MouseVirtualKeys) -> u32 {
    let mut bits = 0u32;
    if keys.left_button {
        bits |= 0x0001;
    }
    if keys.right_button {
        bits |= 0x0002;
    }
    if keys.shift {
        bits |= 0x0004;
    }
    if keys.control {
        bits |= 0x0008;
    }
    if keys.middle_button {
        bits |= 0x0010;
    }
    if keys.x_button1 {
        bits |= 0x0020;
    }
    if keys.x_button2 {
        bits |= 0x0040;
    }
    bits
}

fn pointer_event_kind(
    kind: crate::PointerEventKind,
) -> webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_POINTER_EVENT_KIND {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_POINTER_EVENT_KIND_ACTIVATE, COREWEBVIEW2_POINTER_EVENT_KIND_DOWN,
        COREWEBVIEW2_POINTER_EVENT_KIND_ENTER, COREWEBVIEW2_POINTER_EVENT_KIND_LEAVE,
        COREWEBVIEW2_POINTER_EVENT_KIND_UP, COREWEBVIEW2_POINTER_EVENT_KIND_UPDATE,
    };
    match kind {
        crate::PointerEventKind::Enter => COREWEBVIEW2_POINTER_EVENT_KIND_ENTER,
        crate::PointerEventKind::Down => COREWEBVIEW2_POINTER_EVENT_KIND_DOWN,
        crate::PointerEventKind::Update => COREWEBVIEW2_POINTER_EVENT_KIND_UPDATE,
        crate::PointerEventKind::Up => COREWEBVIEW2_POINTER_EVENT_KIND_UP,
        crate::PointerEventKind::Leave => COREWEBVIEW2_POINTER_EVENT_KIND_LEAVE,
        crate::PointerEventKind::Activate => COREWEBVIEW2_POINTER_EVENT_KIND_ACTIVATE,
        crate::PointerEventKind::CaptureChanged => COREWEBVIEW2_POINTER_EVENT_KIND_UPDATE,
    }
}

fn pointer_flags_for(kind: crate::PointerEventKind) -> u32 {
    const POINTER_FLAG_INRANGE: u32 = 0x00000002;
    const POINTER_FLAG_INCONTACT: u32 = 0x00000004;
    const POINTER_FLAG_PRIMARY: u32 = 0x00002000;
    const POINTER_FLAG_DOWN: u32 = 0x00010000;
    const POINTER_FLAG_UPDATE: u32 = 0x00020000;
    const POINTER_FLAG_UP: u32 = 0x00040000;
    const POINTER_FLAG_CAPTURECHANGED: u32 = 0x00200000;
    match kind {
        crate::PointerEventKind::Down => {
            POINTER_FLAG_DOWN | POINTER_FLAG_INCONTACT | POINTER_FLAG_INRANGE | POINTER_FLAG_PRIMARY
        }
        crate::PointerEventKind::Up => POINTER_FLAG_UP | POINTER_FLAG_PRIMARY,
        crate::PointerEventKind::Update => {
            POINTER_FLAG_UPDATE
                | POINTER_FLAG_INCONTACT
                | POINTER_FLAG_INRANGE
                | POINTER_FLAG_PRIMARY
        }
        crate::PointerEventKind::Enter => POINTER_FLAG_INRANGE | POINTER_FLAG_PRIMARY,
        crate::PointerEventKind::Leave => POINTER_FLAG_PRIMARY,
        crate::PointerEventKind::Activate => POINTER_FLAG_PRIMARY,
        crate::PointerEventKind::CaptureChanged => POINTER_FLAG_CAPTURECHANGED,
    }
}

fn focus_reason(reason: FocusReason) -> COREWEBVIEW2_MOVE_FOCUS_REASON {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_MOVE_FOCUS_REASON_NEXT, COREWEBVIEW2_MOVE_FOCUS_REASON_PREVIOUS,
        COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
    };
    match reason {
        FocusReason::Programmatic => COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
        FocusReason::Next => COREWEBVIEW2_MOVE_FOCUS_REASON_NEXT,
        FocusReason::Previous => COREWEBVIEW2_MOVE_FOCUS_REASON_PREVIOUS,
    }
}

fn cdp_dispatch_key_event_params(
    event_type: &str,
    event: &KeyboardInput,
    text: Option<&str>,
) -> String {
    let (key, code) = cdp_key_identity(event);
    let modifiers = cdp_modifiers(event.modifiers);
    let mut params = format!(
        r#"{{"type":{},"windowsVirtualKeyCode":{},"nativeVirtualKeyCode":{},"code":{},"key":{},"modifiers":{},"autoRepeat":{}"#,
        json_string(event_type),
        event.virtual_key_code,
        event.virtual_key_code,
        json_string(&code),
        json_string(&key),
        modifiers,
        event.is_repeat
    );
    if let Some(text) = text {
        params.push_str(&format!(
            r#","text":{},"unmodifiedText":{}"#,
            json_string(text),
            json_string(&event.characters_ignoring_modifiers)
        ));
    }
    params.push('}');
    params
}

fn cdp_key_identity(event: &KeyboardInput) -> (String, String) {
    match event.virtual_key_code {
        0x30..=0x39 => {
            let ch = char::from_u32(event.virtual_key_code).unwrap_or('0');
            (ch.to_string(), format!("Digit{ch}"))
        }
        0x41..=0x5A => {
            let upper = char::from_u32(event.virtual_key_code).unwrap_or('A');
            let key = event
                .characters_ignoring_modifiers
                .chars()
                .next()
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| upper.to_ascii_lowercase().to_string());
            (key, format!("Key{upper}"))
        }
        0x10 | 0xA0 | 0xA1 => ("Shift".into(), "ShiftLeft".into()),
        0x11 | 0xA2 | 0xA3 => ("Control".into(), "ControlLeft".into()),
        0x12 | 0xA4 | 0xA5 => ("Alt".into(), "AltLeft".into()),
        0x5B | 0x5C => ("Meta".into(), "MetaLeft".into()),
        0x0D => ("Enter".into(), "Enter".into()),
        0x08 => ("Backspace".into(), "Backspace".into()),
        0x09 => ("Tab".into(), "Tab".into()),
        0x1B => ("Escape".into(), "Escape".into()),
        0x20 => (" ".into(), "Space".into()),
        _ => (
            event
                .characters_ignoring_modifiers
                .chars()
                .next()
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| format!("Unidentified")),
            String::new(),
        ),
    }
}

fn cdp_modifiers(modifiers: crate::KeyModifierFlags) -> u32 {
    let mut bits = 0u32;
    if modifiers.alt {
        bits |= 1;
    }
    if modifiers.control {
        bits |= 2;
    }
    if modifiers.meta {
        bits |= 4;
    }
    if modifiers.shift {
        bits |= 8;
    }
    bits
}

fn modifier_is_down(virtual_key_code: u32, modifiers: crate::KeyModifierFlags) -> bool {
    match virtual_key_code {
        0x10 | 0xA0 | 0xA1 => modifiers.shift,
        0x11 | 0xA2 | 0xA3 => modifiers.control,
        0x12 | 0xA4 | 0xA5 => modifiers.alt,
        0x5B | 0x5C => modifiers.meta,
        0x14 => modifiers.caps_lock,
        _ => false,
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn is_webview2_keyboard_message(message: u32) -> bool {
    matches!(
        message,
        WM_KEYDOWN
            | WM_KEYUP
            | WM_SYSKEYDOWN
            | WM_SYSKEYUP
            | WM_CHAR
            | WM_SYSCHAR
            | WM_DEADCHAR
            | WM_SYSDEADCHAR
            | WM_IME_STARTCOMPOSITION
            | WM_IME_ENDCOMPOSITION
            | WM_IME_COMPOSITION
            | WM_IME_COMPOSITIONFULL
            | WM_IME_CONTROL
            | WM_IME_NOTIFY
            | WM_IME_REQUEST
            | WM_IME_SELECT
            | WM_IME_SETCONTEXT
            | WM_IME_CHAR
            | WM_IME_KEYDOWN
            | WM_IME_KEYUP
    )
}

pub(super) fn register_cursor_changed_handler(
    composition_controller: &ICoreWebView2CompositionController,
    cursor_queue: Arc<Mutex<VecDeque<CursorShape>>>,
) -> Result<i64, WebSurfaceError> {
    use webview2_com::CursorChangedEventHandler;
    let cc = composition_controller.clone();
    let handler = CursorChangedEventHandler::create(Box::new(move |_, _| {
        let mut hcursor: HCURSOR = HCURSOR::default();
        if unsafe { cc.Cursor(&mut hcursor) }.is_ok() {
            let shape = hcursor_to_shape(hcursor);
            if let Ok(mut q) = cursor_queue.lock() {
                q.push_back(shape);
            }
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        composition_controller
            .add_CursorChanged(&handler, &mut token)
            .map_err(platform("add_CursorChanged"))?;
    }
    Ok(token)
}

pub(super) fn register_accelerator_key_pressed_handler(
    controller: &ICoreWebView2Controller,
    nav_queue: Arc<Mutex<VecDeque<NavigationEvent>>>,
) -> Result<i64, WebSurfaceError> {
    let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let mut raw_kind = COREWEBVIEW2_KEY_EVENT_KIND(0);
        let mut virtual_key_code = 0u32;
        let mut key_event_lparam = 0i32;
        let mut raw_physical = COREWEBVIEW2_PHYSICAL_KEY_STATUS::default();

        unsafe {
            args.KeyEventKind(&mut raw_kind)?;
            args.VirtualKey(&mut virtual_key_code)?;
            args.KeyEventLParam(&mut key_event_lparam)?;
            args.PhysicalKeyStatus(&mut raw_physical)?;
        }

        let browser_accelerator_key_enabled = args
            .cast::<ICoreWebView2AcceleratorKeyPressedEventArgs2>()
            .ok()
            .map(|args2| unsafe {
                read_bool_from(|value| args2.IsBrowserAcceleratorKeyEnabled(value))
            });
        let event = AcceleratorKeyEvent {
            kind: key_event_kind(raw_kind),
            is_system_key: matches!(
                raw_kind,
                COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN
                    | COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_UP
            ),
            virtual_key_code,
            key_event_lparam,
            physical_key_status: PhysicalKeyStatus {
                repeat_count: raw_physical.RepeatCount,
                scan_code: raw_physical.ScanCode,
                is_extended_key: raw_physical.IsExtendedKey.as_bool(),
                is_menu_key_down: raw_physical.IsMenuKeyDown.as_bool(),
                was_key_down: raw_physical.WasKeyDown.as_bool(),
                is_key_released: raw_physical.IsKeyReleased.as_bool(),
            },
            browser_accelerator_key_enabled,
        };
        if let Ok(mut q) = nav_queue.lock() {
            q.push_back(NavigationEvent::AcceleratorKeyPressed { event });
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        controller
            .add_AcceleratorKeyPressed(&handler, &mut token)
            .map_err(platform("add_AcceleratorKeyPressed"))?;
    }
    Ok(token)
}

fn key_event_kind(kind: COREWEBVIEW2_KEY_EVENT_KIND) -> KeyEventKind {
    match kind {
        COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP | COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_UP => {
            KeyEventKind::Up
        }
        COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN | COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN => {
            KeyEventKind::Down
        }
        _ => KeyEventKind::Down,
    }
}

fn hcursor_to_shape(cursor: HCURSOR) -> CursorShape {
    let pairs: [(windows::core::PCWSTR, CursorShape); 13] = [
        (IDC_ARROW, CursorShape::Default),
        (IDC_HAND, CursorShape::Pointer),
        (IDC_IBEAM, CursorShape::Text),
        (IDC_WAIT, CursorShape::Wait),
        (IDC_CROSS, CursorShape::Crosshair),
        (IDC_SIZEALL, CursorShape::ResizeAll),
        (IDC_SIZENS, CursorShape::ResizeNs),
        (IDC_SIZEWE, CursorShape::ResizeEw),
        (IDC_SIZENESW, CursorShape::ResizeNeSw),
        (IDC_SIZENWSE, CursorShape::ResizeNwSe),
        (IDC_NO, CursorShape::NotAllowed),
        (IDC_HELP, CursorShape::Help),
        (IDC_APPSTARTING, CursorShape::Progress),
    ];
    for (id, shape) in pairs {
        let h = unsafe { LoadCursorW(None, id) };
        if let Ok(h) = h
            && h.0 == cursor.0
        {
            return shape;
        }
    }
    CursorShape::Custom(format!("hcursor:{:?}", cursor.0))
}

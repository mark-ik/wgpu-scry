// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Input forwarding via JavaScript event synthesis.
//!
//! The offscreen WebView shape has no natural GDK input route (no
//! window-system focus path delivers events to it), so the producer
//! synthesizes DOM events on the page side and dispatches them
//! through `webkit_web_view_evaluate_javascript`. The same shape the
//! macOS WKWebView producer uses for `send_pointer_input` (and the
//! parity matrix calls "mouse-shaped JS pointer events"), extended
//! here to cover keyboard, mouse, and pointer input.
//!
//! **Fidelity caveats:** synthesized DOM events arrive at page
//! listeners with `event.isTrusted === false`. Page code that
//! discriminates on `isTrusted` (some click-fraud defences,
//! `requestFullscreen()`, autoplay-gating user gestures, etc.) will
//! reject these events. Native click side-effects that the engine
//! triggers from real user gestures (form submission via Enter,
//! native context menus, focus stealing) do not fire. For most page
//! interactions (DOM event handlers, form-field updates, click-to-
//! navigate) this path works; a future native `GdkEvent`-synthesis
//! upgrade would close the trust gap.

use crate::{
    DragEventKind, DragInput, KeyEventKind, KeyboardInput, MouseEventKind, MouseInput,
    MouseVirtualKeys, PointerEventKind, PointerInput,
};

use super::script_message::escape_for_js;

/// JS source that dispatches a DOM `MouseEvent` (or `WheelEvent` for
/// wheel events) at the element under the given client coordinates.
/// Falls back to `document.body` if no element is hit.
pub(crate) fn mouse_event_js(input: MouseInput) -> String {
    let (type_name, button, _is_wheel) = mouse_kind_to_type(input.kind);
    let buttons_mask = mouse_buttons_mask(&input.virtual_keys);
    let (x, y) = (input.point.0, input.point.1);

    if matches!(
        input.kind,
        MouseEventKind::Wheel | MouseEventKind::HorizontalWheel
    ) {
        let delta = input.mouse_data;
        let (dx, dy) = if matches!(input.kind, MouseEventKind::HorizontalWheel) {
            (delta, 0)
        } else {
            // WebView2 wheel deltas are 120-per-notch; map to CSS
            // `WheelEvent.deltaY` in "pixel" mode at ~16 px/notch so
            // page handlers get a familiar magnitude.
            (0, -delta * 16 / 120)
        };
        return format!(
            r#"(function() {{
    var x = {x}, y = {y};
    var t = document.elementFromPoint(x, y) || document.body;
    if (!t) return;
    t.dispatchEvent(new WheelEvent("wheel", {{
        bubbles: true, cancelable: true, view: window,
        clientX: x, clientY: y,
        deltaX: {dx}, deltaY: {dy}, deltaMode: 0,
        ctrlKey: {ctrl}, shiftKey: {shift},
        buttons: {buttons_mask}
    }}));
}})();"#,
            ctrl = bool_lit(input.virtual_keys.control),
            shift = bool_lit(input.virtual_keys.shift),
        );
    }

    format!(
        r#"(function() {{
    var x = {x}, y = {y};
    var t = document.elementFromPoint(x, y) || document.body;
    if (!t) return;
    t.dispatchEvent(new MouseEvent("{type_name}", {{
        bubbles: true, cancelable: true, view: window,
        clientX: x, clientY: y, button: {button}, buttons: {buttons_mask},
        ctrlKey: {ctrl}, shiftKey: {shift}
    }}));
}})();"#,
        ctrl = bool_lit(input.virtual_keys.control),
        shift = bool_lit(input.virtual_keys.shift),
    )
}

/// JS source for a `PointerEvent` dispatch.
pub(crate) fn pointer_event_js(input: PointerInput) -> String {
    let type_name = match input.kind {
        PointerEventKind::Down | PointerEventKind::Activate => "pointerdown",
        PointerEventKind::Up => "pointerup",
        PointerEventKind::Update => "pointermove",
        PointerEventKind::Enter => "pointerenter",
        PointerEventKind::Leave => "pointerleave",
        PointerEventKind::CaptureChanged => "pointercancel",
    };
    let pointer_type = match input.device {
        crate::PointerDevice::Touch => "touch",
        crate::PointerDevice::Pen => "pen",
        crate::PointerDevice::Mouse => "mouse",
    };
    let (x, y) = (input.point.0, input.point.1);
    format!(
        r#"(function() {{
    var x = {x}, y = {y};
    var t = document.elementFromPoint(x, y) || document.body;
    if (!t) return;
    t.dispatchEvent(new PointerEvent("{type_name}", {{
        bubbles: true, cancelable: true, view: window,
        pointerId: {id}, pointerType: "{pointer_type}",
        pressure: {pressure},
        clientX: x, clientY: y, isPrimary: true
    }}));
}})();"#,
        id = input.pointer_id,
        pressure = input.pressure,
    )
}

/// JS source for a `KeyboardEvent` dispatch on `document.activeElement`
/// (falling back to `document.body`).
pub(crate) fn keyboard_event_js(input: &KeyboardInput) -> String {
    let type_name = match input.kind {
        KeyEventKind::Down => "keydown",
        KeyEventKind::Up => "keyup",
        // ModifiersChanged has no DOM analog — the modifier state is
        // reflected in subsequent key/mouse events. Emit a no-op so
        // the producer's caller doesn't need to filter.
        KeyEventKind::ModifiersChanged => return String::new(),
    };
    let key = escape_for_js(&input.characters);
    let code = escape_for_js(&input.characters_ignoring_modifiers);
    format!(
        r#"(function() {{
    var t = document.activeElement || document.body;
    if (!t) return;
    t.dispatchEvent(new KeyboardEvent("{type_name}", {{
        bubbles: true, cancelable: true,
        key: {key}, code: {code}, keyCode: {keycode},
        ctrlKey: {ctrl}, shiftKey: {shift}, altKey: {alt}, metaKey: {meta},
        repeat: {repeat}
    }}));
}})();"#,
        keycode = input.virtual_key_code,
        ctrl = bool_lit(input.modifiers.control),
        shift = bool_lit(input.modifiers.shift),
        alt = bool_lit(input.modifiers.alt),
        meta = bool_lit(input.modifiers.meta),
        repeat = bool_lit(input.is_repeat),
    )
}

/// JS source that programmatically focuses the page so subsequent
/// keyboard events have a target. Hosts call this when the user clicks
/// the webview region or tabs into it.
pub(crate) fn focus_page_js() -> &'static str {
    "(function() { if (document.body) document.body.focus(); })();"
}

/// JS source for a `DragEvent` dispatch. Native `GdkEventDND` would
/// give us `isTrusted = true` and a real `DataTransfer`, but it
/// requires a `GdkDragContext` from a real drag source — those can't
/// be synthesized cleanly without a drag origin widget. Pages whose
/// drop handlers only observe event coordinates and types still
/// work via this synthesis path; pages that read `event.dataTransfer.files`
/// will see an empty list.
pub(crate) fn drag_event_js(input: DragInput) -> String {
    let type_name = match input.kind {
        DragEventKind::Enter => "dragenter",
        DragEventKind::Over => "dragover",
        DragEventKind::Leave => "dragleave",
        DragEventKind::Drop => "drop",
    };
    let (x, y) = (input.point.0, input.point.1);
    format!(
        r#"(function() {{
    var x = {x}, y = {y};
    var t = document.elementFromPoint(x, y) || document.body;
    if (!t) return;
    var dt;
    try {{ dt = new DataTransfer(); }} catch (e) {{ dt = null; }}
    t.dispatchEvent(new DragEvent("{type_name}", {{
        bubbles: true, cancelable: true, view: window,
        clientX: x, clientY: y,
        ctrlKey: {ctrl}, shiftKey: {shift},
        dataTransfer: dt
    }}));
}})();"#,
        ctrl = bool_lit(input.virtual_keys.control),
        shift = bool_lit(input.virtual_keys.shift),
    )
}

fn mouse_kind_to_type(kind: MouseEventKind) -> (&'static str, u32, bool) {
    match kind {
        MouseEventKind::LeftButtonDown => ("mousedown", 0, false),
        MouseEventKind::LeftButtonUp => ("mouseup", 0, false),
        MouseEventKind::LeftButtonDoubleClick => ("dblclick", 0, false),
        MouseEventKind::MiddleButtonDown => ("mousedown", 1, false),
        MouseEventKind::MiddleButtonUp => ("mouseup", 1, false),
        MouseEventKind::MiddleButtonDoubleClick => ("dblclick", 1, false),
        MouseEventKind::RightButtonDown => ("mousedown", 2, false),
        MouseEventKind::RightButtonUp => ("mouseup", 2, false),
        MouseEventKind::RightButtonDoubleClick => ("dblclick", 2, false),
        MouseEventKind::XButtonDown => ("mousedown", 3, false),
        MouseEventKind::XButtonUp => ("mouseup", 3, false),
        MouseEventKind::XButtonDoubleClick => ("dblclick", 3, false),
        MouseEventKind::Move => ("mousemove", 0, false),
        MouseEventKind::Wheel => ("wheel", 0, true),
        MouseEventKind::HorizontalWheel => ("wheel", 0, true),
        MouseEventKind::Leave => ("mouseleave", 0, false),
    }
}

fn mouse_buttons_mask(vk: &MouseVirtualKeys) -> u32 {
    let mut mask = 0;
    if vk.left_button {
        mask |= 1;
    }
    if vk.right_button {
        mask |= 2;
    }
    if vk.middle_button {
        mask |= 4;
    }
    if vk.x_button1 {
        mask |= 8;
    }
    if vk.x_button2 {
        mask |= 16;
    }
    mask
}

#[inline]
fn bool_lit(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

//! Input forwarding via JavaScript event synthesis (WebKitGTK 6.0 /
//! GTK 4 line).
//!
//! Port of [`crate::webkitgtk_producer::input`] adapted to the
//! `webkit6 = "0.6"` + `gtk4 = "0.11"` binding set. The offscreen
//! WebView shape has no natural GDK input route (no window-system
//! focus path delivers events to it), so the producer synthesizes DOM
//! events on the page side and dispatches them through
//! `webkit_web_view_evaluate_javascript`. Same shape the macOS
//! WKWebView producer uses for `send_pointer_input` (and the parity
//! matrix calls "mouse-shaped JS pointer events"), extended here to
//! cover keyboard, mouse, pointer, and drag input.
//!
//! **Fidelity caveats:** synthesized DOM events arrive at page
//! listeners with `event.isTrusted === false`. Page code that
//! discriminates on `isTrusted` (some click-fraud defences,
//! `requestFullscreen()`, autoplay-gating user gestures, etc.) will
//! reject these events. Native click side-effects that the engine
//! triggers from real user gestures (form submission via Enter,
//! native context menus, focus stealing) do not fire. For most page
//! interactions (DOM event handlers, form-field updates, click-to-
//! navigate) this path works.
//!
//! **Why no native-event fallback on GTK 4:** the GTK 3 producer
//! pairs this JS-synthesis path with a native `GdkEvent`-dispatch
//! primary (`input_native.rs`, via `gtk_main_do_event`) that closes
//! the `isTrusted` gap. GTK 4 removed `gtk_main_do_event` (and
//! `gdk_event_new` / `gdk_event_put` along with it — see the
//! GTK 4 migration guide), so there is no analogous synthetic-event
//! path on the WebKitGTK 6.0 line. The webkit6 producer therefore
//! ships JS-synthesis only; a future native upgrade would route
//! through `gtk4::GestureClick` controller synthesis or direct
//! `GdkSurface` event queue manipulation, both of which are
//! materially harder than the GTK 3 `gtk_main_do_event` call.

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
///
/// Currently unused — `move_focus` lands in a follow-on sub-phase
/// (see Phase A of `design_docs/2026-05-15_phase4_strategy.md`).
/// Kept here so the surface is ready when that ports across.
#[allow(dead_code)]
pub(crate) fn focus_page_js() -> &'static str {
    "(function() { if (document.body) document.body.focus(); })();"
}

/// JS source for a `DragEvent` dispatch. Mirrors the GTK 3
/// precedent — native `GdkEventDND` would give us `isTrusted = true`
/// and a real `DataTransfer`, but it requires a `GdkDragContext` from
/// a real drag source. Pages whose drop handlers only observe event
/// coordinates and types still work via this synthesis path; pages
/// that read `event.dataTransfer.files` will see an empty list.
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

#[cfg(test)]
mod tests {
    //! Pure-string JS-builder assertions — no GTK runtime needed.
    //! These pin the synthesized JS shape so a refactor that breaks
    //! the event-dispatch contract surfaces as a unit-test failure
    //! rather than a runtime page-handler regression.

    use super::*;
    use crate::{KeyModifierFlags, PointerDevice};

    #[test]
    fn mouse_event_js_left_button_down_dispatches_mousedown() {
        let input = MouseInput {
            kind: MouseEventKind::LeftButtonDown,
            point: (10, 20),
            virtual_keys: MouseVirtualKeys::default(),
            mouse_data: 0,
        };
        let js = mouse_event_js(input);
        assert!(js.contains(r#"new MouseEvent("mousedown""#));
        assert!(js.contains("clientX: x, clientY: y"));
        assert!(js.contains("var x = 10, y = 20"));
        assert!(js.contains("button: 0"));
    }

    #[test]
    fn mouse_event_js_wheel_uses_wheel_event() {
        let input = MouseInput {
            kind: MouseEventKind::Wheel,
            point: (0, 0),
            virtual_keys: MouseVirtualKeys::default(),
            mouse_data: 120,
        };
        let js = mouse_event_js(input);
        assert!(js.contains(r#"new WheelEvent("wheel""#));
        // 120/notch × 16 px/notch / 120 = 16, negated for natural-scroll feel.
        assert!(js.contains("deltaY: -16"));
        assert!(js.contains("deltaX: 0"));
    }

    #[test]
    fn keyboard_event_js_modifiers_changed_is_empty() {
        let input = KeyboardInput {
            kind: KeyEventKind::ModifiersChanged,
            virtual_key_code: 0,
            characters: String::new(),
            characters_ignoring_modifiers: String::new(),
            modifiers: KeyModifierFlags::default(),
            is_repeat: false,
        };
        assert!(keyboard_event_js(&input).is_empty());
    }

    #[test]
    fn keyboard_event_js_keydown_carries_modifiers() {
        let input = KeyboardInput {
            kind: KeyEventKind::Down,
            virtual_key_code: 65,
            characters: "a".into(),
            characters_ignoring_modifiers: "KeyA".into(),
            modifiers: KeyModifierFlags {
                shift: true,
                control: true,
                alt: false,
                meta: false,
                caps_lock: false,
            },
            is_repeat: false,
        };
        let js = keyboard_event_js(&input);
        assert!(js.contains(r#"new KeyboardEvent("keydown""#));
        assert!(js.contains(r#"key: "a""#));
        assert!(js.contains(r#"code: "KeyA""#));
        assert!(js.contains("keyCode: 65"));
        assert!(js.contains("ctrlKey: true"));
        assert!(js.contains("shiftKey: true"));
        assert!(js.contains("altKey: false"));
        assert!(js.contains("metaKey: false"));
    }

    #[test]
    fn keyboard_event_js_escapes_quotes_in_characters() {
        let input = KeyboardInput {
            kind: KeyEventKind::Down,
            virtual_key_code: 0,
            characters: "\"".into(),
            characters_ignoring_modifiers: "\\".into(),
            modifiers: KeyModifierFlags::default(),
            is_repeat: false,
        };
        let js = keyboard_event_js(&input);
        // `"` becomes `\"` inside the JS string literal — without the
        // escape the synthesized JS would have unbalanced quotes and
        // either fail to parse or, worse, let the character break out
        // of the literal.
        assert!(js.contains(r#"key: "\"""#));
        assert!(js.contains(r#"code: "\\""#));
    }

    #[test]
    fn pointer_event_js_touch_uses_pointer_event() {
        let input = PointerInput {
            kind: PointerEventKind::Down,
            device: PointerDevice::Touch,
            point: (5, 7),
            pointer_id: 42,
            pressure: 0.5,
            tilt: (0.0, 0.0),
        };
        let js = pointer_event_js(input);
        assert!(js.contains(r#"new PointerEvent("pointerdown""#));
        assert!(js.contains(r#"pointerType: "touch""#));
        assert!(js.contains("pointerId: 42"));
        assert!(js.contains("pressure: 0.5"));
    }

    #[test]
    fn drag_event_js_drop_uses_drag_event() {
        let input = DragInput {
            kind: DragEventKind::Drop,
            virtual_keys: MouseVirtualKeys::default(),
            point: (100, 200),
            allowed_effects: 0,
        };
        let js = drag_event_js(input);
        assert!(js.contains(r#"new DragEvent("drop""#));
        assert!(js.contains("var x = 100, y = 200"));
    }

    #[test]
    fn mouse_buttons_mask_encodes_each_button() {
        let mut vk = MouseVirtualKeys::default();
        vk.left_button = true;
        vk.right_button = true;
        assert_eq!(super::mouse_buttons_mask(&vk), 1 | 2);
        let mut vk = MouseVirtualKeys::default();
        vk.middle_button = true;
        vk.x_button1 = true;
        vk.x_button2 = true;
        assert_eq!(super::mouse_buttons_mask(&vk), 4 | 8 | 16);
    }
}

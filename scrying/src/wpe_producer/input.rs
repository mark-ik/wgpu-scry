//! Input forwarding (keyboard + pointer + scroll) for the WPE producer.
//!
//! Translates scrying's `KeyboardInput` / `MouseInput` / `PointerInput`
//! into `wpe_event_*_new` constructions and dispatches via
//! `wpe_view_event(view, ev)`. Touch / drag / IME / pen-pressure /
//! cursor-shape are explicitly deferred (see the 4c.4 spec).
//!
//! Dispatch is single-threaded — the producer is `!Send` by design
//! (model A), so callers must invoke these from the producer's
//! construction thread.

use super::ffi;
use crate::{
    KeyEventKind, KeyboardInput, MouseEventKind, MouseInput, PointerDevice, PointerEventKind,
    PointerInput, WebSurfaceError,
};

// ============================================================================
// Pure-Rust translation helpers (unit-testable; no FFI dependency).
// ============================================================================

/// Map a scrying mouse-button-kind into a 1-based WebKit button index
/// (Left=1, Middle=2, Right=3 — matching the W3C `MouseEvent.button`
/// convention WebKit consumes). Returns `None` for non-button kinds
/// (Move / Wheel / etc).
pub(super) fn mouse_button_index(kind: MouseEventKind) -> Option<u32> {
    match kind {
        MouseEventKind::LeftButtonDown
        | MouseEventKind::LeftButtonUp
        | MouseEventKind::LeftButtonDoubleClick => Some(1),
        MouseEventKind::MiddleButtonDown
        | MouseEventKind::MiddleButtonUp
        | MouseEventKind::MiddleButtonDoubleClick => Some(2),
        MouseEventKind::RightButtonDown
        | MouseEventKind::RightButtonUp
        | MouseEventKind::RightButtonDoubleClick => Some(3),
        _ => None,
    }
}

/// Is this mouse kind a button-down event (single or double-click)?
pub(super) fn is_mouse_down(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::LeftButtonDown
            | MouseEventKind::MiddleButtonDown
            | MouseEventKind::RightButtonDown
            | MouseEventKind::LeftButtonDoubleClick
            | MouseEventKind::MiddleButtonDoubleClick
            | MouseEventKind::RightButtonDoubleClick
    )
}

/// `press_count` for `wpe_event_pointer_button_new`: 2 for double-clicks,
/// 1 for single-click presses, 0 (i.e. unused) for releases.
pub(super) fn mouse_press_count(kind: MouseEventKind) -> u32 {
    match kind {
        MouseEventKind::LeftButtonDoubleClick
        | MouseEventKind::MiddleButtonDoubleClick
        | MouseEventKind::RightButtonDoubleClick => 2,
        MouseEventKind::LeftButtonDown
        | MouseEventKind::MiddleButtonDown
        | MouseEventKind::RightButtonDown => 1,
        _ => 0,
    }
}

/// For `MouseEventKind::Wheel` / `HorizontalWheel`: return `(dx, dy)`
/// from scrying's `mouse_data` field (signed). Returns `(0.0, 0.0)`
/// for non-wheel kinds.
pub(super) fn wheel_deltas(ev: &MouseInput) -> (f64, f64) {
    match ev.kind {
        MouseEventKind::Wheel => (0.0, ev.mouse_data as f64),
        MouseEventKind::HorizontalWheel => (ev.mouse_data as f64, 0.0),
        _ => (0.0, 0.0),
    }
}

/// Translate scrying's pointer event into a WPEEventType + a "is move"
/// flag. Returns `None` for pointer kinds we don't dispatch
/// (Enter/Leave/CaptureChanged — those aren't input dispatch in WPE's
/// model: WPE emits enter/leave; hosts don't send them).
pub(super) fn pointer_kind_to_wpe(kind: PointerEventKind) -> Option<(i32, bool)> {
    // (wpe-type, is_move)
    match kind {
        PointerEventKind::Update => Some((ffi::WPE_EVENT_POINTER_MOVE, true)),
        PointerEventKind::Down | PointerEventKind::Activate => {
            Some((ffi::WPE_EVENT_POINTER_DOWN, false))
        }
        PointerEventKind::Up => Some((ffi::WPE_EVENT_POINTER_UP, false)),
        _ => None,
    }
}

// ============================================================================
// Dispatch entry points — called from the WpeProducer trait impl.
// ============================================================================

/// Build the WPEModifiers bitmask from scrying's `KeyModifierFlags`.
/// Consumed by `dispatch_keyboard`; separated so it's testable without FFI.
fn modifier_flags(m: crate::KeyModifierFlags) -> u32 {
    let mut bits: u32 = 0;
    if m.control  { bits |= ffi::WPE_MODIFIER_KEYBOARD_CONTROL; }
    if m.shift    { bits |= ffi::WPE_MODIFIER_KEYBOARD_SHIFT; }
    if m.alt      { bits |= ffi::WPE_MODIFIER_KEYBOARD_ALT; }
    if m.meta     { bits |= ffi::WPE_MODIFIER_KEYBOARD_META; }
    if m.caps_lock { bits |= ffi::WPE_MODIFIER_KEYBOARD_CAPS_LOCK; }
    bits
}

/// WPE scroll event type tag — `wpe_event_scroll_new` does not take a
/// type discriminant parameter (scroll type is implicit in the API), so
/// this constant is referenced via `_` to keep the symbol table complete
/// alongside the other WPEEventType values.
#[allow(dead_code)]
pub(super) const WPE_SCROLL_TYPE: i32 = ffi::WPE_EVENT_SCROLL;

/// SAFETY: `view` must be a non-null `WPEView*` valid for the current
/// call. The constructed event is consumed by `wpe_view_event`
/// (transfer-full); we don't unref it ourselves.
pub(super) unsafe fn dispatch_keyboard(view: *mut ffi::WPEView, ev: &KeyboardInput) {
    let ty = match ev.kind {
        KeyEventKind::Down => ffi::WPE_EVENT_KEYBOARD_KEY_DOWN,
        KeyEventKind::Up => ffi::WPE_EVENT_KEYBOARD_KEY_UP,
        // ModifiersChanged is a state notification; WPE has no direct
        // "modifiers-only" event. Skip the dispatch (the next real key
        // event carries the updated modifier mask anyway).
        KeyEventKind::ModifiersChanged => return,
    };
    // EMPIRICAL (4c.4 MVP): keyval = 0; rely on WebKit to derive from
    // keycode internally. If a future test (4c.5 JS-message-back) shows
    // text input doesn't reach the page, derive keyval from
    // ev.characters or via xkb.
    let keycode = ev.virtual_key_code;
    let keyval: u32 = 0;
    let modifiers = modifier_flags(ev.modifiers);
    let evt = unsafe {
        ffi::wpe_event_keyboard_new(
            ty, view, ffi::WPE_INPUT_SOURCE_KEYBOARD, 0, modifiers, keycode, keyval,
        )
    };
    if !evt.is_null() {
        unsafe { ffi::wpe_view_event(view, evt) };
    }
}

/// SAFETY: same view validity contract as `dispatch_keyboard`.
pub(super) unsafe fn dispatch_mouse(view: *mut ffi::WPEView, ev: &MouseInput) {
    let (x, y) = (ev.point.0 as f64, ev.point.1 as f64);
    match ev.kind {
        MouseEventKind::Wheel | MouseEventKind::HorizontalWheel => {
            let (dx, dy) = wheel_deltas(ev);
            let evt = unsafe {
                ffi::wpe_event_scroll_new(
                    view,
                    ffi::WPE_INPUT_SOURCE_MOUSE,
                    0, // time
                    0, // modifiers
                    dx,
                    dy,
                    0, // not precise
                    0, // not stop
                    x,
                    y,
                )
            };
            if !evt.is_null() {
                unsafe { ffi::wpe_view_event(view, evt) };
            }
        }
        MouseEventKind::Move => {
            let evt = unsafe {
                ffi::wpe_event_pointer_move_new(
                    ffi::WPE_EVENT_POINTER_MOVE,
                    view,
                    ffi::WPE_INPUT_SOURCE_MOUSE,
                    0, // time
                    0, // modifiers
                    x,
                    y,
                    0.0, // delta_x
                    0.0, // delta_y
                )
            };
            if !evt.is_null() {
                unsafe { ffi::wpe_view_event(view, evt) };
            }
        }
        kind => {
            if let Some(button) = mouse_button_index(kind) {
                let ty = if is_mouse_down(kind) {
                    ffi::WPE_EVENT_POINTER_DOWN
                } else {
                    ffi::WPE_EVENT_POINTER_UP
                };
                let press_count = mouse_press_count(kind);
                let evt = unsafe {
                    ffi::wpe_event_pointer_button_new(
                        ty,
                        view,
                        ffi::WPE_INPUT_SOURCE_MOUSE,
                        0, // time
                        0, // modifiers
                        button,
                        x,
                        y,
                        press_count,
                    )
                };
                if !evt.is_null() {
                    unsafe { ffi::wpe_view_event(view, evt) };
                }
            }
            // X-buttons and other kinds: silently no-op (MVP scope).
        }
    }
}

/// SAFETY: same view validity contract. Returns Err on Touch device
/// (deferred to 4c.4.1).
pub(super) unsafe fn dispatch_pointer(
    view: *mut ffi::WPEView,
    ev: &PointerInput,
) -> Result<(), WebSurfaceError> {
    if ev.device == PointerDevice::Touch {
        return Err(WebSurfaceError::Unsupported(
            "WPE touch input not yet implemented; 4c.4.1 follow-up",
        ));
    }
    let (x, y) = (ev.point.0 as f64, ev.point.1 as f64);
    let Some((ty, is_move)) = pointer_kind_to_wpe(ev.kind) else {
        return Ok(()); // Enter/Leave/CaptureChanged silently no-op
    };
    let source = match ev.device {
        PointerDevice::Pen => ffi::WPE_INPUT_SOURCE_PEN,
        // Touch is rejected above; Mouse is the remaining case.
        _ => ffi::WPE_INPUT_SOURCE_MOUSE,
    };
    let evt = if is_move {
        unsafe { ffi::wpe_event_pointer_move_new(ty, view, source, 0, 0, x, y, 0.0, 0.0) }
    } else {
        // Button 1 (left) for Activate/Down; Up uses the same button id.
        // Press count: 1 for Down/Activate, 0 for Up.
        let press_count = if ty == ffi::WPE_EVENT_POINTER_DOWN { 1 } else { 0 };
        unsafe {
            ffi::wpe_event_pointer_button_new(ty, view, source, 0, 0, 1, x, y, press_count)
        }
    };
    if !evt.is_null() {
        unsafe { ffi::wpe_view_event(view, evt) };
    }
    Ok(())
}

// ============================================================================
// Pure-Rust unit tests for the translation helpers (no display needed).
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_button_index_left_middle_right() {
        assert_eq!(mouse_button_index(MouseEventKind::LeftButtonDown), Some(1));
        assert_eq!(mouse_button_index(MouseEventKind::MiddleButtonDown), Some(2));
        assert_eq!(mouse_button_index(MouseEventKind::RightButtonDown), Some(3));
    }

    #[test]
    fn mouse_button_index_double_clicks_match_single() {
        assert_eq!(
            mouse_button_index(MouseEventKind::LeftButtonDoubleClick),
            Some(1)
        );
    }

    #[test]
    fn mouse_button_index_non_button_kinds_return_none() {
        assert_eq!(mouse_button_index(MouseEventKind::Move), None);
        assert_eq!(mouse_button_index(MouseEventKind::Wheel), None);
        assert_eq!(mouse_button_index(MouseEventKind::XButtonDown), None);
    }

    #[test]
    fn is_mouse_down_classifies_correctly() {
        assert!(is_mouse_down(MouseEventKind::LeftButtonDown));
        assert!(is_mouse_down(MouseEventKind::MiddleButtonDoubleClick));
        assert!(!is_mouse_down(MouseEventKind::LeftButtonUp));
        assert!(!is_mouse_down(MouseEventKind::Move));
    }

    #[test]
    fn press_count_double_is_two_single_is_one_release_is_zero() {
        assert_eq!(mouse_press_count(MouseEventKind::LeftButtonDoubleClick), 2);
        assert_eq!(mouse_press_count(MouseEventKind::LeftButtonDown), 1);
        assert_eq!(mouse_press_count(MouseEventKind::LeftButtonUp), 0);
        assert_eq!(mouse_press_count(MouseEventKind::Move), 0);
    }

    #[test]
    fn wheel_deltas_vertical_and_horizontal() {
        let v = MouseInput {
            kind: MouseEventKind::Wheel,
            virtual_keys: Default::default(),
            mouse_data: 120,
            point: (0, 0),
        };
        assert_eq!(wheel_deltas(&v), (0.0, 120.0));
        let h = MouseInput {
            kind: MouseEventKind::HorizontalWheel,
            virtual_keys: Default::default(),
            mouse_data: -40,
            point: (0, 0),
        };
        assert_eq!(wheel_deltas(&h), (-40.0, 0.0));
        let m = MouseInput {
            kind: MouseEventKind::Move,
            virtual_keys: Default::default(),
            mouse_data: 999,
            point: (0, 0),
        };
        assert_eq!(wheel_deltas(&m), (0.0, 0.0));
    }

    #[test]
    fn pointer_kind_translation() {
        assert_eq!(
            pointer_kind_to_wpe(PointerEventKind::Update),
            Some((ffi::WPE_EVENT_POINTER_MOVE, true))
        );
        assert_eq!(
            pointer_kind_to_wpe(PointerEventKind::Down),
            Some((ffi::WPE_EVENT_POINTER_DOWN, false))
        );
        assert_eq!(
            pointer_kind_to_wpe(PointerEventKind::Up),
            Some((ffi::WPE_EVENT_POINTER_UP, false))
        );
        assert_eq!(pointer_kind_to_wpe(PointerEventKind::Enter), None);
        assert_eq!(pointer_kind_to_wpe(PointerEventKind::Leave), None);
        assert_eq!(pointer_kind_to_wpe(PointerEventKind::CaptureChanged), None);
    }
}

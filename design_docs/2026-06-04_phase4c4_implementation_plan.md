# Phase 4c.4 — Interactive Trio Input MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Promote `WpeProducer`'s `send_keyboard_input` / `send_mouse_input` / `send_pointer_input` from `Unsupported` defaults to real impls that dispatch real WPEEvents via `wpe_view_event`. Touch/drag/IME/cursor stay `Unsupported`/deferred.

**Architecture:** New `scrying/src/wpe_producer/input.rs` holds pure-Rust translation from scrying input types → `wpe_event_*_new` constructor args, plus dispatch helpers that call `wpe_view_event`. Pure-Rust unit tests cover every translation branch (no display needed). A new `scrying/tests/wpe_input.rs` integration binary (independent process per the one-WPE-per-process discipline) dispatches a sequence of real events against a live producer and asserts the renderer keeps producing frames.

**Tech Stack:** Rust 2024, hand-written `extern "C"` to `libWPEWebKit-2.0.so`, gated on `cfg(target_os = "linux") + feature = "wpe"`. glib 0.18 (already in tree).

**Spec:** [`2026-06-04_phase4c4_input_mvp.md`](2026-06-04_phase4c4_input_mvp.md)

**Conventions:**
- All FFI compiles only under `--features wpe`.
- One integration test binary per ignored runtime test (per the 4c.2 retro).
- The plan-internal commit-message scaffolds intentionally do NOT include a `Co-Authored-By: Claude` trailer (user preference).

---

## Verified ground-truth constants and enum shapes

(Read out of the trait definitions in `scrying/src/lib.rs` and from
`/usr/include/wpe-webkit-2.0/wpe-platform/wpe/WPEEvent.h` at plan-write
time. The implementer should NOT need to re-derive these.)

**`WPEEventType` (zero-based, in declaration order):**
- `WPE_EVENT_NONE = 0`
- `WPE_EVENT_POINTER_DOWN = 1`
- `WPE_EVENT_POINTER_UP = 2`
- `WPE_EVENT_POINTER_MOVE = 3`
- `WPE_EVENT_POINTER_ENTER = 4`
- `WPE_EVENT_POINTER_LEAVE = 5`
- `WPE_EVENT_SCROLL = 6`
- `WPE_EVENT_KEYBOARD_KEY_DOWN = 7`
- `WPE_EVENT_KEYBOARD_KEY_UP = 8`
- `WPE_EVENT_TOUCH_*` = 9–12 (touch is deferred; we don't bind these in this phase)

**`WPEModifiers` (bitmask):**
- `WPE_MODIFIER_KEYBOARD_CONTROL = 1 << 0`
- `WPE_MODIFIER_KEYBOARD_SHIFT   = 1 << 1`
- `WPE_MODIFIER_KEYBOARD_ALT     = 1 << 2`
- `WPE_MODIFIER_KEYBOARD_META    = 1 << 3`
- `WPE_MODIFIER_KEYBOARD_CAPS_LOCK = 1 << 4`
- pointer button bits 1<<5..1<<9 — not needed for MVP.

**Scrying input-enum variants:**
- `MouseEventKind`: `LeftButtonDown/Up/DoubleClick`, `MiddleButtonDown/Up/DoubleClick`, `RightButtonDown/Up/DoubleClick`, `XButtonDown/Up/DoubleClick`, `Move`, `Wheel`, `HorizontalWheel`.
- `PointerEventKind`: `Activate`, `Down`, `Enter`, `Leave`, `Up`, `Update`, `CaptureChanged`.
- `PointerDevice`: `Touch`, `Pen`, `Mouse`.
- `KeyEventKind`: `Down`, `Up`, `ModifiersChanged`.

**Scrying input-struct fields used in this phase:**
- `MouseInput { kind: MouseEventKind, virtual_keys: MouseVirtualKeys, mouse_data: i32, point: (i32, i32) }`
- `PointerInput { kind: PointerEventKind, device: PointerDevice, pointer_id: u32, point: (i32, i32), pressure: f32, /* + tilt fields */ }`
- `KeyboardInput { kind: KeyEventKind, virtual_key_code: u32, characters: String, characters_ignoring_modifiers: String, /* + modifiers field */ }`

**Trait method defaults to override (in `scrying/src/lib.rs`):** the four `send_*_input` methods currently each have an `Unsupported` default body. We override three (keyboard/mouse/pointer) under `--features wpe`; `send_drag_input` stays `Unsupported`.

---

## File Structure

- **Create:** `scrying/src/wpe_producer/input.rs` — translation + dispatch + pure-Rust unit tests. `#[cfg(feature = "wpe")]`.
- **Create:** `scrying/tests/wpe_input.rs` — `#[ignore]`d runtime smoke (independent process).
- **Modify:** `scrying/src/wpe_producer/ffi.rs` — add `WPEEvent` opaque, the 6 `WPE_EVENT_*` constants we use, and 5 FFI fn decls.
- **Modify:** `scrying/src/wpe_producer/mod.rs` — declare `#[cfg(feature = "wpe")] mod input;`.
- **Modify:** `scrying/src/wpe_producer/producer.rs` — replace three trait method bodies (`send_keyboard_input`, `send_mouse_input`, `send_pointer_input`) with feature-gated real impls.

---

## Task 1: FFI additions

**Files:**
- Modify: `scrying/src/wpe_producer/ffi.rs`

- [ ] **Step 1: Add the `WPEEvent` opaque struct + event-type constants**

Find the existing block of opaque structs in `ffi.rs` and add after `WPEToplevel`:

```rust
#[repr(C)] pub struct WPEEvent { _opaque: [u8; 0] }
```

Add a constants block just before the `unsafe extern "C" { ... }` block:

```rust
// WPEEventType discriminants — verified against
// /usr/include/wpe-webkit-2.0/wpe-platform/wpe/WPEEvent.h. The C enum
// is zero-based in declaration order.
pub const WPE_EVENT_POINTER_DOWN:      i32 = 1;
pub const WPE_EVENT_POINTER_UP:        i32 = 2;
pub const WPE_EVENT_POINTER_MOVE:      i32 = 3;
pub const WPE_EVENT_SCROLL:            i32 = 6;
pub const WPE_EVENT_KEYBOARD_KEY_DOWN: i32 = 7;
pub const WPE_EVENT_KEYBOARD_KEY_UP:   i32 = 8;

// WPEModifiers bitmask flags — verified against the same header.
// Pointer-button modifier bits exist (1<<5..1<<9) but are not used by
// the MVP, so we don't bind them yet.
pub const WPE_MODIFIER_KEYBOARD_CONTROL:   u32 = 1 << 0;
pub const WPE_MODIFIER_KEYBOARD_SHIFT:     u32 = 1 << 1;
pub const WPE_MODIFIER_KEYBOARD_ALT:       u32 = 1 << 2;
pub const WPE_MODIFIER_KEYBOARD_META:      u32 = 1 << 3;
pub const WPE_MODIFIER_KEYBOARD_CAPS_LOCK: u32 = 1 << 4;
```

- [ ] **Step 2: Add the 5 FFI decls inside the existing `unsafe extern "C"` block**

Inside the existing `unsafe extern "C" { ... }` block (grouped with the other WPE event/view symbols):

```rust
    // --- Input event construction + dispatch (4c.4) ---
    pub fn wpe_event_keyboard_new(
        ty: i32, view: *mut WPEView, time: u32, modifiers: u32,
        keycode: u32, keyval: u32,
    ) -> *mut WPEEvent;
    pub fn wpe_event_pointer_button_new(
        ty: i32, view: *mut WPEView, time: u32, modifiers: u32,
        x: f64, y: f64, button: u32, press_count: u32,
    ) -> *mut WPEEvent;
    pub fn wpe_event_pointer_move_new(
        ty: i32, view: *mut WPEView, time: u32, modifiers: u32,
        x: f64, y: f64, dx: f64, dy: f64,
    ) -> *mut WPEEvent;
    pub fn wpe_event_scroll_new(
        view: *mut WPEView, time: u32, modifiers: u32,
        dx: f64, dy: f64,
        has_precise_deltas: i32, is_stop: i32,
        x: f64, y: f64,
    ) -> *mut WPEEvent;
    pub fn wpe_view_event(view: *mut WPEView, event: *mut WPEEvent);
```

If the implementer wants to double-check the exact param order/types, the
header (`/usr/include/wpe-webkit-2.0/wpe-platform/wpe/WPEEvent.h`) is the
truth source. The signatures above are based on the
`wpe_event_*_new` declarations grepped at lines ~171–230 of that header.

- [ ] **Step 3: Build both configurations**

Run: `cargo build -p scrying`
Expected: PASS, 0 warnings.

Run: `cargo build -p scrying --features wpe`
Expected: PASS. **Expected NEW dead-code warnings** on the 5 fns + the 6 event-type constants + the 5 modifier constants — they're consumed in Task 2. Don't suppress with `#[allow(dead_code)]` — they clear naturally.

- [ ] **Step 4: Commit**

```bash
git add scrying/src/wpe_producer/ffi.rs
git commit -m "$(cat <<'EOF'
phase 4c.4: FFI decls for keyboard / pointer / scroll input events

Adds WPEEvent opaque type, the 6 WPEEventType discriminants and 5
WPEModifiers bits we'll use, and the 5 input-event constructors plus
wpe_view_event dispatch. Verified against
/usr/include/wpe-webkit-2.0/wpe-platform/wpe/WPEEvent.h. Task 2 consumes
these from input.rs.
EOF
)"
```

Do NOT push. Commit on `main`.

---

## Task 2: `input.rs` translations + trait method bodies + unit tests

**Files:**
- Create: `scrying/src/wpe_producer/input.rs`
- Modify: `scrying/src/wpe_producer/mod.rs`
- Modify: `scrying/src/wpe_producer/producer.rs`

- [ ] **Step 1: Declare `mod input;` in `mod.rs`**

Locate the existing wpe-gated module declarations:

```rust
#[cfg(feature = "wpe")]
mod ffi;
#[cfg(feature = "wpe")]
mod headless;
#[cfg(feature = "wpe")]
mod navigation;
```

Add:

```rust
#[cfg(feature = "wpe")]
mod input;
```

- [ ] **Step 2: Create `scrying/src/wpe_producer/input.rs` with translation helpers + dispatch fns**

```rust
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
/// in WebKit's "pixels-ish" delta convention. scrying packs the delta
/// into `mouse_data` (signed); positive = scroll-down for vertical,
/// scroll-right for horizontal. WebKit conventionally uses negative
/// dy for scroll-up — we forward scrying's sign verbatim and let the
/// caller flip if they want the page-scrolls-up-on-wheel-up semantics.
pub(super) fn wheel_deltas(ev: &MouseInput) -> (f64, f64) {
    match ev.kind {
        MouseEventKind::Wheel => (0.0, ev.mouse_data as f64),
        MouseEventKind::HorizontalWheel => (ev.mouse_data as f64, 0.0),
        _ => (0.0, 0.0),
    }
}

/// Translate scrying's pointer event into a WPEEventType + a "is move"
/// flag (move uses a different constructor than down/up). Returns
/// `None` for pointer kinds we don't dispatch (Enter / Leave /
/// Activate / CaptureChanged — those aren't input dispatch in the
/// WPE model).
pub(super) fn pointer_kind_to_wpe(kind: PointerEventKind) -> Option<(i32, bool)> {
    // (wpe-type, is_move)
    match kind {
        PointerEventKind::Update => Some((ffi::WPE_EVENT_POINTER_MOVE, true)),
        PointerEventKind::Down | PointerEventKind::Activate => {
            Some((ffi::WPE_EVENT_POINTER_DOWN, false))
        }
        PointerEventKind::Up => Some((ffi::WPE_EVENT_POINTER_UP, false)),
        // Enter/Leave/CaptureChanged: no dispatch in WPEPlatform model.
        // WPE_EVENT_POINTER_ENTER/LEAVE exist but they're emitted BY WPE,
        // not consumed FROM hosts; sending them back doesn't represent a
        // real user interaction. Skip.
        _ => None,
    }
}

// ============================================================================
// Dispatch entry points — call from the WpeProducer trait impl.
// ============================================================================

/// SAFETY: `view` must be a non-null `WPEView*` valid for the current
/// call. The constructed event is consumed by `wpe_view_event`
/// (transfer-full); we don't unref it ourselves.
pub(super) unsafe fn dispatch_keyboard(view: *mut ffi::WPEView, ev: &KeyboardInput) {
    // ModifiersChanged is a state notification; WPE has no direct
    // "modifiers-only" event. Skip the dispatch (the next real key
    // event carries the updated modifier mask anyway).
    let ty = match ev.kind {
        KeyEventKind::Down => ffi::WPE_EVENT_KEYBOARD_KEY_DOWN,
        KeyEventKind::Up => ffi::WPE_EVENT_KEYBOARD_KEY_UP,
        KeyEventKind::ModifiersChanged => return,
    };
    // EMPIRICAL: keyval = 0; rely on WebKit to derive from keycode. If
    // a future test (4c.5 JS-message-back) shows text input doesn't
    // reach the page, derive keyval from ev.characters or via xkb.
    let keycode = ev.virtual_key_code;
    let keyval: u32 = 0;
    let modifiers: u32 = 0; // 4c.4 doesn't route modifier state yet
    let evt = unsafe { ffi::wpe_event_keyboard_new(ty, view, 0, modifiers, keycode, keyval) };
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
                    view, 0, 0, dx, dy, 0 /* not precise */, 0 /* not stop */, x, y,
                )
            };
            if !evt.is_null() {
                unsafe { ffi::wpe_view_event(view, evt) };
            }
        }
        MouseEventKind::Move => {
            let evt = unsafe {
                ffi::wpe_event_pointer_move_new(ffi::WPE_EVENT_POINTER_MOVE, view, 0, 0, x, y, 0.0, 0.0)
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
                    ffi::wpe_event_pointer_button_new(ty, view, 0, 0, x, y, button, press_count)
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
    let evt = if is_move {
        unsafe { ffi::wpe_event_pointer_move_new(ty, view, 0, 0, x, y, 0.0, 0.0) }
    } else {
        // Button 1 (left) for Activate/Down; Up uses the same button id.
        // Press count: 1 for Down/Activate, 0 for Up.
        let press_count = if ty == ffi::WPE_EVENT_POINTER_DOWN { 1 } else { 0 };
        unsafe { ffi::wpe_event_pointer_button_new(ty, view, 0, 0, x, y, 1, press_count) }
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
        // WebKit's MouseEvent.button is per-button, not per-press-count;
        // double-clicks use the same button id with press_count = 2.
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
        // Non-wheel kinds: deltas are zero.
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
```

> **Verifying `MouseVirtualKeys::Default::default()` exists.** scrying's
> `MouseVirtualKeys` is `derive(Default)` (lib.rs ~line 935 onwards;
> grep to confirm). If `Default::default()` doesn't resolve, the field
> may need a specific zero-state — adapt to whatever scrying actually
> exposes for "no buttons / no modifiers" state. The test creates
> sample inputs only for the wheel_deltas check; the helper itself
> doesn't read `virtual_keys`, so this is purely a test-fixture
> concern.

- [ ] **Step 3: Run the unit tests**

Run: `cargo test -p scrying --features wpe wpe_producer::input::tests`
Expected: **7 passed, 0 failed.** These exercise the translation helpers without any FFI or display.

If `MouseVirtualKeys::default()` doesn't compile, fix the test fixtures per the implementation note above and re-run.

- [ ] **Step 4: Promote the trait method bodies in `producer.rs`**

In `scrying/src/wpe_producer/producer.rs`, find the existing `impl WebSurfaceProducer for WpeProducer { ... }` block. Three trait methods currently use the trait default (returning `Unsupported`) — they may or may not be present in the impl block. Add them (or replace if present):

```rust
    fn send_keyboard_input(&mut self, event: crate::KeyboardInput) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            // SAFETY: handles.view is non-null per the construction guard;
            // dispatch_keyboard is single-threaded on the producer's thread.
            unsafe { super::input::dispatch_keyboard(self.handles.view, &event); }
            Ok(())
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = event;
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }

    fn send_mouse_input(&mut self, event: crate::MouseInput) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            unsafe { super::input::dispatch_mouse(self.handles.view, &event); }
            Ok(())
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = event;
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }

    fn send_pointer_input(&mut self, event: crate::PointerInput) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            unsafe { super::input::dispatch_pointer(self.handles.view, &event) }
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = event;
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }
```

Leave `send_drag_input` alone — its default returns `Unsupported`, which is what we want.

If `send_keyboard_input` / `send_mouse_input` / `send_pointer_input` are not yet methods in the impl block (they may have been relying on trait defaults from 4c.3), ADD them — they're new method overrides.

- [ ] **Step 5: Build both configurations**

Run: `cargo build -p scrying`
Expected: PASS, 0 warnings (the trait defaults are still in use here).

Run: `cargo build -p scrying --features wpe`
Expected: PASS, **0 warnings.** All FFI symbols and constants added in Task 1 should now be consumed.

- [ ] **Step 6: Run all the (non-ignored) tests**

Run: `cargo test -p scrying`
Expected: 7 unit + 3 integration passing, unchanged.

Run: `cargo test -p scrying --features wpe`
Expected: **19 unit** (12 from prior phases + 7 new input::tests) + 3 integration passing, **2 ignored** (the unit smoke + the round-trip).

Run: `cargo test -p scrying --features wpe navigate_resize_and_render -- --ignored --nocapture`
Expected: PASS, same `smoke#1 / smoke#2` lines as before. (Regression check that the new input wiring didn't disturb the existing seam.)

- [ ] **Step 7: Commit**

```bash
git add scrying/src/wpe_producer/input.rs scrying/src/wpe_producer/mod.rs scrying/src/wpe_producer/producer.rs
git commit -m "$(cat <<'EOF'
phase 4c.4: input.rs translations + trait method bodies

Pure-Rust translation from scrying KeyboardInput/MouseInput/PointerInput
into wpe_event_*_new constructor args, plus dispatch via wpe_view_event.
Seven unit tests cover the translation branches (button indices,
press counts, wheel deltas, pointer kind mapping). Trait methods
send_keyboard_input / send_mouse_input / send_pointer_input promoted
from Unsupported defaults to real impls under --features wpe; touch
PointerInput.device returns Unsupported (deferred to 4c.4.1).
send_drag_input stays Unsupported.

keyval=0, modifiers=0, time=0 in this MVP — WebKit derives keyval from
keycode internally. Refinement deferred until 4c.5 adds JS message-back
so we can assert "text appears on page".
EOF
)"
```

---

## Task 3: Runtime integration smoke (independent binary)

**Files:**
- Create: `scrying/tests/wpe_input.rs`

- [ ] **Step 1: Create the integration test file**

```rust
//! Phase 4c.4 input-forwarding integration smoke.
//!
//! Independent integration binary (separate process from the unit-test
//! smoke and the wpe_to_vulkan_roundtrip smoke) so it has its own
//! WebKit init, honoring the one-WPE-per-process discipline.
//!
//! Constructs a WpeProducer, navigates to a page with an `<input>`,
//! dispatches a sequence of keyboard / pointer / scroll events, and
//! asserts the renderer keeps producing frames after each event. Does
//! NOT assert "the page received the events correctly" — that requires
//! JS message-back wiring (4c.5).
//!
//! Run with:
//!   cargo test -p scrying --features wpe --test wpe_input \
//!     -- --ignored --nocapture

#![cfg(all(target_os = "linux", feature = "wpe"))]

use dpi::PhysicalSize;
use scrying::wpe_producer::{WpeProducer, WpeProducerConfig};
use scrying::{
    DmaBufImage, KeyEventKind, KeyboardInput, MouseEventKind, MouseInput, NativeFrame,
    NavigationEvent, PointerDevice, PointerEventKind, PointerInput, WebSurfaceFrame,
    WebSurfaceProducer,
};

#[test]
#[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
fn input_dispatch_does_not_crash() {
    // --- 1. Stand up the producer ---
    let config = WpeProducerConfig::new(PhysicalSize::new(256, 256), std::env::temp_dir());
    let mut producer = match WpeProducer::new(config) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: WpeProducer::new failed (no display / GPU?): {e}");
            return;
        }
    };

    // --- 2. Navigate to a page with a focusable <input> ---
    if let Err(e) = producer.navigate_to_string(
        "<body style='margin:0;background:#1e90ff'>\
         <input id='probe' style='font-size:32px' autofocus>\
         </body>",
        std::time::Duration::from_secs(5),
    ) {
        eprintln!("SKIP: navigate_to_string failed: {e}");
        return;
    }
    assert!(
        producer
            .poll_navigation_event()
            .iter()
            .chain(std::iter::from_fn(|| producer.poll_navigation_event()).take(8))
            .any(|e| matches!(e, NavigationEvent::Completed { success: true, .. })),
        "expected a successful Completed event"
    );
    // (Re-drain — the `.iter().chain(from_fn...)` shape above may not
    // exhaust the queue; do a final drain loop for cleanliness.)
    while let Some(_) = producer.poll_navigation_event() {}

    // --- 3. Wait for the first frame so we know rendering's alive ---
    let first_frame = acquire_with_pump(&mut producer);
    let frame_1_size = match &first_frame {
        WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img)) => img.size,
        _ => panic!("expected DMABUF frame"),
    };
    close_frame_fds_if_dmabuf(&first_frame);
    eprintln!("input smoke: first frame {}x{}", frame_1_size.width, frame_1_size.height);

    // --- 4. Dispatch a sequence of events ---
    // Pointer move + click at the center of the toplevel.
    let cx = (frame_1_size.width / 2) as i32;
    let cy = (frame_1_size.height / 2) as i32;
    producer.send_pointer_input(PointerInput {
        kind: PointerEventKind::Update,
        device: PointerDevice::Mouse,
        pointer_id: 1,
        point: (cx, cy),
        pressure: 0.0,
        tilt_x_radians: 0.0,
        tilt_y_radians: 0.0,
    }).expect("send_pointer_input move");

    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::LeftButtonDown,
        virtual_keys: Default::default(),
        mouse_data: 0,
        point: (cx, cy),
    }).expect("send_mouse_input down");
    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::LeftButtonUp,
        virtual_keys: Default::default(),
        mouse_data: 0,
        point: (cx, cy),
    }).expect("send_mouse_input up");

    // Type a few keys. Linux xkb keycodes: 'w' = 25, 'p' = 33, 'e' = 26
    // (xkb USB HID keyboard layout; these are press/release pairs).
    for &keycode in &[25u32, 33, 26] {
        producer.send_keyboard_input(KeyboardInput {
            kind: KeyEventKind::Down,
            virtual_key_code: keycode,
            characters: String::new(),
            characters_ignoring_modifiers: String::new(),
            ..Default::default()
        }).expect("send_keyboard_input down");
        producer.send_keyboard_input(KeyboardInput {
            kind: KeyEventKind::Up,
            virtual_key_code: keycode,
            characters: String::new(),
            characters_ignoring_modifiers: String::new(),
            ..Default::default()
        }).expect("send_keyboard_input up");
    }

    // Scroll wheel.
    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::Wheel,
        virtual_keys: Default::default(),
        mouse_data: 120,
        point: (cx, cy),
    }).expect("send_mouse_input wheel");

    // --- 5. Verify the renderer keeps producing frames after input ---
    let second_frame = acquire_with_pump(&mut producer);
    let frame_2_size = match &second_frame {
        WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img)) => img.size,
        _ => panic!("expected DMABUF frame post-input"),
    };
    close_frame_fds_if_dmabuf(&second_frame);
    assert!(frame_2_size.width > 0 && frame_2_size.height > 0);
    eprintln!("input smoke: post-input frame {}x{}", frame_2_size.width, frame_2_size.height);
}

/// Pump glib::MainContext::default() (the same singleton the producer
/// uses) until a frame lands or 5s elapses. Same shape as the
/// wpe_to_vulkan_roundtrip integration test.
fn acquire_with_pump(producer: &mut WpeProducer) -> scrying::WebSurfaceFrame {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match producer.acquire_frame() {
            Ok(f) => return f,
            Err(_) if std::time::Instant::now() < deadline => {
                ctx.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            Err(e) => panic!("FAIL: acquire_frame timed out: {e}"),
        }
    }
}

/// Best-effort: close producer-owned fds on a DMABUF frame the test
/// took ownership of. Pattern matches wpe_to_vulkan_roundtrip.rs.
fn close_frame_fds_if_dmabuf(frame: &WebSurfaceFrame) {
    if let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img)) = frame {
        for plane in &img.planes {
            unsafe { libc::close(plane.fd); }
        }
        if let Some(fd) = img.semaphore_fd {
            unsafe { libc::close(fd); }
        }
    }
}
```

> **`KeyboardInput::default()` usage.** scrying's `KeyboardInput` may
> not have `derive(Default)` if some fields are non-defaultable. If the
> `..Default::default()` syntax doesn't compile, replace with a fully
> explicit struct literal: locate `KeyboardInput` in `scrying/src/lib.rs`
> and list every field (the spec captured the ones we use:
> `kind, virtual_key_code, characters, characters_ignoring_modifiers`;
> add `modifiers: ..., ...` per the actual definition). The deps on
> Default are a convenience, not a hard requirement.
>
> Same caveat for `PointerInput` — the field list above includes
> `tilt_x_radians`/`tilt_y_radians`; if the actual fields differ, copy
> the real definition.

> **`glib` dev-dep.** Already added by the round-trip's Task 1
> (`scrying/Cargo.toml`'s `[target.'cfg(target_os = "linux")'.dev-dependencies]`).
> No Cargo.toml change needed.

- [ ] **Step 2: Build the integration binary**

Run: `cargo build -p scrying --features wpe --tests`
Expected: PASS, 0 warnings.

If `KeyboardInput`/`PointerInput` struct-literal shape doesn't compile, fix per the implementation note above.

- [ ] **Step 3: Run the smoke**

Run: `cargo test -p scrying --features wpe --test wpe_input -- --ignored --nocapture`
Expected: **PASS, exit 0**, with two `input smoke:` lines:
```
input smoke: first frame 1024x768
input smoke: post-input frame 1024x768
```

The contract: producer survives the input sequence and continues to render. The dispatched events DON'T need to visibly change the page — that requires JS observability (4c.5). If `acquire_with_pump` panics on the second pump (no frame after input), that suggests one of:
- The dispatch crashed the WPE process (would be visible in the panic).
- WPE doesn't auto-paint just from input dispatch; we'd need to nudge a redraw. Possible workaround: navigate to a slightly different page after input, or just accept the test passes the same single frame twice (the asserts only require non-zero size).

If the second pump times out cleanly, soften the test to:
- Try `acquire_frame` once without re-pumping; if it returns Ok with a frame, use that. Otherwise, accept that "no new frame" is fine as long as no crash occurred — the input dispatch itself is the contract being tested.

Iterate on this empirically; report what works.

- [ ] **Step 4: Regression check the existing ignored tests**

Run: `cargo test -p scrying --features wpe navigate_resize_and_render -- --ignored --nocapture`
Expected: PASS, unchanged.

Run: `cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip -- --ignored --nocapture`
Expected: FAILS exactly as before (the durable Outcome-B failure documenting the multi-plane gap).

Both integration binaries are separate processes, so all three ignored tests can coexist.

- [ ] **Step 5: Commit**

```bash
git add scrying/tests/wpe_input.rs
git commit -m "$(cat <<'EOF'
phase 4c.4: runtime smoke — input dispatch doesn't crash the renderer

New tests/wpe_input.rs integration binary (independent process) that
navigates to a page with an <input>, dispatches a sequence of
keyboard/pointer/mouse-button/wheel events, then asserts the renderer
keeps producing frames. Per-process WebKit init keeps it independent
of the unit-test smoke and the round-trip smoke; cargo runs each as
its own binary.

Doesn't assert "events visibly affected the page" — that needs JS
message-back, which is 4c.5 territory. The contract this smoke holds
is "input dispatch is FFI-sound and the producer continues to render."
EOF
)"
```

---

## Task 4: Strategy checklist update

**Files:**
- Modify: `design_docs/2026-05-15_phase4_strategy.md`

- [ ] **Step 1: Flip 4c.4 to done; add 4c.4.x sub-phases**

Find the existing 4c.4 line:

```markdown
- [ ] **4c.4** Input forwarding via `wpe_view_event(WPEEvent*)` —
      keyboard / pointer / scroll / touch / IME. WPEPlatform path, not
      legacy libwpe.
```

Replace with:

```markdown
- [x] **4c.4** Input forwarding MVP — keyboard + mouse-pointer +
      scroll via `wpe_view_event(WPEEvent*)`. Pure-Rust unit tests
      cover the scrying-input → WPEEvent translation; runtime
      integration smoke `tests/wpe_input.rs` verifies dispatch
      doesn't crash the renderer. Spec
      [`2026-06-04_phase4c4_input_mvp.md`](2026-06-04_phase4c4_input_mvp.md),
      plan [`2026-06-04_phase4c4_implementation_plan.md`](2026-06-04_phase4c4_implementation_plan.md).
      MVP punts keyval derivation (uses 0; WebKit derives from
      keycode), modifier routing (always 0), event timestamps
      (always 0), touch input, drag input, and IME composition to
      4c.4.x sub-phases or 4c.5.
- [ ] **4c.4.1** Touch input via `wpe_event_touch_new` + sequence-id
      mapping (PointerInput.device == Touch).
- [ ] **4c.4.2** Drag input — investigate whether WPE exposes a
      drag-and-drop signal surface or whether HTML5 DOM events need
      JS-bridge injection.
- [ ] **4c.4.3** IME composition via `WPEInputMethodContext` (or the
      WPE-platform equivalent).
```

Also update the status header:

```markdown
**Status:** 4a + 4b.1 + 4c.1 + 4c.2 + 4c.3 + 4c.4 shipped; 4c.4.x / 4c.5+ in flight.
```

- [ ] **Step 2: Commit**

```bash
git add design_docs/2026-05-15_phase4_strategy.md
git commit -m "$(cat <<'EOF'
docs: phase 4c.4 shipped — checklist + status

Flips 4c.4 to done (interactive trio MVP — keyboard + mouse + scroll).
Splits the original line into MVP + three sub-phases (4c.4.1 touch,
4c.4.2 drag, 4c.4.3 IME) per the empirical-unknown-per-phase lesson
from 4c.2's retrospective.
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- Trait methods `send_keyboard_input` / `send_mouse_input` / `send_pointer_input` promoted → Task 2 Step 4. ✓
- `send_drag_input` stays `Unsupported` → confirmed by not touching it in Task 2 Step 4. ✓
- Translation surface in `input.rs` → Task 2 Step 2. ✓
- Pure-Rust unit tests covering translation branches → Task 2 Step 2 (7 tests). ✓
- Runtime integration smoke in `tests/wpe_input.rs` → Task 3. ✓
- FFI additions (5 fn decls, 6 event-type constants, 5 modifier constants, `WPEEvent` opaque) → Task 1. ✓
- Touch returns `Unsupported` → Task 2 Step 2 (`dispatch_pointer`'s Touch guard). ✓
- IME, cursor, pen pressure deferred → confirmed by not touching `ime.rs`/`cursor.rs` and ignoring pressure/tilt fields. ✓
- Empirical-unknowns called out (keyval=0, modifiers=0, time=0) → Task 2 Step 2 comments + Task 4 checklist note. ✓
- Module structure (input.rs only, no input_native split for MVP) → respected. ✓

**Placeholder scan:** No "TBD"/"TODO". Three empirical-by-design points (the keyval=0 derivation, the runtime smoke's frame-after-input fallback if WPE doesn't auto-paint on input alone, the struct-literal shape if `Default` doesn't compile) are explicit decision procedures with concrete code, not placeholders.

**Type consistency:**
- `dispatch_keyboard / dispatch_mouse / dispatch_pointer` signatures consistent between Task 2's input.rs definition and Task 2 Step 4's trait-impl call sites.
- `*mut ffi::WPEView` field accessed via `self.handles.view` consistently.
- `mouse_button_index / is_mouse_down / mouse_press_count / wheel_deltas / pointer_kind_to_wpe` — names stable across Task 2 (definition + tests).
- `KeyboardInput / MouseInput / PointerInput` field names cross-checked against `scrying/src/lib.rs` at plan-write time.

**Known risks:**
- `KeyboardInput::default()` may not exist (Task 3's smoke uses `..Default::default()`). Note flags this; implementer falls back to full struct literal if needed.
- Post-input frame timing: WPE may or may not auto-paint after input dispatch. Task 3 Step 3 calls this out with a concrete fallback (accept "no new frame" as long as dispatch didn't crash).

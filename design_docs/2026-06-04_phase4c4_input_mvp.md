# Phase 4c.4 — Interactive Trio Input MVP

Implements checklist item **4c.4** from
[`2026-05-15_phase4_strategy.md`](2026-05-15_phase4_strategy.md), scoped
narrowly to the "interactive trio" — keyboard + pointer (mouse) + scroll
— following the 4c.2 retrospective's lesson that multi-empirical-unknown
phases expand. Touch, drag, IME composition, cursor shape, and pen
pressure/tilt are all deferred to 4c.4.x sub-phases or later.

## Scope

In:
- Trait methods `send_keyboard_input`, `send_mouse_input`,
  `send_pointer_input` promoted from `Unsupported` defaults to real
  impls under `--features wpe`.
- Translation surface in a new `scrying/src/wpe_producer/input.rs`:
  scrying input types → WPE event constructions → dispatch via
  `wpe_view_event(view, ev)`.
- Unit-testable pure-Rust translations covering each scrying →
  WPEEvent branch (no display needed).
- Runtime integration smoke in a new `scrying/tests/wpe_input.rs`
  integration binary (separate process, honoring the
  one-WPE-per-process discipline established in 4c.2).

Out (deferred):
- `send_drag_input` — stays `Unsupported`. HTML5 drag/drop on WPE
  doesn't have a direct WPEEvent equivalent; needs its own design.
- Touch input — when `PointerInput.device == Touch`, return
  `Unsupported` for now; WPE has separate `wpe_event_touch_new` API.
- IME composition state — `~156 lines` of GTK precedent in `ime.rs`;
  separate phase.
- Cursor shape changes (output from page, not input) — `wpe_view_set_cursor_from_name`
  exists but is output-direction; separate phase.
- Pen pressure / tilt routing in pointer events.

## Design

### Architecture: one input module + thin FFI

`scrying/src/wpe_producer/input.rs` holds the translation logic and the
trait-impl bodies. Single file is fine for MVP — GTK split into
`input.rs` (235 lines) + `input_native.rs` (358 lines) because GTK
needs an xkb/keymap layer the WPE FFI doesn't. The WPE side is more
direct: scrying gives us a virtual_key_code (xkb keycode on Linux per
the trait doc) which goes straight into `wpe_event_keyboard_new`'s
`keycode` slot.

If 4c.4.x (touch / IME / drag) lands later, that's the time to
consider an `input_native.rs` split — not now.

### FFI additions (in `wpe_producer/ffi.rs`)

```rust
// Opaque event type
#[repr(C)] pub struct WPEEvent { _opaque: [u8; 0] }

// WPEEventType enum constants — exact int values read from
// /usr/include/wpe-webkit-2.0/wpe-platform/wpe/WPEEvent.h at impl time.
// Likely values (zero-based discriminants of a typedef enum):
//   WPE_EVENT_NONE = 0
//   WPE_EVENT_POINTER_DOWN, POINTER_UP, POINTER_MOVE,
//   POINTER_ENTER, POINTER_LEAVE,
//   WPE_EVENT_SCROLL,
//   WPE_EVENT_KEYBOARD_KEY_DOWN, KEYBOARD_KEY_UP,
//   WPE_EVENT_TOUCH_DOWN, TOUCH_UP, TOUCH_MOVE, TOUCH_CANCEL
pub const WPE_EVENT_POINTER_DOWN:     i32 = ...; // verified at impl time
pub const WPE_EVENT_POINTER_UP:       i32 = ...;
pub const WPE_EVENT_POINTER_MOVE:     i32 = ...;
pub const WPE_EVENT_SCROLL:           i32 = ...;
pub const WPE_EVENT_KEYBOARD_KEY_DOWN: i32 = ...;
pub const WPE_EVENT_KEYBOARD_KEY_UP:   i32 = ...;

unsafe extern "C" {
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
}
```

`wpe_view_event` consumes the event (transfer-full of the WPEEvent
ref). We don't `g_object_unref` it ourselves after the dispatch call;
the WPE side handles it. Verified at impl time from the header.

### Translation surface

In `wpe_producer/input.rs`:

```rust
pub(super) fn dispatch_keyboard(view: *mut ffi::WPEView, ev: &KeyboardInput) {
    let (ty, _) = match ev.kind {
        KeyEventKind::Press   => (ffi::WPE_EVENT_KEYBOARD_KEY_DOWN, ()),
        KeyEventKind::Release => (ffi::WPE_EVENT_KEYBOARD_KEY_UP,   ()),
    };
    let modifiers = modifiers_to_wpe(/* derive */ 0);
    let keycode = ev.virtual_key_code;
    let keyval = 0; // EMPIRICAL: start at zero; iterate if text doesn't propagate
    // SAFETY: view valid for producer lifetime; wpe_event_keyboard_new returns
    // transfer-full; wpe_view_event consumes.
    unsafe {
        let evt = ffi::wpe_event_keyboard_new(ty, view, 0, modifiers, keycode, keyval);
        ffi::wpe_view_event(view, evt);
    }
}

pub(super) fn dispatch_mouse(view: *mut ffi::WPEView, ev: &MouseInput) {
    match ev.kind {
        MouseEventKind::Wheel | MouseEventKind::HorizontalWheel => {
            let (dx, dy) = wheel_deltas(ev);
            unsafe {
                let evt = ffi::wpe_event_scroll_new(
                    view, 0, 0, dx, dy, 0 /* not precise */, 0 /* not stop */,
                    ev.point.0 as f64, ev.point.1 as f64,
                );
                ffi::wpe_view_event(view, evt);
            }
        }
        MouseEventKind::LeftDown | MouseEventKind::RightDown | MouseEventKind::MiddleDown => {
            let button = mouse_button_index(ev.kind);
            let press_count = 1;
            unsafe {
                let evt = ffi::wpe_event_pointer_button_new(
                    ffi::WPE_EVENT_POINTER_DOWN, view, 0, 0,
                    ev.point.0 as f64, ev.point.1 as f64, button, press_count,
                );
                ffi::wpe_view_event(view, evt);
            }
        }
        MouseEventKind::LeftUp | MouseEventKind::RightUp | MouseEventKind::MiddleUp => { /* same shape, POINTER_UP */ }
        MouseEventKind::Move => {
            unsafe {
                let evt = ffi::wpe_event_pointer_move_new(
                    ffi::WPE_EVENT_POINTER_MOVE, view, 0, 0,
                    ev.point.0 as f64, ev.point.1 as f64, 0.0, 0.0,
                );
                ffi::wpe_view_event(view, evt);
            }
        }
        // X-buttons, etc — return without dispatching for MVP. (Trait method
        // returns Ok regardless; we don't fail on un-modelled mouse kinds.)
        _ => {}
    }
}

pub(super) fn dispatch_pointer(view: *mut ffi::WPEView, ev: &PointerInput) -> Result<(), WebSurfaceError> {
    if ev.device == PointerDevice::Touch {
        return Err(WebSurfaceError::Unsupported(
            "WPE touch input not yet implemented in 4c.4 MVP",
        ));
    }
    // Move / Down / Up translations parallel dispatch_mouse's. Pressure/tilt
    // ignored for MVP.
    ...
}
```

`modifiers_to_wpe`, `mouse_button_index`, `wheel_deltas` are tiny pure-Rust
helpers — each individually unit-testable.

### Trait method bodies (in `wpe_producer/producer.rs`)

Replace the existing `Unsupported`-returning default-trait shapes for
`send_keyboard_input`, `send_mouse_input`, `send_pointer_input` with
real impls under `--features wpe` that call into `input::dispatch_*`.
Non-wpe build keeps returning `Unsupported`. `send_drag_input` stays
`Unsupported` in both builds.

The dispatch needs `self.handles.view` (the raw `*mut WPEView`).
That's already `pub(super)` in `WpeHandles`, so the new `input.rs`
module can read it.

### Empirical unknowns (Task-2-style spike points)

1. **`keyval = 0` correctness.** Start at zero (WPE may derive from
   keycode internally). If the integration smoke shows text-input
   doesn't reach the page (no DOM mutation observable via, say, a JS
   `oninput` that posts a message back), iterate: derive `keyval` from
   the first char of `KeyboardInput.characters`, or look up the xkb
   keysym for the keycode. We can't easily ASSERT text-was-typed in
   the MVP smoke without JS messaging — the smoke asserts only "no
   crash". So this stays an empirical guess unless 4c.5 wires the
   script-message bridge.

2. **WPEModifiers bit layout.** `WPEModifiers` is a glib flags enum
   in `WPEEvent.h`. Read at impl time. For the MVP, start with
   `modifiers = 0` (no modifiers) on all dispatches; correctness work
   comes when shortcut handling actually matters.

3. **`time = 0` acceptability.** WPE may use `time` for gesture
   recognition (double-clicks, drag thresholds). Start with `0`; if
   the integration smoke reveals double-clicks aren't recognized,
   use a monotonic millisecond clock.

4. **Exact `WPEEventType` int values.** Read from header at impl time;
   `WPE_EVENT_NONE = 0` is the conventional first variant, but order
   should be verified — the header is the truth source.

### Testing

**Pure-Rust unit tests** (in `input.rs` `#[cfg(test)]`):
- `mouse_button_index(LeftDown) == 1`, `RightDown == 2`, `MiddleDown == 3`
  (or whatever WebKit-canonical numbering is — verified against the
  scrying virtual_keys docstring + the GTK precedent).
- `wheel_deltas(MouseInput { kind: Wheel, mouse_data: 120, .. })`
  returns the expected (dx, dy) — convention: vertical scroll, sign
  depends on direction.
- `modifiers_to_wpe(0) == 0` (sanity).
- A "build a keyboard event" test that asserts the args we'd pass to
  `wpe_event_keyboard_new` for a known KeyboardInput (Press, keycode
  38 ('a' in xkb), no modifiers) — exercises the construction path
  without actually calling the FFI (the function accepts a
  test-injected closure / records the args). The test verifies the
  translation, not the dispatch.

**Runtime integration smoke** (new `scrying/tests/wpe_input.rs`):
- Construct `WpeProducer`, navigate to a page that contains an
  `<input>` element (simple inline HTML).
- Pump the main context until the buffer-rendered seam delivers a
  frame (proves the producer is alive).
- Call `producer.send_pointer_input(...)` with a `Move` at the input's
  approximate center (just hard-code a coordinate inside the headless
  toplevel's 1024×768).
- Call `producer.send_mouse_input(...)` with a `LeftDown` then
  `LeftUp` at the same point.
- Call `producer.send_keyboard_input(...)` with a few `Press`+`Release`
  events for keycodes corresponding to "wpe" (a quick word).
- Call `producer.send_mouse_input(...)` with a `Wheel` event.
- Pump for a second post-input frame as evidence the dispatch didn't
  hang or crash the renderer.
- Assert: producer is still alive, second frame is non-zero size,
  buffer-rendered seam still produces frames after input dispatch.

The smoke does NOT assert "the page received the events correctly" —
that would need JS message-back wiring (out of scope; 4c.5 territory).
The contract this smoke holds is **"input dispatch doesn't crash and
the producer continues to render."** That's a meaningful integration
contract on its own — it proves the FFI wiring is sound.

The smoke runs in its own integration binary, separate process from
the unit-test smoke and from the round-trip smoke. Each integration
test is allowed to construct one WPE producer per process.

## File structure

- **Create:** `scrying/src/wpe_producer/input.rs` — translations,
  trait-impl helpers, pure-Rust unit tests. `#[cfg(feature = "wpe")]`.
- **Create:** `scrying/tests/wpe_input.rs` — `#[ignore]`d runtime smoke
  in its own integration binary.
- **Modify:** `scrying/src/wpe_producer/ffi.rs` — add `WPEEvent` opaque,
  5 event constructors, `wpe_view_event` dispatch, the
  `WPE_EVENT_*` int constants.
- **Modify:** `scrying/src/wpe_producer/mod.rs` — declare
  `#[cfg(feature = "wpe")] mod input;`.
- **Modify:** `scrying/src/wpe_producer/producer.rs` — replace four
  trait method bodies (`send_keyboard_input`, `send_mouse_input`,
  `send_pointer_input`, and update the `send_drag_input` stub's
  comment to reference 4c.4.x).

## Anti-scope-creep guards

- Do NOT add `input_native.rs` (only if a future xkb keysym lookup
  needs it — defer to 4c.4.x).
- Do NOT change the `WebSurfaceProducer` trait surface.
- Do NOT add a second `#[ignore]`d test to any existing binary.
- Do NOT touch `headless.rs`, `navigation.rs`, the existing FFI
  navigation surface, or the producer's nav_state/handles structure
  beyond what's mentioned above.

## Followups this informs

- **4c.4.1 — touch input.** When `PointerInput.device == Touch`, use
  `wpe_event_touch_new`. Touch's sequence-id concept (one id per
  finger) maps to scrying's `pointer_id` directly.
- **4c.4.2 — drag input.** Investigate whether WPE has a drag-and-drop
  signal surface or if it needs HTML5-style DOM events injected via
  the JS message bridge.
- **4c.4.3 — IME composition.** Likely a `WPEInputMethodContext` or
  similar; needs its own design.
- **4c.5 — cursor shape changes** (output, not input — moved to a
  different phase from input forwarding).
- **`keyval` derivation when the test gains DOM observability.** Once
  4c.5 wires script-message back-channel, the smoke can assert "the
  input received 'wpe'" and we know whether `keyval = 0` was right.

## Checklist deltas (after this lands)

- [x] **4c.4** Input forwarding MVP (keyboard + pointer/mouse + scroll)
- [ ] **4c.4.1** Touch input via `wpe_event_touch_new`
- [ ] **4c.4.2** Drag input (likely via JS message bridge)
- [ ] **4c.4.3** IME composition via `WPEInputMethodContext`
- [ ] **4c.5** Cookies, schemes, popups, downloads, cursor, IME state
- [ ] **4c.6** `demo-wpe` runtime probe
- [ ] **4c.7** `docs/wpe-deployment.md`
- [ ] **4c.8** Parity matrix + README updates

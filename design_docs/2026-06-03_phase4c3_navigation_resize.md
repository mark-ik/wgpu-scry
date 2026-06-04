# Phase 4c.3 — Navigation + Resize for the WPE Producer

Implements checklist item **4c.3** from
[`2026-05-15_phase4_strategy.md`](2026-05-15_phase4_strategy.md), narrowed
per the 4c.2 retrospective: navigation + resize land in this phase; input
forwarding is bumped to its own follow-on phase (renumbered 4c.4 once that
spec lands).

## Scope

In:
- `WebSurfaceProducer` trait methods: `navigate_to_string`,
  `navigate_to_url`, `resize`, `poll_navigation_event`.
- Inherent producer methods mirroring the GTK shape: `load_html(html,
  base_uri)`, `load_uri(uri)`, `wait_for_load(timeout)`.
- Internal `NavState` driven by `load-changed` / `load-failed` signal
  closures on the WebKitWebView.
- Resize through `WPEToplevel`, with a fail-fast construction guard.

Out (4c.4+):
- Input forwarding (WPEEvent — keyboard, pointer, touch, scroll, IME).
- Cookies, scheme handlers, popups, downloads, cursor, IME state.
- Title / favicon / navigation events beyond
  Started/Committed/Finished/Failed.

## Design

### Architecture: direct port of webkitgtk_producer/navigation.rs

The WPE-side `WebKitWebView` exposes the same `WebKitLoadEvent` enum
(Started/Committed/Finished/Redirected) and the same
`load-changed`/`load-failed` signals as the GTK header. So the
signal-driven `NavState` pattern transfers wholesale; only the resize
path diverges.

### NavState + signal wiring

New module `scrying/src/wpe_producer/navigation.rs`. `#[cfg(feature =
"wpe")]`.

```rust
pub(super) struct NavState {
    pub committed_uri: Option<String>,
    pub finished: bool,
    pub failed: Option<String>,
    pub events: VecDeque<NavigationEvent>,
}
```

Owned by the producer as `Rc<RefCell<NavState>>` — single-threaded (model
A); no `Mutex`/`Arc` overhead needed. The buffer-rendered seam keeps
using `Arc<Mutex<…>>` because that closure was originally drafted as if
it might be invoked off-thread; nav state stays consistent with the GTK
precedent.

`install_load_signals(webview, &nav_state)` connects:
- `load-changed` (carries `WebKitLoadEvent` as `i32` over glib): on
  Started → clear flags, push `NavigationEvent::Starting`. Committed →
  store `committed_uri`, push `SourceChanged`. Finished → set
  `finished`, push `Completed { success: true }`. Redirected → skip
  (no scrying variant).
- `load-failed` → `failed = Some(message)`, `finished = true` (unblocks
  waiters), push `Completed { success: false }`.

Connection uses glib 0.18's `connect_closure` + `closure_local!` — the
same pattern Task 4 proved out for `buffer-rendered`.

### Inherent producer API (mirror GTK)

```rust
impl WpeProducer {
    pub fn load_html(&self, html: &str, base_uri: Option<&str>);
    pub fn load_uri(&self, uri: &str);
    pub fn wait_for_load(&self, timeout: Duration) -> Result<(), WebSurfaceError>;
}
```

`wait_for_load` pumps `self.handles.main_context` with the existing
`pump_until` until `nav_state.borrow().finished`, then returns
`Err(WebSurfaceError::NavigationFailed(msg))` if `failed.is_some()`, else
`Ok(())`.

### Trait methods

```rust
fn navigate_to_string(&mut self, html: &str, timeout: Duration) -> Result<(), _> {
    arm_navigation(&self.nav_state);
    self.load_html(html, None);
    self.wait_for_load(timeout)
}
fn navigate_to_url(&mut self, url: &str, timeout: Duration)  -> Result<(), _> {
    arm_navigation(&self.nav_state);
    self.load_uri(url);
    self.wait_for_load(timeout)
}
fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), _> {
    if size.width == 0 || size.height == 0 { return Err(Platform("non-zero only")); }
    let ok = unsafe { ffi::wpe_toplevel_resize(self.handles.toplevel,
                                                size.width as i32, size.height as i32) };
    if ok == 0 {
        return Err(Platform("wpe_toplevel_resize returned FALSE"));
    }
    self.size = size;
    Ok(())
}
fn poll_navigation_event(&mut self) -> Option<NavigationEvent> {
    self.nav_state.borrow_mut().events.pop_front()
}
```

`arm_navigation(state)` is a small helper that clears
`finished`/`failed`/`committed_uri` before a fresh load.

### Resize: through the toplevel with a fail-fast guard

`build_producer_view` is extended:
1. After the existing display-binding guard, call
   `wpe_view_get_toplevel(view)`. If null → `Err(Platform("WebView has no
   toplevel; resize would always fail"))`. This catches the headless
   no-toplevel case at construction, not lazily on first resize.
2. Return `(webview, view, toplevel)`; `WpeHandles` grows a `toplevel:
   *mut ffi::WPEToplevel` field (held for lifetime; not unref'd here
   since `wpe_view_get_toplevel` is transfer-none — the view owns it).

Resize calls `wpe_toplevel_resize(toplevel, w, h)`. The view's `resized`
signal (already enumerated in Task 4's diagnostic) fires after; WebKit
re-renders and `buffer-rendered` delivers a new frame at the new size.

### FFI additions (`ffi.rs`)

```rust
#[repr(C)] pub struct WPEToplevel { _opaque: [u8; 0] }

unsafe extern "C" {
    pub fn wpe_view_get_toplevel(view: *mut WPEView) -> *mut WPEToplevel;
    pub fn wpe_toplevel_resize(t: *mut WPEToplevel, width: c_int, height: c_int) -> c_int; // gboolean
    pub fn webkit_web_view_load_uri(view: *mut WebKitWebView, uri: *const c_char);
    // webkit_web_view_load_html: drop the #[allow(dead_code)] (real caller now).
}
```

### Producer struct deltas

```rust
#[cfg(feature = "wpe")]
pub(super) struct WpeHandles {
    pub webview: glib::Object,
    pub view: *mut ffi::WPEView,
    pub toplevel: *mut ffi::WPEToplevel,   // NEW
    pub main_context: glib::MainContext,
}

pub struct WpeProducer {
    // ... existing fields ...
    #[cfg(feature = "wpe")] pub(super) nav_state: std::rc::Rc<std::cell::RefCell<navigation::NavState>>,
}
```

The `wpe` `new` initializes `nav_state` AFTER `build_producer_view`
returns, then calls `navigation::install_load_signals(&webview,
&nav_state)` BEFORE connecting the buffer-rendered seam. Order doesn't
matter for correctness — both connect on the same WebKitWebView /
WPEView — but installing nav signals first makes any later debugging
easier (load events arrive before frames).

### Error variants

`WebSurfaceError` already has `Platform(String)` and `NotReady(&'static
str)` (used by Task 6). For navigation-failed propagation we use
`Platform(format!("navigation failed: {msg}"))` — no new variant. If a
future caller wants to distinguish nav failures structurally, that's a
separate refactor across all backends.

### Testing

Per the 4c.2 retrospective's one-WPE-per-process constraint, the unit
test module still has exactly **one** `#[ignore]`d runtime test. Replace
`renders_one_dmabuf_frame` with a strict superset
`navigate_resize_and_render`:

1. Construct producer.
2. `navigate_to_string("<body style=margin:0;background:#1e90ff></body>", 5s)` — asserts Ok.
3. After return, call `acquire_frame` → assert non-zero `DmaBufImage` (post-navigation frame).
4. Close fds of that frame.
5. Re-arm and call `resize(PhysicalSize::new(512, 512))` → assert Ok.
6. Re-trigger paint by `navigate_to_string` again (same HTML) → wait → acquire.
7. Assert the new `DmaBufImage`'s size reflects 512×512 (or whatever the runtime gives — if the headless toplevel coerces, log it and assert non-zero + plane count instead, mirroring the smoke's permissive shape).
8. Close fds; producer drops cleanly.

Plus pure-Rust unit tests (no display) for:
- `NavState` transitions on `load-changed` events (call the closure manually with each `WebKitLoadEvent` variant).
- `arm_navigation` clears the right fields.
- `poll_navigation_event` drains FIFO order.

### Empirical unknowns (Task-2-style spike points)

These get specific verification steps in the implementation plan rather
than being hardcoded:

1. **Toplevel non-null on headless display.** Almost certainly yes
   (Task 6's smoke rendered at the headless default 1024×768, which
   implies a toplevel). Guard at construction will tell us
   immediately.
2. **`wpe_toplevel_resize` actually drives a new buffer.** Returns
   gboolean — true means accepted. Whether WebKit then re-renders at
   that size depends on whether `webkit_web_view` listens for the
   view's `resized` signal. Verify by asserting the post-resize frame
   has the new dimensions in step 7 above; if not, may also need
   `wpe_view_resized(view, w, h)` after the toplevel call. Iterate.
3. **`load-changed` signal arrives over glib.** Confirmed structurally
   (Task 4 enumerated WPEView signals — but `load-changed` is on the
   `WebKitWebView`, not the view, so a separate connect target).
   `connect_closure` panics on missing signal, so first run is the
   oracle.

## Module / file changes

- **New:** `scrying/src/wpe_producer/navigation.rs`.
- **Modify:** `scrying/src/wpe_producer/ffi.rs` — 3 new FFI decls + 1
  opaque struct.
- **Modify:** `scrying/src/wpe_producer/mod.rs` — declare `mod
  navigation;` under `#[cfg(feature = "wpe")]`.
- **Modify:** `scrying/src/wpe_producer/producer.rs` — `WpeHandles`
  gains `toplevel`; `WpeProducer` gains `nav_state`; trait methods
  promoted from stubs; one inherent block added.
- **Modify:** `scrying/src/wpe_producer/headless.rs` — `build_producer_view`
  returns `(webview, view, toplevel)`; replaces the smoke test with the
  superset `navigate_resize_and_render`.

## Deferred / explicitly not done here

- Input forwarding (WPEEvent).
- Cookies (`WebKitNetworkSession` cookie manager API), scheme handlers,
  popups, downloads, cursor, IME.
- Title / favicon / additional `NavigationEvent` variants.

## Phase checklist deltas (after this lands)

- [x] **4c.3** Producer navigation (navigate_to_url / navigate_to_string
      / poll_navigation_event) + resize via WPEToplevel.
- [ ] **4c.4** (renumbered) Input forwarding via WPEEvent.
- [ ] **4c.5** (renumbered) Phase 2b–2e surface port (cookies, schemes,
      popups, downloads, cursor, IME state).
- [ ] **4c.6** (renumbered) `demo-wpe` runtime probe.
- [ ] **4c.7** (renumbered) `docs/wpe-deployment.md`.
- [ ] **4c.8** (renumbered) Parity matrix + README updates.

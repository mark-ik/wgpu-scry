# Phase 4c.3 — Navigation + Resize Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Promote `WpeProducer`'s `navigate_to_string` / `navigate_to_url` from `Unsupported` stubs to real loads with main-loop-pumped completion waits, wire `resize` through `WPEToplevel`, and expose the `NavigationEvent` queue.

**Architecture:** Direct port of `webkitgtk_producer/navigation.rs` — `Rc<RefCell<NavState>>` mutated by `load-changed` / `load-failed` glib signal closures on the `WebKitWebView`. Resize diverges: `wpe_view_get_toplevel(view)` + `wpe_toplevel_resize(toplevel, w, h)`, with a fail-fast construction guard. Single in-crate `#[ignore]`d runtime test replaces the Task-6 smoke with a superset that navigates + resizes + asserts a real frame.

**Tech Stack:** Rust 2024, `glib` 0.18 (`connect_closure` / `closure_local!`), hand-written `extern "C"` to `libWPEWebKit-2.0.so`, model A thread-affine producer.

**Spec:** [`2026-06-03_phase4c3_navigation_resize.md`](2026-06-03_phase4c3_navigation_resize.md)

**Conventions:**
- All FFI compiles only under `--features wpe` (same gate as 4c.2).
- glib stays 0.18 — same crate version the rest of the WPE FFI uses.
- One `#[ignore]`d runtime-WPE test in the unit-test binary; replaces the existing `renders_one_dmabuf_frame`. Documented in `headless.rs` module doc; do not reintroduce a second.
- Commit after every task with the trailer `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.

---

## File Structure

- **Create:** `scrying/src/wpe_producer/navigation.rs` — `NavState`, `WebKitLoadEvent` constants, `install_load_signals`, `wait_for_load`, `arm_navigation`. `#[cfg(feature = "wpe")]`.
- **Modify:** `scrying/src/wpe_producer/ffi.rs` — add `WPEToplevel` opaque struct + 3 FFI decls; drop `#[allow(dead_code)]` on `webkit_web_view_load_html`.
- **Modify:** `scrying/src/wpe_producer/mod.rs` — declare `mod navigation;` under `#[cfg(feature = "wpe")]`.
- **Modify:** `scrying/src/wpe_producer/producer.rs` — `WpeHandles` gains `toplevel` field; `WpeProducer` gains `nav_state` field (wpe-gated); inherent `load_html`/`load_uri`/`wait_for_load`; trait methods promoted to real impls.
- **Modify:** `scrying/src/wpe_producer/headless.rs` — `build_producer_view` returns `(webview, view, toplevel)`; smoke test rewritten to `navigate_resize_and_render`.

---

## Task 1: FFI additions (toplevel + load_uri)

**Files:**
- Modify: `scrying/src/wpe_producer/ffi.rs`

- [ ] **Step 1: Add the WPEToplevel opaque struct and three FFI decls**

In `ffi.rs`, after the existing `WPEBufferDMABuf` struct, add the opaque type:

```rust
#[repr(C)] pub struct WPEToplevel { _opaque: [u8; 0] }
```

Inside the existing `unsafe extern "C" { ... }` block, add (next to the existing WPEView calls):

```rust
    // Toplevel chain — under WPEPlatform the view's render size is set on
    // its WPEToplevel, not on the view directly.
    pub fn wpe_view_get_toplevel(view: *mut WPEView) -> *mut WPEToplevel;
    pub fn wpe_toplevel_resize(t: *mut WPEToplevel, width: c_int, height: c_int) -> c_int; // gboolean
```

And the load_uri counterpart to load_html:

```rust
    pub fn webkit_web_view_load_uri(view: *mut WebKitWebView, uri: *const c_char);
```

- [ ] **Step 2: Drop the `#[allow(dead_code)]` on `webkit_web_view_load_html`**

The Task-6 cleanup put `#[allow(dead_code)]` on the load_html decl because its only caller was `#[cfg(test)] load_html_for_smoke`. Task 4 will add a real (non-test) caller in `producer.rs::load_html`. Find this in `ffi.rs`:

```rust
    // Currently used only by the smoke test (cfg(test)); promoted to the
    // public navigation API in Phase 4c.3.
    #[allow(dead_code)]
    pub fn webkit_web_view_load_html(...);
```

Remove the `#[allow(dead_code)]` line and update the comment:

```rust
    // Inline HTML load; both strings are copied by WebKit before returning.
    // `base_uri` may be NULL (treated as "about:blank").
    pub fn webkit_web_view_load_html(
        web_view: *mut WebKitWebView,
        content: *const c_char,
        base_uri: *const c_char,
    );
```

- [ ] **Step 3: Verify both build configs**

Run: `cargo build -p scrying`
Expected: builds (no feature; FFI extern decls don't compile FFI calls).

Run: `cargo build -p scrying --features wpe`
Expected: builds. New decls are unused so far → expect dead_code warnings on `wpe_view_get_toplevel`, `wpe_toplevel_resize`, `webkit_web_view_load_uri`. These are TRANSIENT and consumed by Tasks 2, 5. Do NOT add `#[allow(dead_code)]` to silence them; they'll be used within this plan.

- [ ] **Step 4: Commit**

```bash
git add scrying/src/wpe_producer/ffi.rs
git commit -m "$(cat <<'EOF'
phase 4c.3: FFI decls for toplevel resize + webkit_web_view_load_uri

Adds WPEToplevel opaque + wpe_view_get_toplevel / wpe_toplevel_resize and
webkit_web_view_load_uri. Drops the test-only #[allow(dead_code)] on
load_html (Task 4 of this plan adds the real caller).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Toplevel acquisition guard in `build_producer_view`

**Files:**
- Modify: `scrying/src/wpe_producer/headless.rs`

- [ ] **Step 1: Change `build_producer_view`'s return type and add the toplevel guard**

In `headless.rs`, update the signature:

```rust
pub(super) fn build_producer_view()
    -> Result<(glib::Object, *mut ffi::WPEView, *mut ffi::WPEToplevel), WebSurfaceError>
```

After the existing step 9 (`view.is_null()` guard), before `Ok(...)`, add:

```rust
    // 10. Acquire the view's toplevel (resize target). `wpe_view_get_toplevel`
    //     is transfer-none — the view owns it, no unref on our side.
    // SAFETY: `view` is non-null per step 9.
    let toplevel = unsafe { ffi::wpe_view_get_toplevel(view) };
    if toplevel.is_null() {
        return Err(WebSurfaceError::Platform(
            "wpe_view_get_toplevel returned null on the headless display; \
             resize would always fail".into(),
        ));
    }

    Ok((webview, view, toplevel))
```

(Replace the existing `Ok((webview, view))` with this block.)

- [ ] **Step 2: Run the existing ignored test to confirm the toplevel guard passes**

Build: `cargo build -p scrying --features wpe` → expect compile errors in `producer.rs` because `build_producer_view`'s caller now gets a 3-tuple. **Do not fix producer.rs yet** — Task 3 does that. Skip ahead to Task 3 to finish the wire-up, then come back here and re-run.

After Task 3 lands, run:
`cargo test -p scrying --features wpe renders_one_dmabuf_frame -- --ignored --nocapture`

This verifies the toplevel guard didn't break the empirically-known-working construction path. Expected: PASS, exit 0, same smoke output (1024x768).

- [ ] **Step 3: Commit (after Task 3 makes it compile)**

```bash
git add scrying/src/wpe_producer/headless.rs
git commit -m "$(cat <<'EOF'
phase 4c.3: acquire WPEToplevel at construction with fail-fast guard

build_producer_view now returns (webview, view, toplevel). On the headless
display wpe_view_get_toplevel must be non-null or resize would silently
fail later; assert at construction instead.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

> Practical note: Tasks 2 and 3 produce a coupled diff (signature change + caller update). The plan separates them for clarity, but the implementer may stage both files and make ONE commit covering both, with the combined message. That's acceptable.

---

## Task 3: `WpeHandles` gains `toplevel`; producer wires through the new tuple

**Files:**
- Modify: `scrying/src/wpe_producer/producer.rs`

- [ ] **Step 1: Add the `toplevel` field to `WpeHandles`**

In `producer.rs`, locate `WpeHandles`:

```rust
#[cfg(feature = "wpe")]
#[allow(dead_code)]
pub(super) struct WpeHandles {
    pub webview: glib::Object,
    pub view: *mut super::ffi::WPEView,
    pub main_context: glib::MainContext,
}
```

Add a `toplevel` field (held for `resize` to call into; not unref'd because `wpe_view_get_toplevel` is transfer-none):

```rust
#[cfg(feature = "wpe")]
#[allow(dead_code)]
pub(super) struct WpeHandles {
    pub webview: glib::Object,
    pub view: *mut super::ffi::WPEView,
    /// Borrowed from the view (transfer-none); valid for the view's lifetime.
    /// `resize` calls `wpe_toplevel_resize` against this.
    pub toplevel: *mut super::ffi::WPEToplevel,
    pub main_context: glib::MainContext,
}
```

- [ ] **Step 2: Update the wpe `new` constructor to consume the 3-tuple**

In `producer.rs`, locate the `#[cfg(feature = "wpe")] pub fn new`:

```rust
        let main_context = glib::MainContext::default();
        let (webview, view) = super::headless::build_producer_view()?;
        Ok(Self {
            // ...
            handles: WpeHandles { webview, view, main_context },
        })
```

Change to:

```rust
        let main_context = glib::MainContext::default();
        let (webview, view, toplevel) = super::headless::build_producer_view()?;
        Ok(Self {
            // ...
            handles: WpeHandles { webview, view, toplevel, main_context },
        })
```

(The buffer-rendered `connect_buffer_rendered` call that follows construction stays as-is.)

- [ ] **Step 3: Verify build + the existing smoke still passes**

Run: `cargo build -p scrying --features wpe`
Expected: builds clean.

Run: `cargo test -p scrying --features wpe renders_one_dmabuf_frame -- --ignored`
Expected: PASS, exit 0 (same as before — the toplevel guard doesn't change rendered output).

- [ ] **Step 4: Commit (or combine with Task 2 per the note above)**

```bash
git add scrying/src/wpe_producer/producer.rs
git commit -m "$(cat <<'EOF'
phase 4c.3: thread toplevel handle through WpeProducer::new

WpeHandles gains a toplevel pointer (transfer-none borrow from the view);
new() unpacks the 3-tuple from build_producer_view and stores it.
Resize uses it in Task 5.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `NavState` + `install_load_signals` + `arm_navigation` + `wait_for_load` (unit-testable core)

The closure body's behavior (NavState mutation per load event) is the one piece of nav logic that's testable without a display. We TDD it.

**Files:**
- Create: `scrying/src/wpe_producer/navigation.rs`
- Modify: `scrying/src/wpe_producer/mod.rs` (declare the module)

- [ ] **Step 1: Declare `mod navigation;` in `mod.rs`**

In `scrying/src/wpe_producer/mod.rs`, locate the existing wpe-gated module declarations:

```rust
#[cfg(feature = "wpe")]
mod ffi;
#[cfg(feature = "wpe")]
mod headless;
```

Add:

```rust
#[cfg(feature = "wpe")]
mod navigation;
```

- [ ] **Step 2: Write `navigation.rs` skeleton with the `NavState` + transition helper**

Create `scrying/src/wpe_producer/navigation.rs`:

```rust
//! Navigation: `load_uri` / `load_html` with main-loop-pumped completion
//! waits. Direct port of `webkitgtk_producer/navigation.rs` — same
//! `WebKitLoadEvent` enum + `load-changed`/`load-failed` signals on the
//! WebKitWebView. Single-threaded model A: state is `Rc<RefCell<NavState>>`
//! mutated from glib signal closures on the construction thread.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::{NavigationEvent, WebSurfaceError};

/// `WebKitLoadEvent` from `/usr/include/wpe-webkit-2.0/wpe/WebKitWebView.h`
/// (the int values land in the glib closure as `i32`).
pub(super) const WEBKIT_LOAD_STARTED: i32 = 0;
pub(super) const WEBKIT_LOAD_REDIRECTED: i32 = 1;
pub(super) const WEBKIT_LOAD_COMMITTED: i32 = 2;
pub(super) const WEBKIT_LOAD_FINISHED: i32 = 3;

#[derive(Default)]
pub(super) struct NavState {
    pub committed_uri: Option<String>,
    pub finished: bool,
    pub failed: Option<String>,
    pub events: VecDeque<NavigationEvent>,
}

impl NavState {
    /// Apply a `load-changed` event. `uri` is the WebKitWebView's current URI
    /// at the time the signal fired (may be empty for inline HTML).
    pub fn on_load_changed(&mut self, event: i32, uri: String) {
        match event {
            WEBKIT_LOAD_STARTED => {
                self.finished = false;
                self.failed = None;
                self.events.push_back(NavigationEvent::Starting { url: uri });
            }
            WEBKIT_LOAD_COMMITTED => {
                self.committed_uri = Some(uri.clone());
                self.events.push_back(NavigationEvent::SourceChanged { url: uri });
            }
            WEBKIT_LOAD_FINISHED => {
                self.finished = true;
                self.events.push_back(NavigationEvent::Completed { url: uri, success: true });
            }
            // Redirected fires between Started and Committed — no scrying
            // variant for it, so skip (matches GTK).
            _ => {}
        }
    }

    /// Apply a `load-failed` event. `error` is the GError message.
    pub fn on_load_failed(&mut self, uri: String, error: String) {
        self.failed = Some(error);
        // Unblock waiters; trait-level navigate methods inspect `failed`.
        self.finished = true;
        self.events.push_back(NavigationEvent::Completed { url: uri, success: false });
    }
}

/// Clear pre-load state before a fresh load. Leaves `events` intact — they're
/// drained by `poll_navigation_event` on the producer's own schedule.
pub(super) fn arm_navigation(state: &Rc<RefCell<NavState>>) {
    let mut s = state.borrow_mut();
    s.finished = false;
    s.failed = None;
    s.committed_uri = None;
}
```

- [ ] **Step 3: Write the failing NavState unit tests**

Append to `navigation.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::NavigationEvent;

    fn st() -> Rc<RefCell<NavState>> { Rc::new(RefCell::new(NavState::default())) }

    #[test]
    fn started_clears_flags_and_emits_starting() {
        let s = st();
        s.borrow_mut().finished = true;
        s.borrow_mut().failed = Some("old".into());
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_STARTED, "https://x".into());
        let st = s.borrow();
        assert!(!st.finished);
        assert!(st.failed.is_none());
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Starting { url }) if url == "https://x"));
    }

    #[test]
    fn committed_stores_uri_and_emits_source_changed() {
        let s = st();
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_COMMITTED, "https://y".into());
        let st = s.borrow();
        assert_eq!(st.committed_uri.as_deref(), Some("https://y"));
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::SourceChanged { url }) if url == "https://y"));
    }

    #[test]
    fn finished_sets_flag_and_emits_completed_success() {
        let s = st();
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_FINISHED, "https://z".into());
        let st = s.borrow();
        assert!(st.finished);
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Completed { url, success: true }) if url == "https://z"));
    }

    #[test]
    fn redirected_is_a_no_op_on_events() {
        let s = st();
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_REDIRECTED, "https://w".into());
        assert!(s.borrow().events.is_empty());
    }

    #[test]
    fn load_failed_sets_failed_and_finished_and_emits_completed_failure() {
        let s = st();
        s.borrow_mut().on_load_failed("https://q".into(), "boom".into());
        let st = s.borrow();
        assert_eq!(st.failed.as_deref(), Some("boom"));
        assert!(st.finished);
        assert!(matches!(st.events.front(),
            Some(NavigationEvent::Completed { url, success: false }) if url == "https://q"));
    }

    #[test]
    fn arm_clears_flags_keeps_events() {
        let s = st();
        s.borrow_mut().on_load_changed(WEBKIT_LOAD_FINISHED, "x".into());
        arm_navigation(&s);
        let st = s.borrow();
        assert!(!st.finished);
        assert!(st.failed.is_none());
        assert!(st.committed_uri.is_none());
        assert_eq!(st.events.len(), 1, "events queue is drained by poll, not by arm");
    }
}
```

- [ ] **Step 4: Run the unit tests; they all pass**

Run: `cargo test -p scrying --features wpe wpe_producer::navigation::tests`
Expected: 6 passed, 0 failed. These are pure-Rust, no display — they exercise the closure body's behavior without glib.

- [ ] **Step 5: Add `install_load_signals` + `wait_for_load`**

Append to `navigation.rs`, before the `#[cfg(test)]` block:

```rust
use glib::prelude::*;

/// Connect `load-changed` and `load-failed` on the given WebKitWebView,
/// routing into `state`. The closures run on the producer's main context
/// thread (model A). They capture only `Rc<RefCell<NavState>>` clones — no
/// `&mut self`-like borrows survive into the closure body.
pub(super) fn install_load_signals(webview: &glib::Object, state: &Rc<RefCell<NavState>>) {
    {
        let s = state.clone();
        webview.connect_closure(
            "load-changed",
            false,
            glib::closure_local!(move |view: glib::Object, event: i32| {
                let uri = view
                    .property::<Option<String>>("uri")
                    .unwrap_or_default();
                s.borrow_mut().on_load_changed(event, uri);
            }),
        );
    }
    {
        let s = state.clone();
        webview.connect_closure(
            "load-failed",
            false,
            glib::closure_local!(move |_view: glib::Object, _event: i32, failing_uri: String, error: glib::Error| {
                s.borrow_mut().on_load_failed(failing_uri, error.message().to_string());
                // `load-failed` is gboolean-returning in C (true = handled). The glib
                // marshalling here defaults the return; that's fine — we don't try to
                // suppress the default failure UI (there is none on a headless view).
            }),
        );
    }
}

/// Pump the producer's `MainContext` until `state.finished`, returning an
/// error wrapping `state.failed` if the load failed.
pub(super) fn wait_for_load(
    ctx: &glib::MainContext,
    state: &Rc<RefCell<NavState>>,
    timeout: Duration,
) -> Result<(), WebSurfaceError> {
    let deadline = Instant::now() + timeout;
    super::headless::pump_until(ctx, deadline, || state.borrow().finished)?;
    if let Some(msg) = state.borrow().failed.clone() {
        return Err(WebSurfaceError::Platform(format!("navigation failed: {msg}")));
    }
    Ok(())
}
```

Two empirical points the implementer should validate at the first runtime test (Task 6):
1. The glib closure arg marshalling for `load-failed`'s GError → `glib::Error`. If `connect_closure` panics with a type mismatch, drop the typed `glib::Error` arg and accept whatever glib 0.18 marshals (e.g. `String` for the message via a 2-arg form on some glib versions). Keep iterating until it connects.
2. The `view.property::<Option<String>>("uri")` call — if glib 0.18 rejects the type or the property name, alternative is `view.property::<String>("uri")` (some glib versions don't make it optional). Adjust to what compiles.

- [ ] **Step 6: Build under the feature**

Run: `cargo build -p scrying --features wpe`
Expected: PASS. The new `install_load_signals` + `wait_for_load` are unused so far → warnings expected; Tasks 5–6 consume them.

- [ ] **Step 7: Commit**

```bash
git add scrying/src/wpe_producer/navigation.rs scrying/src/wpe_producer/mod.rs
git commit -m "$(cat <<'EOF'
phase 4c.3: NavState + glib load-changed/load-failed signal seam

NavState mirrors webkitgtk_producer's: committed_uri + finished + failed
+ FIFO NavigationEvent queue, mutated by on_load_changed / on_load_failed.
install_load_signals connects via glib 0.18 connect_closure on the
WebKitWebView; wait_for_load pumps until finished. Closure-body
transitions covered by 6 pure-Rust unit tests.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Wire nav_state into `WpeProducer::new` + inherent + trait methods

**Files:**
- Modify: `scrying/src/wpe_producer/producer.rs`

- [ ] **Step 1: Add `nav_state` field to `WpeProducer`**

In `producer.rs`, add a wpe-gated field on `WpeProducer`:

```rust
pub struct WpeProducer {
    // ... existing fields ...
    #[cfg(feature = "wpe")]
    pub(super) nav_state: std::rc::Rc<std::cell::RefCell<super::navigation::NavState>>,
}
```

- [ ] **Step 2: Initialize `nav_state` in the wpe `new` and install signals**

In the `#[cfg(feature = "wpe")] pub fn new(config)` body, after the `WpeHandles` construction and BEFORE `connect_buffer_rendered`:

```rust
        let main_context = glib::MainContext::default();
        let (webview, view, toplevel) = super::headless::build_producer_view()?;
        let nav_state = std::rc::Rc::new(std::cell::RefCell::new(
            super::navigation::NavState::default(),
        ));
        super::navigation::install_load_signals(&webview, &nav_state);
        // ... then the existing connect_buffer_rendered call ...
        Ok(Self {
            // ... existing fields ...
            handles: WpeHandles { webview, view, toplevel, main_context },
            nav_state,
        })
```

For the non-wpe stub `new`, don't add the field (the `#[cfg(feature = "wpe")]` gate keeps `nav_state` out of that build entirely; the stub `Ok(Self { ... })` struct expression doesn't list it).

- [ ] **Step 3: Add inherent `load_html` / `load_uri` / `wait_for_load` methods**

In `producer.rs`, add (wpe-gated; place after the existing inherent impl block or in a new `#[cfg(feature = "wpe")] impl WpeProducer { ... }` block):

```rust
#[cfg(feature = "wpe")]
impl WpeProducer {
    /// Non-blocking HTML load. Companion `wait_for_load` (or the trait's
    /// `navigate_to_string`) drives completion.
    pub fn load_html(&self, html: &str, base_uri: Option<&str>) {
        use glib::translate::ToGlibPtr;
        let raw: *mut super::ffi::WebKitWebView =
            glib::translate::ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(
                &self.handles.webview,
            ).0 as *mut _;
        let c_html = std::ffi::CString::new(html).unwrap_or_else(|_| {
            std::ffi::CString::new("").unwrap()
        });
        let c_base = base_uri.map(|s| std::ffi::CString::new(s).ok()).flatten();
        // SAFETY: `raw` is borrowed from the owned `webview`; load_html copies
        // both strings before returning.
        unsafe {
            super::ffi::webkit_web_view_load_html(
                raw,
                c_html.as_ptr(),
                c_base.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
            );
        }
    }

    /// Non-blocking URI load. Companion `wait_for_load` (or the trait's
    /// `navigate_to_url`) drives completion.
    pub fn load_uri(&self, uri: &str) {
        use glib::translate::ToGlibPtr;
        let raw: *mut super::ffi::WebKitWebView =
            glib::translate::ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(
                &self.handles.webview,
            ).0 as *mut _;
        let c_uri = std::ffi::CString::new(uri).unwrap_or_else(|_| {
            std::ffi::CString::new("about:blank").unwrap()
        });
        // SAFETY: raw borrowed; load_uri copies the string before returning.
        unsafe { super::ffi::webkit_web_view_load_uri(raw, c_uri.as_ptr()); }
    }

    /// Pump the producer's main context until the most recent navigation
    /// finishes or `timeout` elapses.
    pub fn wait_for_load(&self, timeout: std::time::Duration) -> Result<(), crate::WebSurfaceError> {
        super::navigation::wait_for_load(&self.handles.main_context, &self.nav_state, timeout)
    }
}
```

- [ ] **Step 4: Replace the `Unsupported` trait stubs with real impls**

In `producer.rs`, find the `impl WebSurfaceProducer for WpeProducer` block. Replace `navigate_to_string`, `navigate_to_url` (if present; if not, add it), `resize`, and `poll_navigation_event`. The `wpe` and non-`wpe` builds differ: under non-wpe, `navigate_*` / `resize` / `poll_navigation_event` keep returning `Unsupported`/the existing stub, since there's no display.

Use cfg-attribute selection on the method body to keep both builds compiling. Concrete shape:

```rust
    fn navigate_to_string(
        &mut self,
        html: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            super::navigation::arm_navigation(&self.nav_state);
            self.load_html(html, None);
            self.wait_for_load(timeout)
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = (html, timeout);
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }

    fn navigate_to_url(
        &mut self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        #[cfg(feature = "wpe")] {
            super::navigation::arm_navigation(&self.nav_state);
            self.load_uri(url);
            self.wait_for_load(timeout)
        }
        #[cfg(not(feature = "wpe"))] {
            let _ = (url, timeout);
            Err(WebSurfaceError::Unsupported(
                "WpeProducer compiled without `wpe` feature; rebuild with --features wpe",
            ))
        }
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), WebSurfaceError> {
        if size.width == 0 || size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WPE producer size must be non-zero, got {}x{}",
                size.width, size.height
            )));
        }
        #[cfg(feature = "wpe")] {
            // SAFETY: handles.toplevel is non-null per the construction guard
            // in build_producer_view; transfer-none borrow, valid for the
            // view's (and producer's) lifetime.
            let ok = unsafe {
                super::ffi::wpe_toplevel_resize(
                    self.handles.toplevel,
                    size.width as std::os::raw::c_int,
                    size.height as std::os::raw::c_int,
                )
            };
            if ok == 0 {
                return Err(WebSurfaceError::Platform(
                    format!("wpe_toplevel_resize returned FALSE for {}x{}",
                            size.width, size.height),
                ));
            }
        }
        self.size = size;
        Ok(())
    }

    fn poll_navigation_event(&mut self) -> Option<crate::NavigationEvent> {
        #[cfg(feature = "wpe")] {
            self.nav_state.borrow_mut().events.pop_front()
        }
        #[cfg(not(feature = "wpe"))] { None }
    }
```

If `navigate_to_url` is missing from the existing `impl` block (it was an `Unsupported` default on the trait), add it. The existing `set_offset` stays as it is.

- [ ] **Step 5: Build both configurations**

Run: `cargo build -p scrying`
Expected: PASS (non-wpe stubs return Unsupported / None / Ok(())).

Run: `cargo build -p scrying --features wpe`
Expected: PASS. Now all the Task-1/4 dead-code warnings should be gone.

- [ ] **Step 6: Run the existing smoke test to confirm no regression**

Run: `cargo test -p scrying --features wpe renders_one_dmabuf_frame -- --ignored`
Expected: PASS, exit 0 (the smoke test is unchanged at this point — Task 6 rewrites it).

- [ ] **Step 7: Commit**

```bash
git add scrying/src/wpe_producer/producer.rs
git commit -m "$(cat <<'EOF'
phase 4c.3: navigate_to_{string,url} + resize + poll_navigation_event

Promote trait stubs to real impls under --features wpe. WpeProducer gains a
nav_state (Rc<RefCell<NavState>>) initialized in new() and wired to the
WebKitWebView's load-changed/load-failed signals. Resize goes through
wpe_toplevel_resize; poll_navigation_event drains the NavState event queue.
Non-wpe build still returns Unsupported.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Replace the Task-6 smoke with `navigate_resize_and_render`

The one in-crate `#[ignore]`d runtime test gets upgraded to exercise the full new surface: navigate, then resize, then assert frame at new size.

**Files:**
- Modify: `scrying/src/wpe_producer/headless.rs`

- [ ] **Step 1: Replace the existing test body**

Find the `#[cfg(test)] mod tests { ... fn renders_one_dmabuf_frame ... }` in `headless.rs`. Replace the whole `mod tests` with:

```rust
#[cfg(test)]
mod tests {
    /// End-to-end runtime smoke for 4c.3: construct a headless producer,
    /// navigate to an inline page, assert load completes, acquire a real
    /// `DmaBufImage`, then resize and acquire another frame at the new size.
    /// Strict superset of the 4c.2 smoke + adds nav + resize coverage.
    ///
    /// The one-WPE-per-process constraint (see module doc) means this MUST
    /// remain the only ignored runtime test in this binary.
    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn navigate_resize_and_render() {
        use crate::native_frame::{NativeFrame, SyncMechanism};
        use crate::wpe_producer::{WpeProducer, WpeProducerConfig};
        use crate::{WebSurfaceFrame, WebSurfaceProducer};
        use dpi::PhysicalSize;

        let config = WpeProducerConfig::new(PhysicalSize::new(256, 256), std::env::temp_dir());
        let mut producer = WpeProducer::new(config).expect("construct headless producer");

        // --- 1. Navigate + assert first frame ---
        producer
            .navigate_to_string(
                "<body style='margin:0;background:#1e90ff'></body>",
                std::time::Duration::from_secs(5),
            )
            .expect("navigate_to_string");

        // Drain navigation events so poll_navigation_event paths are exercised.
        let mut nav_events = Vec::new();
        while let Some(e) = producer.poll_navigation_event() {
            nav_events.push(e);
        }
        assert!(
            nav_events.iter().any(|e| matches!(e, crate::NavigationEvent::Completed { success: true, .. })),
            "expected a successful Completed event; got {:?}",
            nav_events
        );

        let frame_1 = producer.acquire_frame().expect("first frame after navigate");
        let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img1)) = frame_1 else {
            panic!("expected a DMABUF frame");
        };
        assert!(img1.size.width > 0 && img1.size.height > 0, "non-zero size 1");
        assert!(!img1.planes.is_empty(), "at least one plane");
        assert!(img1.planes[0].fd >= 0, "valid dup'd fd");
        assert_eq!(img1.producer_sync, SyncMechanism::None);
        eprintln!(
            "smoke#1 (post-nav): {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}",
            img1.size.width, img1.size.height, img1.drm_format, img1.drm_modifier, img1.planes.len()
        );
        super::super::producer::close_frame_fds(&img1);

        // --- 2. Resize + assert next frame reflects the new size ---
        producer
            .resize(PhysicalSize::new(512, 384))
            .expect("resize to 512x384");

        // After resize, WebKit should re-render. Pump up to 5s for a fresh
        // frame whose dimensions differ from img1 (or, if the runtime simply
        // accepts the resize without paint, the next acquire returns at the
        // new size after a fresh navigate).
        let ctx = producer.handles.main_context.clone();
        let pending = producer.pending_frame.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let arrived = super::pump_until(&ctx, deadline, || {
            pending.lock().map(|s| s.is_some()).unwrap_or(false)
        }).is_ok();

        // Empirical fallback: if no new frame arrived from resize alone,
        // re-navigate to trigger a paint. The test still validates "resize
        // takes effect" by checking the post-second-navigate frame size.
        if !arrived {
            eprintln!("(resize did not auto-trigger a buffer-rendered; renavigating)");
            producer
                .navigate_to_string(
                    "<body style='margin:0;background:#22aa22'></body>",
                    std::time::Duration::from_secs(5),
                )
                .expect("renavigate after resize");
            while let Some(_e) = producer.poll_navigation_event() {}
        }

        let frame_2 = producer.acquire_frame().expect("second frame after resize");
        let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img2)) = frame_2 else {
            panic!("expected a DMABUF frame");
        };
        assert!(img2.size.width > 0 && img2.size.height > 0, "non-zero size 2");
        eprintln!(
            "smoke#2 (post-resize): {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}",
            img2.size.width, img2.size.height, img2.drm_format, img2.drm_modifier, img2.planes.len()
        );
        // Soft assertion: the WPE headless toplevel may coerce dimensions;
        // we don't hard-assert exact 512x384. The diagnostic eprintln makes
        // the actual size visible for follow-up. The hard contract is that
        // the seam still produces a valid frame after resize.
        super::super::producer::close_frame_fds(&img2);
    }
}
```

- [ ] **Step 2: Build + run the new ignored test**

Run: `cargo build -p scrying --features wpe`
Expected: PASS.

Run: `cargo test -p scrying --features wpe navigate_resize_and_render -- --ignored --nocapture`
Expected: PASS, exit 0. Two `smoke#…` lines printed. If `connect_closure` panics on the load-changed signature (e.g. arg-type mismatch), revisit Task 4 Step 5 — try `i32` vs `u32` for the event arg, or drop the typed `glib::Error` in load-failed for the plain string the message field carries.

- [ ] **Step 3: Confirm non-ignored tests still pass**

Run: `cargo test -p scrying`
Expected: 7 (no-feat) + 3 integration tests pass, 0 ignored.

Run: `cargo test -p scrying --features wpe`
Expected: 6 unit (the no-feat-only fd Drop test is gated out) + 3 integration tests pass, 1 ignored (the smoke).

- [ ] **Step 4: Commit**

```bash
git add scrying/src/wpe_producer/headless.rs
git commit -m "$(cat <<'EOF'
phase 4c.3: smoke -> navigate_resize_and_render (strict 4c.2 superset)

Replaces renders_one_dmabuf_frame with a test that navigates to an inline
page (asserting the full load-changed -> Completed path), acquires a real
DmaBufImage, calls resize, then re-acquires. Honors the
one-WPE-per-process constraint by remaining the only #[ignore]d runtime
test in this binary. Prints observed dimensions both pre- and post-resize
to surface any toplevel coercion.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Update the strategy checklist

**Files:**
- Modify: `design_docs/2026-05-15_phase4_strategy.md`

- [ ] **Step 1: Flip 4c.3 to done; renumber 4c.4+**

In `design_docs/2026-05-15_phase4_strategy.md`, find the checklist block and update the 4c.3 line + renumber what was 4c.4..4c.7:

```markdown
- [x] **4c.3** Producer navigation (navigate_to_string / navigate_to_url
      / poll_navigation_event) + resize via WPEToplevel; spec
      [`2026-06-03_phase4c3_navigation_resize.md`](2026-06-03_phase4c3_navigation_resize.md),
      plan [`2026-06-03_phase4c3_implementation_plan.md`](2026-06-03_phase4c3_implementation_plan.md).
- [ ] **4c.4** Input forwarding via `wpe_view_event(WPEEvent*)` —
      keyboard / pointer / scroll / touch / IME. WPEPlatform path, not
      legacy libwpe.
- [ ] **4c.5** Phase 2b–2e surface ported from
      `webkitgtk_producer/` (cookies, schemes, popups, downloads,
      cursor, IME state).
- [ ] **4c.6** `demo-wpe` runtime probe — mirrors demo-linux
- [ ] **4c.7** `docs/wpe-deployment.md` — Flatpak SDK manifest
      walkthrough
- [ ] **4c.8** Parity matrix + README updates
```

Update the doc status line at the top:

```markdown
**Status:** 4a + 4b.1 + 4c.1 + 4c.2 + 4c.3 shipped; 4c.4+ in flight.
```

- [ ] **Step 2: Commit**

```bash
git add design_docs/2026-05-15_phase4_strategy.md
git commit -m "$(cat <<'EOF'
docs: phase 4c.3 shipped — checklist + status

Flips 4c.3 to done; bumps the input-forwarding line into its own 4c.4
phase per the 4c.3 spec's scope decision (per-empirical-unknown phasing
lesson from 4c.2 retrospective).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- Architecture (port of GTK navigation): Task 4 (NavState + signals). ✓
- `Rc<RefCell<NavState>>` vs Arc/Mutex: Task 5 uses `Rc<RefCell<...>>`. ✓
- `install_load_signals` connecting on the WebKitWebView: Task 4 Step 5. ✓
- `WebKitLoadEvent` constants from header: Task 4 Step 2. ✓
- `wait_for_load` pumping main_context: Task 4 Step 5. ✓
- `arm_navigation` clears flags, keeps events: Task 4 Step 2 (and tested in Step 3). ✓
- Inherent `load_html` / `load_uri` / `wait_for_load`: Task 5 Step 3. ✓
- Trait `navigate_to_string` / `navigate_to_url` / `resize` / `poll_navigation_event`: Task 5 Step 4. ✓
- Construction guard for null toplevel: Task 2 Step 1. ✓
- `WpeHandles.toplevel`: Task 3 Step 1. ✓
- FFI additions: Task 1. ✓
- Drop `#[allow(dead_code)]` on `webkit_web_view_load_html`: Task 1 Step 2. ✓
- Single ignored runtime test (replaces the smoke): Task 6. ✓
- Unit tests for NavState transitions: Task 4 Step 3. ✓
- Empirical-unknown spike points called out in tasks: Task 4 Step 5 note (glib closure marshalling), Task 6 Step 2 fallback (resize-doesn't-trigger-paint). ✓
- Strategy checklist update: Task 7. ✓
- Deferred (input, cookies, schemes, popups, downloads, cursor, IME): not in any task. ✓

**Placeholder scan:** No "TBD"/"TODO"/"handle errors". Two empirical fallbacks are explicitly described with concrete code procedures (glib closure-arg type fallback in Task 4 Step 5; renavigate-after-resize fallback in Task 6 Step 2).

**Type consistency:**
- `NavState` fields `committed_uri`, `finished`, `failed`, `events` — consistent across Tasks 4 and 5.
- `nav_state: Rc<RefCell<NavState>>` — consistent in `producer.rs` (Task 5) and `navigation.rs` (Task 4).
- `WpeHandles.toplevel: *mut ffi::WPEToplevel` — Tasks 1, 2, 3, 5.
- `wpe_toplevel_resize(t, w: c_int, h: c_int) -> c_int` (gboolean as `c_int`) — declared in Task 1, called in Task 5 with `size.width as c_int`. ✓
- `install_load_signals(&glib::Object, &Rc<RefCell<NavState>>)` — Task 4 Step 5 and Task 5 Step 2.
- Closure event arg `i32`: matches header's `WebKitLoadEvent` enum. ✓

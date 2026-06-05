# Phase 4c.5.a — Script-Message Bridge (WPE)

Port of `webkitgtk_producer/script_message.rs` to the WPE producer. The
foundation for `4c.4.3` IME (text-input focus observability) and
anything else needing JS↔host messaging. Direct port: GTK's
`webkit2gtk::UserContentManager` API is the same on WPE's WebKit 2.52.3,
with one modernization (the `script-message-received` signal emits
`JSCValue*` rather than the older `WebKitJavascriptResult*`).

## Scope

In:
- New `scrying/src/wpe_producer/script_message.rs` mirroring the GTK
  precedent: `SCRY_HANDLER_NAME = "scry"`, the
  `window.chrome.webview` JS shim, `install()` that registers the
  handler + injects the shim + wires the signal, the `escape_for_js`
  helper, and the inherent `wait_for_web_message` on `WpeProducer`.
- FFI additions in `ffi.rs`:
  - Opaque `WebKitUserContentManager` and `WebKitUserScript`.
  - `webkit_web_view_get_user_content_manager`,
    `webkit_user_content_manager_register_script_message_handler`,
    `webkit_user_content_manager_add_script`,
    `webkit_user_script_new`.
  - Injection constants (`WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES`,
    `WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START`).
- `WpeProducer` gains `web_messages: Rc<RefCell<VecDeque<String>>>`
  (wpe-gated).
- Trait method promotions under `--features wpe`:
  - `post_web_message(&str) -> Result<(), _>` — runs
    `window.chrome.webview.__scryDispatch(<escaped>)` on the page.
  - `poll_web_message() -> Option<String>` — drains the front of the
    queue.
- Pure-Rust unit tests for `escape_for_js` (no display).
- Runtime smoke: the existing `wpe_input` integration binary already
  navigates to a page. Add a post-message round-trip step at the end:
  navigate to a page with a JS `window.chrome.webview.postMessage('hi')`,
  call `wait_for_web_message(2s)`, assert it returns `Some("hi")`.

Out (separate phases):
- IME via `scryIme` handler — that's 4c.5.e, blocked on this phase.
- Custom JS bridges beyond the `chrome.webview` shim — 4c.5.f+.
- `evaluate_javascript` callback-style result extraction — `post_web_message`
  in 2.52 can use the newer `webkit_web_view_evaluate_javascript` if it's
  the canonical replacement; if not, `webkit_web_view_run_javascript`
  also works (empirical at FFI-decl time).

## Design

### FFI additions

```rust
#[repr(C)] pub struct WebKitUserContentManager { _opaque: [u8; 0] }
#[repr(C)] pub struct WebKitUserScript { _opaque: [u8; 0] }

pub const WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES: i32 = 0;
pub const WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START: i32 = 0;

unsafe extern "C" {
    pub fn webkit_web_view_get_user_content_manager(
        web_view: *mut WebKitWebView,
    ) -> *mut WebKitUserContentManager;
    pub fn webkit_user_content_manager_register_script_message_handler(
        manager: *mut WebKitUserContentManager,
        name: *const c_char,
        world_name: *const c_char, // NULL for default world
    ) -> c_int; // gboolean
    pub fn webkit_user_content_manager_add_script(
        manager: *mut WebKitUserContentManager,
        script: *mut WebKitUserScript,
    );
    pub fn webkit_user_script_new(
        source: *const c_char,
        injected_frames: i32,
        injection_time: i32,
        allow_list: *const *const c_char, // NULL-terminated; pass null for "all"
        block_list: *const *const c_char, // NULL-terminated; pass null for "none"
    ) -> *mut WebKitUserScript;
    pub fn webkit_web_view_evaluate_javascript(
        web_view: *mut WebKitWebView,
        script: *const c_char,
        length: isize, // -1 for null-terminated
        world_name: *const c_char, // NULL for default
        source_uri: *const c_char, // NULL OK
        cancellable: *mut std::ffi::c_void, // GCancellable*, NULL OK
        callback: *mut std::ffi::c_void, // GAsyncReadyCallback, NULL = fire-and-forget
        user_data: *mut std::ffi::c_void,
    );
}
```

If `webkit_web_view_evaluate_javascript` doesn't link (it's WebKit
2.40+; we're on 2.52.3 so it should), fall back to the older
`webkit_web_view_run_javascript(view, script, NULL, NULL, NULL)`.

### `script_message.rs` shape (mirror of GTK)

Same `SCRY_HANDLER_NAME`, same `CHROME_WEBVIEW_SHIM` text, same
`escape_for_js` helper. The `install()` function:

```rust
pub(super) fn install(
    webview: &glib::Object,
    queue: Rc<RefCell<VecDeque<String>>>,
) {
    use glib::translate::ToGlibPtr;
    let raw_view: *mut ffi::WebKitWebView =
        ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(webview).0 as *mut _;
    let ucm = unsafe { ffi::webkit_web_view_get_user_content_manager(raw_view) };
    // ucm is transfer-none (owned by the webview).

    // Connect FIRST, then register (per the WebKit doc note about race conditions).
    let ucm_obj: glib::Object = unsafe {
        glib::translate::from_glib_none(ucm as *mut glib::gobject_ffi::GObject)
    };
    let q = queue.clone();
    // Signal detail: "script-message-received::scry" so we only get scry messages.
    ucm_obj.connect_closure(
        &format!("script-message-received::{SCRY_HANDLER_NAME}"),
        false,
        glib::closure_local!(move |_ucm: glib::Object, value: glib::Object| {
            // EMPIRICAL: WPE 2.52.3 emits JSCValue* as the signal arg.
            // Extract its string via JSCValue's to_string method.
            // Use jsc_value_to_string(raw) FFI for stability, or
            // glib's JSCValue wrapper if available in the glib 0.18
            // crate.
            // Simpler: call value.property::<String>("string-value")
            // or use the raw jsc API. Iterate at runtime.
            let _ = value;
            // ... extract string and push to q
        }),
    );

    let c_name = std::ffi::CString::new(SCRY_HANDLER_NAME).unwrap();
    let ok = unsafe {
        ffi::webkit_user_content_manager_register_script_message_handler(
            ucm,
            c_name.as_ptr(),
            std::ptr::null(), // default world
        )
    };
    assert!(ok != 0, "scry handler registration must not collide");

    // Inject the chrome.webview shim at document-start.
    let c_shim = std::ffi::CString::new(CHROME_WEBVIEW_SHIM).unwrap();
    let script = unsafe {
        ffi::webkit_user_script_new(
            c_shim.as_ptr(),
            ffi::WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
            ffi::WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    unsafe { ffi::webkit_user_content_manager_add_script(ucm, script); }
    // `add_script` takes its own ref; the producer-owned floating ref
    // we just created can drop. glib's `from_glib_full` on `script`
    // wrapped in a glib::Object would handle this; if we just leak
    // the original `script` pointer here that's a 1-ref leak per
    // producer construction — minor and worth verifying empirically.
}
```

The JSCValue→String extraction is the one genuinely empirical part.
Three approaches in priority order:

1. `value.property::<String>("string-value")` — if JSCValue exposes
   it as a GObject property (some versions do).
2. Add a `jsc_value_to_string(*mut JSCValue) -> *mut c_char` FFI
   decl, call `g_free` on the returned string after copying.
3. Use the glib `JSCValue` Rust wrapper if it's part of the
   `javascriptcore` crate that's already a webkit dep. (Likely no —
   it lives in the gtk-rs ecosystem.)

The implementer can resolve this at runtime — the unit smoke is the
oracle (post a JS message, see what arrives).

### `WpeProducer` changes

```rust
#[cfg(feature = "wpe")]
pub(super) web_messages: std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<String>>>,
```

In `new()` (wpe), after `install_load_signals(&webview, &nav_state)`:

```rust
let web_messages = Rc::new(RefCell::new(VecDeque::new()));
super::script_message::install(&webview, web_messages.clone());
```

Trait methods (replace `Unsupported` defaults):

```rust
fn post_web_message(&mut self, message: &str) -> Result<(), WebSurfaceError> {
    #[cfg(feature = "wpe")] {
        let escaped = super::script_message::escape_for_js(message);
        let script = format!(
            "window.chrome && window.chrome.webview && \
             window.chrome.webview.__scryDispatch && \
             window.chrome.webview.__scryDispatch({escaped});"
        );
        let c = std::ffi::CString::new(script).map_err(|_| WebSurfaceError::Platform(
            "post_web_message: interior NUL".into()))?;
        use glib::translate::ToGlibPtr;
        let raw: *mut super::ffi::WebKitWebView =
            ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&self.handles.webview).0
                as *mut _;
        unsafe {
            super::ffi::webkit_web_view_evaluate_javascript(
                raw, c.as_ptr(), -1,
                std::ptr::null(), std::ptr::null(),
                std::ptr::null_mut(), std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        Ok(())
    }
    #[cfg(not(feature = "wpe"))] {
        let _ = message;
        Err(WebSurfaceError::Unsupported("WpeProducer built without `wpe` feature"))
    }
}

fn poll_web_message(&mut self) -> Option<String> {
    #[cfg(feature = "wpe")] {
        self.web_messages.borrow_mut().pop_front()
    }
    #[cfg(not(feature = "wpe"))] { None }
}
```

Inherent `wait_for_web_message`:

```rust
#[cfg(feature = "wpe")]
impl WpeProducer {
    pub fn wait_for_web_message(&self, timeout: std::time::Duration) -> Option<String> {
        let ctx = &self.handles.main_context;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(m) = self.web_messages.borrow_mut().pop_front() { return Some(m); }
            if std::time::Instant::now() >= deadline { return None; }
            ctx.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
```

### Testing

**Pure-Rust unit tests** for `escape_for_js`:
- Plain ASCII → wrapped in quotes, unchanged.
- Embedded quote → `\"`.
- Embedded newline → `\n`.
- Embedded control byte (`\x01`) → ``.
- Empty string → `""`.

**Runtime smoke** — extend `tests/wpe_input.rs` with a final block: load
inline HTML with `<script>window.chrome.webview.postMessage('hi')</script>`,
call `producer.wait_for_web_message(Duration::from_secs(2))`, assert
`Some("hi")`. If the smoke hangs (analogous to touch on headless),
fall back to documenting the empirical headless caveat and reverting
the smoke addition.

## Implementation order

1. **Task 1** — FFI decls in `ffi.rs` (opaques + constants + 5 fn decls).
2. **Task 2** — `script_message.rs` skeleton: `SCRY_HANDLER_NAME`,
   `CHROME_WEBVIEW_SHIM`, `escape_for_js` + 5 unit tests, `install()`
   stub (don't wire signal yet).
3. **Task 3** — Wire `install()`'s signal closure (the empirical
   JSCValue→String extraction step) + add the queue field to
   `WpeProducer`, init in `new()`.
4. **Task 4** — Promote `post_web_message`/`poll_web_message` trait
   methods + add inherent `wait_for_web_message`.
5. **Task 5** — Runtime smoke: add a post-message assertion to
   `tests/wpe_input.rs`; iterate on headless behavior.
6. **Task 6** — Strategy checklist update (4c.5.a → done).

## Anti-scope-creep guards

- Don't add `scryIme` handler in this phase — that's 4c.4.3's payoff,
  landing in 4c.5.e once the bridge is here.
- Don't expand `chrome.webview` shim beyond the GTK precedent.
- Don't wire 4c.5.b/c/d/f modules.

## Followups

- 4c.5.e IME — install the `scryIme` handler + watch focus/blur/input
  via a user script, post `TextInputState` payloads back through this
  bridge.
- post_web_message currently fire-and-forget (`evaluate_javascript` with
  NULL callback). A future "host-knows-when-the-script-ran" surface
  could use a real `GAsyncReadyCallback` — out of scope.

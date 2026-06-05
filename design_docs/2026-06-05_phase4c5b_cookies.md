# Phase 4c.5.b — Cookies (WPE)

Direct port of `webkitgtk_producer/cookies.rs` (142 lines). Same shape:
get the `WebKitCookieManager` off the producer's `WebKitNetworkSession`,
wrap GAsync ops as sync calls by pumping the producer's `MainContext`
until an `Rc<RefCell<Option<…>>>` cell resolves.

## Scope

In:
- New `scrying/src/wpe_producer/cookies.rs` exposing
  `request_cookies_for_url(url) -> Result<Vec<Cookie>, _>`,
  `set_cookie(&Cookie) -> Result<(), _>`,
  `delete_cookie(&Cookie) -> Result<(), _>`.
- `Cookie` ↔ `soup3::Cookie` translators (`soup_to_scry` / `scry_to_soup`)
  — verbatim from the GTK precedent.
- FFI additions in `ffi.rs`:
  - Opaque `WebKitCookieManager`.
  - `webkit_web_view_get_network_session(webview) -> *mut WebKitNetworkSession`.
  - `webkit_network_session_get_cookie_manager(session) -> *mut WebKitCookieManager`.
  - The 3 op start + 3 op finish FFI decls (`webkit_cookie_manager_*` +
    `_finish`).
- New cargo dep: add `soup` (package `soup3`, version `0.5`) to the
  `wpe` feature's deps. Already used by `webkitgtk-fallback`; same
  pin → coexists cleanly.
- Trait method overrides in `WpeProducer`:
  - `set_cookie(&Cookie) -> Result<(), _>`
  - `delete_cookie(&Cookie) -> Result<(), _>`
  - `request_all_cookies(...)` if the trait has it — check during impl.

Out:
- `set_cookie_change_handler` — separate signal wiring, follow-on
  in a `4c.5.b.1` if there's appetite.
- Cookie persistence policy (`webkit_cookie_manager_set_persistent_storage`).

## Design

### Network-session retrieval

WebKitWebView exposes `webkit_web_view_get_network_session(webview) ->
*mut WebKitNetworkSession` (transfer-none borrow). Use that rather than
holding our own session ref — we explicitly unref'd the construction
ref in 4c.2.

### GAsync ↔ sync bridge

Same shape as the GTK precedent. Each sync method:
1. Get the cookie manager from the network session (via the webview).
2. Create `result: Rc<RefCell<Option<Result<T, String>>>>`.
3. Build a `GAsyncReadyCallback` C trampoline that captures the
   cell's pointer. On invocation it calls the `_finish` FFI to extract
   the value, translates to scrying types, fills the cell.
4. Call the op-start FFI with the trampoline.
5. Pump the producer's `MainContext` until `result.borrow().is_some()`
   or the timeout elapses. Use the existing `pump_until` helper in
   `wpe_producer/headless.rs`.
6. Drain the cell + return.

The trickiest piece: building the C trampoline. Two approaches:

**(A) Pure-FFI trampoline:** write an `extern "C" fn` whose
`user_data: *mut c_void` is a `*const RefCell<Option<...>>`. Inside, cast
back, call the `_finish` FFI, fill the cell. Owns the boxed cell pointer
via `Box::into_raw` at the start; the trampoline takes ownership and
drops the box after filling. The producer holds an `Rc` clone for
draining.

**(B) glib closure:** glib's `gio` bindings expose `Cancellable` and
async result wrappers, but we're not pulling `gio` directly. (A) is
simpler.

Go with (A). The trampoline lives in `cookies.rs` as a private
`extern "C" fn cookie_get_done(source: *mut GObject, result: *mut GAsyncResult,
user_data: *mut c_void)` style fn.

### FFI surface

```rust
#[repr(C)] pub struct WebKitCookieManager { _opaque: [u8; 0] }
// GAsyncResult is opaque from the C side too; we declare it as opaque.
#[repr(C)] pub struct GAsyncResult { _opaque: [u8; 0] }

type GAsyncReadyCallback = unsafe extern "C" fn(
    source: *mut glib::gobject_ffi::GObject,
    result: *mut GAsyncResult,
    user_data: *mut std::ffi::c_void,
);

unsafe extern "C" {
    pub fn webkit_web_view_get_network_session(
        web_view: *mut WebKitWebView,
    ) -> *mut WebKitNetworkSession;
    pub fn webkit_network_session_get_cookie_manager(
        session: *mut WebKitNetworkSession,
    ) -> *mut WebKitCookieManager;
    pub fn webkit_cookie_manager_get_cookies(
        manager: *mut WebKitCookieManager,
        uri: *const c_char,
        cancellable: *mut std::ffi::c_void, // NULL
        callback: GAsyncReadyCallback,
        user_data: *mut std::ffi::c_void,
    );
    pub fn webkit_cookie_manager_get_cookies_finish(
        manager: *mut WebKitCookieManager,
        result: *mut GAsyncResult,
        error: *mut *mut glib::ffi::GError,
    ) -> *mut glib::ffi::GList; // list of SoupCookie*
    pub fn webkit_cookie_manager_add_cookie(
        manager: *mut WebKitCookieManager,
        cookie: *mut std::ffi::c_void, // SoupCookie* — opaque from our side
        cancellable: *mut std::ffi::c_void,
        callback: GAsyncReadyCallback,
        user_data: *mut std::ffi::c_void,
    );
    pub fn webkit_cookie_manager_add_cookie_finish(
        manager: *mut WebKitCookieManager,
        result: *mut GAsyncResult,
        error: *mut *mut glib::ffi::GError,
    ) -> c_int; // gboolean
    pub fn webkit_cookie_manager_delete_cookie(
        manager: *mut WebKitCookieManager,
        cookie: *mut std::ffi::c_void,
        cancellable: *mut std::ffi::c_void,
        callback: GAsyncReadyCallback,
        user_data: *mut std::ffi::c_void,
    );
    pub fn webkit_cookie_manager_delete_cookie_finish(
        manager: *mut WebKitCookieManager,
        result: *mut GAsyncResult,
        error: *mut *mut glib::ffi::GError,
    ) -> c_int;
}
```

The `*mut c_void` for `cookie` in add/delete is intentional — we pass a
`SoupCookie *` from the `soup3` crate via `to_glib_full()`/`to_glib_none()`.

### `cookies.rs` shape

```rust
//! Cookie store API (WPE) — direct port of webkitgtk_producer/cookies.rs.
//! Get the WebKitCookieManager off the WebView's WebKitNetworkSession,
//! wrap GAsync ops as sync calls by pumping the producer's MainContext.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use glib::translate::{FromGlibPtrFull, ToGlibPtr};
use soup::Cookie as SoupCookie;

use super::ffi;
use super::producer::WpeProducer;
use crate::{Cookie, WebSurfaceError};

const COOKIE_OP_TIMEOUT: Duration = Duration::from_secs(3);

impl WpeProducer {
    fn cookie_manager(&self) -> *mut ffi::WebKitCookieManager {
        use glib::translate::ToGlibPtr;
        let raw_view: *mut ffi::WebKitWebView =
            ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(
                &self.handles.webview,
            ).0 as *mut _;
        // SAFETY: webview borrowed for the call; both getters are
        // transfer-none (the WebView owns its network session; the
        // network session owns its cookie manager).
        unsafe {
            let session = ffi::webkit_web_view_get_network_session(raw_view);
            ffi::webkit_network_session_get_cookie_manager(session)
        }
    }

    pub fn request_cookies_for_url(&self, url: &str) -> Result<Vec<Cookie>, WebSurfaceError> {
        let manager = self.cookie_manager();
        let c_url = std::ffi::CString::new(url).map_err(|_| WebSurfaceError::Platform(
            "request_cookies_for_url: URL contained interior NUL".into()))?;
        let result: Rc<RefCell<Option<Result<Vec<Cookie>, String>>>> =
            Rc::new(RefCell::new(None));
        // Box the (manager, cell) tuple so the trampoline can recover it.
        let boxed = Box::new((manager, result.clone()));
        let raw_ud = Box::into_raw(boxed) as *mut std::ffi::c_void;
        unsafe {
            ffi::webkit_cookie_manager_get_cookies(
                manager, c_url.as_ptr(), std::ptr::null_mut(),
                cookie_get_trampoline, raw_ud,
            );
        }
        // Pump until the trampoline fills the cell or timeout.
        let deadline = Instant::now() + COOKIE_OP_TIMEOUT;
        super::headless::pump_until(
            &self.handles.main_context, deadline,
            || result.borrow().is_some(),
        )?;
        let value = result.borrow_mut().take().ok_or(
            WebSurfaceError::NotReady("cookie get did not complete in time")
        )?;
        value.map_err(|e| WebSurfaceError::Platform(format!("cookie get failed: {e}")))
    }

    pub fn set_cookie(&self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        let manager = self.cookie_manager();
        let mut soup_cookie = scry_to_soup(cookie);
        let result: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
        let boxed = Box::new((manager, result.clone()));
        let raw_ud = Box::into_raw(boxed) as *mut std::ffi::c_void;
        let raw_cookie: *mut std::ffi::c_void = unsafe {
            // soup3 0.5's Cookie::to_glib_full returns a transfer-full
            // SoupCookie* — webkit_cookie_manager_add_cookie consumes
            // its own copy via soup_cookie_copy internally, so we still
            // own the soup_cookie reference returned. Use to_glib_none
            // for borrowed semantics; webkit will copy.
            ToGlibPtr::<*mut soup::ffi::SoupCookie>::to_glib_none(&mut soup_cookie).0
                as *mut _
        };
        unsafe {
            ffi::webkit_cookie_manager_add_cookie(
                manager, raw_cookie, std::ptr::null_mut(),
                cookie_op_trampoline, raw_ud,
            );
        }
        let deadline = Instant::now() + COOKIE_OP_TIMEOUT;
        super::headless::pump_until(
            &self.handles.main_context, deadline,
            || result.borrow().is_some(),
        )?;
        let value = result.borrow_mut().take().ok_or(
            WebSurfaceError::NotReady("cookie add did not complete in time")
        )?;
        value.map_err(|e| WebSurfaceError::Platform(format!("cookie add failed: {e}")))
    }

    pub fn delete_cookie(&self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        // Mirror of set_cookie but calls webkit_cookie_manager_delete_cookie.
        // ...
    }
}

unsafe extern "C" fn cookie_get_trampoline(
    _source: *mut glib::gobject_ffi::GObject,
    result: *mut ffi::GAsyncResult,
    user_data: *mut std::ffi::c_void,
) {
    // SAFETY: user_data is the Box<(manager, Rc<RefCell<Option<Result<Vec<Cookie>, String>>>>)>
    // that the get_cookies caller leaked via Box::into_raw. We take it
    // back, run _finish to extract the GList<SoupCookie*>, translate
    // each, fill the cell, and drop the Box (which drops one Rc).
    let boxed: Box<(*mut ffi::WebKitCookieManager,
                    Rc<RefCell<Option<Result<Vec<Cookie>, String>>>>)> =
        unsafe { Box::from_raw(user_data as *mut _) };
    let (manager, cell) = *boxed;
    let mut error: *mut glib::ffi::GError = std::ptr::null_mut();
    let g_list = unsafe {
        ffi::webkit_cookie_manager_get_cookies_finish(manager, result, &mut error)
    };
    if !error.is_null() {
        let msg = unsafe { glib::translate::from_glib_full::<*mut glib::ffi::GError, glib::Error>(error) }
            .message().to_string();
        *cell.borrow_mut() = Some(Err(msg));
        return;
    }
    // Walk the GList<SoupCookie*>, translate each.
    let mut cookies = Vec::new();
    let mut cur = g_list;
    while !cur.is_null() {
        let item = unsafe { (*cur).data as *mut soup::ffi::SoupCookie };
        if !item.is_null() {
            // SoupCookie reference is held by the GList; from_glib_none
            // adds a ref the Rust wrapper will drop. The list itself is
            // freed below; the wrapper's ref keeps the cookie alive
            // long enough for soup_to_scry to extract its fields.
            let sc: SoupCookie = unsafe { glib::translate::from_glib_none(item) };
            cookies.push(soup_to_scry(sc));
        }
        cur = unsafe { (*cur).next };
    }
    // Free the GList (but not the SoupCookies — they're owned by their
    // wrappers now).
    unsafe { glib::ffi::g_list_free(g_list); }
    *cell.borrow_mut() = Some(Ok(cookies));
}

unsafe extern "C" fn cookie_op_trampoline(
    _source: *mut glib::gobject_ffi::GObject,
    result: *mut ffi::GAsyncResult,
    user_data: *mut std::ffi::c_void,
) {
    // For add/delete: _finish returns gboolean + GError**.
    // SAFETY: user_data is the boxed (manager, cell) pointer.
    let boxed: Box<(*mut ffi::WebKitCookieManager,
                    Rc<RefCell<Option<Result<(), String>>>>)> =
        unsafe { Box::from_raw(user_data as *mut _) };
    let (manager, cell) = *boxed;
    let mut error: *mut glib::ffi::GError = std::ptr::null_mut();
    let ok = unsafe {
        ffi::webkit_cookie_manager_add_cookie_finish(manager, result, &mut error)
    };
    if !error.is_null() {
        let msg = unsafe { glib::translate::from_glib_full::<_, glib::Error>(error) }
            .message().to_string();
        *cell.borrow_mut() = Some(Err(msg));
        return;
    }
    if ok == 0 {
        *cell.borrow_mut() = Some(Err("operation returned FALSE without setting GError".into()));
        return;
    }
    *cell.borrow_mut() = Some(Ok(()));
}

// --- translators (verbatim from GTK) ---

fn soup_to_scry(mut sc: SoupCookie) -> Cookie {
    Cookie {
        name: sc.name().map(|g| g.to_string()).unwrap_or_default(),
        value: sc.value().map(|g| g.to_string()).unwrap_or_default(),
        domain: sc.domain().map(|g| g.to_string()).unwrap_or_default(),
        path: sc.path().map(|g| g.to_string()).unwrap_or_default(),
        expires_at: sc.expires().map(|dt| dt.to_unix() as f64),
        is_secure: sc.is_secure(),
        is_http_only: sc.is_http_only(),
    }
}

fn scry_to_soup(c: &Cookie) -> SoupCookie {
    let max_age = match c.expires_at {
        Some(ts) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as f64)
                .unwrap_or(0.0);
            let seconds = (ts - now).max(0.0) as i32;
            seconds.max(1)
        }
        None => -1,
    };
    let mut sc = SoupCookie::new(&c.name, &c.value, &c.domain, &c.path, max_age);
    sc.set_secure(c.is_secure);
    sc.set_http_only(c.is_http_only);
    sc
}
```

**Wrinkle: the trampoline can't have two distinct signatures.** I wrote
two trampolines (`cookie_get_trampoline` for the GList-returning get,
`cookie_op_trampoline` for the gboolean-returning add/delete) because
their `_finish` calls + cell value types differ. Delete reuses the
op trampoline by pointing at `webkit_cookie_manager_delete_cookie_finish`
— either: (a) have a separate `cookie_delete_trampoline`, or (b)
generic-on-a-function-pointer-stored-in-the-box. (a) is simpler; do (a).

So actually:
- `cookie_get_trampoline` (GList finisher)
- `cookie_add_trampoline` (calls add_cookie_finish)
- `cookie_delete_trampoline` (calls delete_cookie_finish)

Three trampolines. Each ~25 lines; total still <100 LOC.

### Trait method overrides in producer.rs

The trait has `set_cookie` and `set_cookie_change_handler` per my earlier
grep. The first is what we're shipping; the second is out-of-scope here.
What about `request_all_cookies` / `delete_cookie`? Check during impl —
if they don't exist as trait methods, inherent on `WpeProducer` is fine
(mirroring the GTK precedent's inherent shape).

For the cookie change handler trait method: leave the default
`Unsupported` in place; it's a separate signal-wiring chunk.

### Smoke

Extend `tests/wpe_input.rs` (or add a new integration test): set a
cookie, then `request_cookies_for_url(...)`, assert the cookie's
present. Be cautious — if any cookie op hangs on headless (touch did,
post-message didn't), revert + document. Same headless-caveat playbook.

## Implementation order

Single dispatch — the shape is tightly connected and easier to get right
holistically:

1. `Cargo.toml` — add `soup = { package = "soup3", version = "0.5", optional = true }` to the wpe feature's deps (already pinned for `webkitgtk-fallback`).
2. `ffi.rs` — opaques + 8 fn decls.
3. `cookies.rs` — translators + cookie_manager + 3 sync methods + 3 trampolines + soup-cookie wrapper-via-from_glib_none.
4. `producer.rs` — promote trait method overrides.
5. `mod.rs` — declare the module.
6. Smoke addition in `tests/wpe_input.rs`.
7. Strategy doc: `4c.5.b` checkbox.

If a hang appears at any step, revert just that addition + document the
headless caveat in `cookies.rs`'s module doc.

## Empirical risks

- **soup3 0.5 version pin clash** with the existing webkitgtk-fallback
  dep. Both should land on the same version; if not, that's a Cargo
  resolver issue to investigate.
- **`webkit_web_view_get_network_session` may not exist on WPE 2.52.3
  WebView.** Verify the header. If absent, hold a `*mut WebKitNetworkSession`
  ref in `WpeHandles` (release the existing g_object_unref at
  build_producer_view + retain instead).
- **Cookie op hangs on headless** — analogous to touch. If it hangs,
  revert + document.

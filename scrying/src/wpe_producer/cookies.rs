// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Cookie store API (WPE) — direct port of `webkitgtk_producer/cookies.rs`.
//!
//! WPE's cookie manager hangs off the WebView's `WebKitNetworkSession`
//! (the ephemeral session the producer construction attaches in
//! [`super::headless::build_producer_view`] and then drops its own ref
//! to — the WebView keeps it alive). We recover it on demand via
//! `webkit_web_view_get_network_session` (transfer-none borrow) +
//! `webkit_network_session_get_cookie_manager` (also transfer-none),
//! so no cookie-side state needs to live on [`WpeProducer`] itself.
//!
//! The op-start FFIs are GAsync (fire-and-forget); their results land
//! through a `GAsyncReadyCallback` trampoline that the matching
//! `*_finish` FFI then decodes. We bridge that into a synchronous call
//! by stuffing the result into an `Rc<RefCell<Option<...>>>` cell and
//! pumping the producer's affine `MainContext` via
//! [`super::headless::pump_until`] until the cell fills (or the deadline
//! elapses).
//!
//! ## Trampoline payload contract
//!
//! Each sync call boxes a `(manager_ptr, Rc<RefCell<Option<...>>>)`
//! tuple, `Box::into_raw`s it, and passes the raw pointer as
//! `user_data`. The C-side trampoline takes the box back with
//! `Box::from_raw`, calls the matching `*_finish` FFI to extract the
//! value, writes it into the cell (the caller's clone of the same `Rc`
//! observes the write), and drops the box (releasing one Rc). Returning
//! the manager pointer through the box rather than capturing it in the
//! caller's closure mirrors the way the gtk precedent's higher-level
//! `manager.add_cookie(...)` wrapper hides it — same shape, just
//! hand-rolled.
//!
//! Three trampolines, not one — `_get` returns `*mut GList`, `_add`
//! and `_delete` return `gboolean`. A single generic trampoline would
//! need the `*_finish` fn-pointer in the box; three specialised ones
//! are simpler and total <100 LOC.
//!
//! ## Scope
//!
//! In: `request_cookies_for_url`, `set_cookie`, `delete_cookie`.
//! Out (separate signal-wiring chunk): `set_cookie_change_handler`,
//! cookie persistence policy (`webkit_cookie_manager_set_persistent_storage`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use glib::translate::ToGlibPtr;
use soup::Cookie as SoupCookie;

use super::ffi;
use super::producer::WpeProducer;
use crate::{Cookie, WebSurfaceError};

/// Deadline budget for any single cookie op. Matches the GTK precedent.
const COOKIE_OP_TIMEOUT: Duration = Duration::from_secs(3);

impl WpeProducer {
    /// Borrow the cookie manager off the producer's WebView via the
    /// network session. Both intermediate getters are transfer-none
    /// (the WebView owns the session; the session owns the manager) —
    /// no ref bookkeeping needed.
    fn cookie_manager(&self) -> *mut ffi::WebKitCookieManager {
        let raw_view: *mut ffi::WebKitWebView =
            ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&self.handles.webview).0
                as *mut _;
        // SAFETY: `raw_view` is borrowed from the owned `handles.webview`
        // and remains valid for the call. The two getters are
        // transfer-none chained — the WebView owns its network session
        // (we explicitly drop the construction ref in
        // `build_producer_view` because g_object_new takes its own), and
        // the network session owns its cookie manager. Both pointers are
        // valid for the duration of the WebView's lifetime, which spans
        // the producer's.
        unsafe {
            let session = ffi::webkit_web_view_get_network_session(raw_view);
            ffi::webkit_network_session_get_cookie_manager(session)
        }
    }

    /// Fetch all cookies the store currently has for `url`, blocking
    /// until the async fetch completes (or `COOKIE_OP_TIMEOUT` elapses).
    /// Mirrors the GTK precedent's shape; WPEWebKit's underlying API is
    /// per-URI rather than store-wide. Hosts that need full enumeration
    /// can iterate known origins.
    pub fn request_cookies_for_url(&self, url: &str) -> Result<Vec<Cookie>, WebSurfaceError> {
        let manager = self.cookie_manager();
        let c_url = std::ffi::CString::new(url).map_err(|_| {
            WebSurfaceError::Platform("request_cookies_for_url: URL contained interior NUL".into())
        })?;
        let result: Rc<RefCell<Option<Result<Vec<Cookie>, String>>>> = Rc::new(RefCell::new(None));
        // The trampoline takes ownership of one Rc clone via the box;
        // we keep the other Rc clone here for the drain after pumping.
        let boxed: Box<GetUserData> = Box::new((manager, result.clone()));
        let raw_ud = Box::into_raw(boxed) as *mut std::ffi::c_void;
        // SAFETY: `manager`, `c_url`, and `raw_ud` are valid for the
        // entire async-op lifetime; webkit copies the URL string before
        // returning. NULL cancellable is the documented "no cancellation"
        // sentinel. The trampoline takes back the user_data on
        // completion via Box::from_raw — exactly one trampoline call per
        // op, so the box is reclaimed exactly once.
        unsafe {
            ffi::webkit_cookie_manager_get_cookies(
                manager,
                c_url.as_ptr(),
                std::ptr::null_mut(),
                cookie_get_trampoline,
                raw_ud,
            );
        }
        let deadline = Instant::now() + COOKIE_OP_TIMEOUT;
        super::headless::pump_until(&self.handles.main_context, deadline, || {
            result.borrow().is_some()
        })?;
        let value = result.borrow_mut().take().ok_or(WebSurfaceError::NotReady(
            "WPE cookie get did not deliver in time",
        ))?;
        value.map_err(|e| WebSurfaceError::Platform(format!("cookie get failed: {e}")))
    }

    /// Add a cookie to the store, blocking until the async add
    /// completes. Mirrors the GTK precedent.
    pub fn set_cookie(&self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        self.run_op_cookie(cookie, CookieOp::Add)
    }

    /// Delete a cookie from the store, blocking until the async delete
    /// completes. The cookie's `name`, `domain`, `path` identify the
    /// entry; other fields are ignored.
    pub fn delete_cookie(&self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        self.run_op_cookie(cookie, CookieOp::Delete)
    }

    /// Shared shape behind `set_cookie` / `delete_cookie` — the only
    /// difference is which start/finish FFI pair runs, captured by
    /// [`CookieOp`].
    fn run_op_cookie(&self, cookie: &Cookie, op: CookieOp) -> Result<(), WebSurfaceError> {
        if matches!(op, CookieOp::Add) {
            if cookie.same_site.is_some() {
                return Err(WebSurfaceError::Unsupported(
                    "WPE cookie manager path does not expose SameSite setters in scrying yet",
                ));
            }
            if cookie.partitioned {
                return Err(WebSurfaceError::Unsupported(
                    "WPE cookie manager path does not expose Partitioned cookie setters in scrying yet",
                ));
            }
        }
        let manager = self.cookie_manager();
        // soup3 0.5's `to_glib_none` for `Cookie` takes `&mut self`,
        // matching the precedent — materialize a local mutable cookie.
        let mut soup_cookie = scry_to_soup(cookie);
        let result: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
        let boxed: Box<OpUserData> = Box::new((manager, result.clone()));
        let raw_ud = Box::into_raw(boxed) as *mut std::ffi::c_void;
        // SAFETY: `soup_cookie`'s to_glib_none returns a SoupCookie*
        // borrowed from the local; webkit_cookie_manager_{add,delete}_cookie
        // copies its own reference of the cookie struct (per the WebKit
        // C contract — the SoupCookie passed in is `transfer-none`).
        let raw_cookie: *mut std::ffi::c_void =
            ToGlibPtr::<*mut soup::ffi::SoupCookie>::to_glib_none(&mut soup_cookie).0
                as *mut std::ffi::c_void;
        let (start, trampoline): (CookieOpStart, ffi::GAsyncReadyCallback) = match op {
            CookieOp::Add => (ffi::webkit_cookie_manager_add_cookie, cookie_add_trampoline),
            CookieOp::Delete => (
                ffi::webkit_cookie_manager_delete_cookie,
                cookie_delete_trampoline,
            ),
        };
        // SAFETY: all four pointer args are valid for the call;
        // `soup_cookie` is owned for the rest of this fn so its raw
        // alias remains valid during the synchronous start call. NULL
        // cancellable is the documented sentinel. Exactly one
        // trampoline invocation per op reclaims the boxed user_data.
        unsafe {
            start(
                manager,
                raw_cookie,
                std::ptr::null_mut(),
                trampoline,
                raw_ud,
            );
        }
        let deadline = Instant::now() + COOKIE_OP_TIMEOUT;
        super::headless::pump_until(&self.handles.main_context, deadline, || {
            result.borrow().is_some()
        })?;
        result
            .borrow_mut()
            .take()
            .ok_or(WebSurfaceError::NotReady(
                "WPE cookie op did not complete in time",
            ))?
            .map_err(|e| WebSurfaceError::Platform(format!("cookie op failed: {e}")))
    }
}

/// Selector for [`WpeProducer::run_op_cookie`].
#[derive(Clone, Copy)]
enum CookieOp {
    Add,
    Delete,
}

/// Fn-pointer alias for the add/delete start FFIs (they share a
/// signature — only the symbol and finish-side decode differ).
type CookieOpStart = unsafe extern "C" fn(
    manager: *mut ffi::WebKitCookieManager,
    cookie: *mut std::ffi::c_void,
    cancellable: *mut std::ffi::c_void,
    callback: ffi::GAsyncReadyCallback,
    user_data: *mut std::ffi::c_void,
);

/// Trampoline user-data payload for `request_cookies_for_url`.
type GetUserData = (
    *mut ffi::WebKitCookieManager,
    Rc<RefCell<Option<Result<Vec<Cookie>, String>>>>,
);

/// Trampoline user-data payload for `set_cookie` / `delete_cookie`.
type OpUserData = (
    *mut ffi::WebKitCookieManager,
    Rc<RefCell<Option<Result<(), String>>>>,
);

/// Trampoline for `webkit_cookie_manager_get_cookies`. Calls the
/// `_finish` FFI to extract a `GList<SoupCookie*>`, translates each
/// entry to a scry [`Cookie`], frees the list, and writes the result
/// into the cell.
unsafe extern "C" fn cookie_get_trampoline(
    _source: *mut glib::gobject_ffi::GObject,
    result: *mut ffi::GAsyncResult,
    user_data: *mut std::ffi::c_void,
) {
    // SAFETY: `user_data` was produced by `Box::into_raw` on a
    // `Box<GetUserData>` immediately before the matching
    // `webkit_cookie_manager_get_cookies` call, and webkit guarantees
    // the callback fires exactly once per op. Recovering the box here
    // reclaims that single allocation.
    let boxed: Box<GetUserData> = unsafe { Box::from_raw(user_data as *mut GetUserData) };
    let (manager, cell) = *boxed;
    let mut error: *mut glib::ffi::GError = std::ptr::null_mut();
    // SAFETY: `manager` is the transfer-none pointer the start call
    // captured; the WebView (and thus the manager) outlives the async
    // op because the producer is thread-affine and we don't drop the
    // producer between op-start and op-finish. `result` is the per-op
    // GAsyncResult webkit hands us.
    let g_list =
        unsafe { ffi::webkit_cookie_manager_get_cookies_finish(manager, result, &mut error) };
    if !error.is_null() {
        // SAFETY: `error` is a transfer-full GError* from the _finish
        // FFI; `from_glib_full` adopts ownership and frees on drop.
        let msg = unsafe {
            glib::translate::from_glib_full::<*mut glib::ffi::GError, glib::Error>(error)
        }
        .message()
        .to_string();
        *cell.borrow_mut() = Some(Err(msg));
        return;
    }
    let mut cookies = Vec::new();
    let mut cur = g_list;
    while !cur.is_null() {
        // SAFETY: walking the GList; (*cur).data is a SoupCookie* the
        // list owns (transfer-full on the list, transfer-none for each
        // element — `from_glib_none` adds a ref the Rust wrapper
        // releases on Drop, keeping the cookie alive while we copy
        // its fields out in soup_to_scry).
        let item = unsafe { (*cur).data as *mut soup::ffi::SoupCookie };
        if !item.is_null() {
            let sc: SoupCookie = unsafe { glib::translate::from_glib_none(item) };
            cookies.push(soup_to_scry(sc));
        }
        // SAFETY: GList nodes are valid until g_list_free below.
        cur = unsafe { (*cur).next };
    }
    // SAFETY: g_list was transfer-full from _finish; free the list
    // nodes. The SoupCookies the list referenced are now owned by the
    // Rust wrappers we created via from_glib_none (each holds its own
    // ref), so freeing the list doesn't dangle our pointers.
    unsafe {
        glib::ffi::g_list_free(g_list);
    }
    *cell.borrow_mut() = Some(Ok(cookies));
}

/// Trampoline for `webkit_cookie_manager_add_cookie`.
unsafe extern "C" fn cookie_add_trampoline(
    _source: *mut glib::gobject_ffi::GObject,
    result: *mut ffi::GAsyncResult,
    user_data: *mut std::ffi::c_void,
) {
    // SAFETY: see `cookie_get_trampoline` — same single-fire contract,
    // payload type is `OpUserData` here.
    let boxed: Box<OpUserData> = unsafe { Box::from_raw(user_data as *mut OpUserData) };
    let (manager, cell) = *boxed;
    let mut error: *mut glib::ffi::GError = std::ptr::null_mut();
    let ok = unsafe { ffi::webkit_cookie_manager_add_cookie_finish(manager, result, &mut error) };
    finish_op_into_cell(ok, error, &cell);
}

/// Trampoline for `webkit_cookie_manager_delete_cookie`.
unsafe extern "C" fn cookie_delete_trampoline(
    _source: *mut glib::gobject_ffi::GObject,
    result: *mut ffi::GAsyncResult,
    user_data: *mut std::ffi::c_void,
) {
    // SAFETY: see `cookie_get_trampoline` — same single-fire contract.
    let boxed: Box<OpUserData> = unsafe { Box::from_raw(user_data as *mut OpUserData) };
    let (manager, cell) = *boxed;
    let mut error: *mut glib::ffi::GError = std::ptr::null_mut();
    let ok =
        unsafe { ffi::webkit_cookie_manager_delete_cookie_finish(manager, result, &mut error) };
    finish_op_into_cell(ok, error, &cell);
}

/// Shared decode for the add/delete trampolines: a gboolean + GError**
/// pair becomes a `Result<(), String>` written into the result cell.
fn finish_op_into_cell(
    ok: std::os::raw::c_int,
    error: *mut glib::ffi::GError,
    cell: &Rc<RefCell<Option<Result<(), String>>>>,
) {
    if !error.is_null() {
        // SAFETY: see cookie_get_trampoline — transfer-full GError.
        let msg = unsafe {
            glib::translate::from_glib_full::<*mut glib::ffi::GError, glib::Error>(error)
        }
        .message()
        .to_string();
        *cell.borrow_mut() = Some(Err(msg));
        return;
    }
    if ok == 0 {
        *cell.borrow_mut() = Some(Err("operation returned FALSE without setting GError".into()));
        return;
    }
    *cell.borrow_mut() = Some(Ok(()));
}

// --- translators (verbatim from the GTK precedent) ---

fn soup_to_scry(mut sc: SoupCookie) -> Cookie {
    Cookie {
        name: sc.name().map(|g| g.to_string()).unwrap_or_default(),
        value: sc.value().map(|g| g.to_string()).unwrap_or_default(),
        domain: sc.domain().map(|g| g.to_string()).unwrap_or_default(),
        path: sc.path().map(|g| g.to_string()).unwrap_or_default(),
        expires_at: sc.expires().map(|dt| dt.to_unix() as f64),
        is_secure: sc.is_secure(),
        is_http_only: sc.is_http_only(),
        same_site: None,
        partitioned: false,
    }
}

fn scry_to_soup(c: &Cookie) -> SoupCookie {
    // libsoup's `Cookie::new` takes `max_age` in seconds (`-1` = session
    // cookie). Convert from absolute Unix timestamp.
    let max_age = match c.expires_at {
        Some(ts) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as f64)
                .unwrap_or(0.0);
            let seconds = (ts - now).max(0.0) as i32;
            // libsoup treats `0` as "expire immediately"; clamp so a
            // cookie that expired in the past isn't silently accepted
            // as a session cookie.
            seconds.max(1)
        }
        None => -1,
    };
    let mut sc = SoupCookie::new(&c.name, &c.value, &c.domain, &c.path, max_age);
    sc.set_secure(c.is_secure);
    sc.set_http_only(c.is_http_only);
    sc
}

// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Cookie store API — host-driven get / set / delete via the
//! `WebsiteDataManager::cookie_manager`.
//!
//! WebKitGTK 4.1's `webkit_cookie_manager_get_cookies` reads
//! per-URI rather than enumerating the whole store. The producer
//! exposes a per-URI fetch (`request_cookies_for_url`) plus
//! `set_cookie` / `delete_cookie`. Hosts that want a full enumeration
//! can iterate known origins. A future
//! `webkit_cookie_manager_get_all_cookies` upgrade (WebKitGTK 2.42+)
//! would offer enumeration directly — gated behind a newer feature
//! flag when we move the binding pin.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soup::Cookie as SoupCookie;
use webkit2gtk::{CookieManager, CookieManagerExt, WebsiteDataManagerExt};

use crate::{Cookie, WebSurfaceError};

use super::helpers::pump_until;
use super::producer::WebKitGtkProducer;

impl WebKitGtkProducer {
    fn cookie_manager(&self) -> Result<CookieManager, WebSurfaceError> {
        self._data_manager.cookie_manager().ok_or_else(|| {
            WebSurfaceError::Platform("WebsiteDataManager has no cookie manager".into())
        })
    }

    /// Fetch all cookies the store currently has for `url`, blocking
    /// until the async fetch completes (or `frame_timeout` elapses).
    /// Mirrors the macOS producer's `request_all_cookies` shape;
    /// WebKitGTK 4.1's underlying API is per-URI rather than
    /// store-wide.
    pub fn request_cookies_for_url(&self, url: &str) -> Result<Vec<Cookie>, WebSurfaceError> {
        let manager = self.cookie_manager()?;
        let result: Rc<RefCell<Option<Result<Vec<Cookie>, String>>>> = Rc::new(RefCell::new(None));
        let r = result.clone();
        manager.cookies(url, webkit2gtk::gio::Cancellable::NONE, move |res| {
            let translated = match res {
                Ok(cookies) => Ok(cookies.into_iter().map(soup_to_scry).collect()),
                Err(e) => Err(e.to_string()),
            };
            *r.borrow_mut() = Some(translated);
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        pump_until(deadline, || result.borrow().is_some())?;
        let res = result.borrow_mut().take().ok_or(WebSurfaceError::NotReady(
            "WebKitGTK cookie get did not deliver in time",
        ))?;
        res.map_err(|e| WebSurfaceError::Platform(format!("cookie get failed: {e}")))
    }

    /// Add a cookie to the store, blocking until the async add
    /// completes.
    pub fn set_cookie(&self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        if cookie.same_site.is_some() {
            return Err(WebSurfaceError::Unsupported(
                "WebKitGTK cookie manager path does not expose SameSite setters in scrying yet",
            ));
        }
        if cookie.partitioned {
            return Err(WebSurfaceError::Unsupported(
                "WebKitGTK cookie manager path does not expose Partitioned cookie setters in scrying yet",
            ));
        }
        // soup3's auto-generated bindings take `&mut Cookie` even for
        // operations that don't mutate semantically — getters too.
        // Materialize a local mutable cookie for the call.
        let mut soup_cookie = scry_to_soup(cookie);
        let manager = self.cookie_manager()?;
        let done: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
        let d = done.clone();
        manager.add_cookie(
            &mut soup_cookie,
            webkit2gtk::gio::Cancellable::NONE,
            move |res| {
                *d.borrow_mut() = Some(res.map_err(|e| e.to_string()));
            },
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        pump_until(deadline, || done.borrow().is_some())?;
        done.borrow_mut()
            .take()
            .ok_or(WebSurfaceError::NotReady(
                "WebKitGTK cookie add did not complete in time",
            ))?
            .map_err(|e| WebSurfaceError::Platform(format!("cookie add failed: {e}")))
    }

    /// Delete a cookie from the store, blocking until the async
    /// delete completes. The cookie's `name`, `domain`, `path` are
    /// used to identify the entry; other fields are ignored.
    pub fn delete_cookie(&self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        let mut soup_cookie = scry_to_soup(cookie);
        let manager = self.cookie_manager()?;
        let done: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
        let d = done.clone();
        manager.delete_cookie(
            &mut soup_cookie,
            webkit2gtk::gio::Cancellable::NONE,
            move |res| {
                *d.borrow_mut() = Some(res.map_err(|e| e.to_string()));
            },
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        pump_until(deadline, || done.borrow().is_some())?;
        done.borrow_mut()
            .take()
            .ok_or(WebSurfaceError::NotReady(
                "WebKitGTK cookie delete did not complete in time",
            ))?
            .map_err(|e| WebSurfaceError::Platform(format!("cookie delete failed: {e}")))
    }
}

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
    // libsoup's `Cookie::new` takes `max_age` in seconds (`-1` =
    // session cookie). Convert from absolute Unix timestamp.
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

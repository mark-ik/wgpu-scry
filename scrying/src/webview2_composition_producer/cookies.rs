// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

impl WebView2CompositionProducer {
    /// Kick off an async fetch of every cookie in the WebView2 profile's
    /// cookie manager. Drain via [`Self::poll_cookies`].
    pub fn request_all_cookies(&mut self) -> Result<(), WebSurfaceError> {
        let manager = self.cookie_manager()?;
        let slot = self.pending_cookies.clone();
        let handler = GetCookiesCompletedHandler::create(Box::new(move |result, cookie_list| {
            result?;
            if let Some(cookie_list) = cookie_list {
                match unsafe { cookies_from_webview2_list(&cookie_list) } {
                    Ok(cookies) => {
                        if let Ok(mut pending) = slot.lock() {
                            *pending = Some(cookies);
                        }
                    }
                    Err(error) => {
                        eprintln!("scrying: WebView2 cookie conversion failed: {error}");
                    }
                }
            }
            Ok(())
        }));
        unsafe { manager.GetCookies(PCWSTR::null(), &handler) }
            .map_err(platform("CookieManager.GetCookies"))
    }

    /// Drain the most recent [`Self::request_all_cookies`] result.
    pub fn poll_cookies(&mut self) -> Option<Vec<Cookie>> {
        self.pending_cookies.lock().ok().and_then(|mut s| s.take())
    }

    /// Set / overwrite a cookie in the WebView2 profile's cookie manager.
    pub fn set_cookie(&mut self, cookie: &Cookie) -> Result<(), WebSurfaceError> {
        let manager = self.cookie_manager()?;
        let webview_cookie = unsafe { webview2_cookie_from(&manager, cookie)? };
        unsafe { manager.AddOrUpdateCookie(&webview_cookie) }
            .map_err(platform("CookieManager.AddOrUpdateCookie"))?;
        self.notify_cookie_changed();
        Ok(())
    }

    /// Delete a cookie by name + domain + path.
    pub fn delete_cookie(
        &mut self,
        name: &str,
        domain: &str,
        path: &str,
    ) -> Result<(), WebSurfaceError> {
        let manager = self.cookie_manager()?;
        let name = CoTaskMemPWSTR::from(name);
        let domain = CoTaskMemPWSTR::from(domain);
        let path = CoTaskMemPWSTR::from(path);
        unsafe {
            manager.DeleteCookiesWithDomainAndPath(
                *name.as_ref().as_pcwstr(),
                *domain.as_ref().as_pcwstr(),
                *path.as_ref().as_pcwstr(),
            )
        }
        .map_err(platform("CookieManager.DeleteCookiesWithDomainAndPath"))?;
        self.notify_cookie_changed();
        Ok(())
    }

    /// Register a best-effort cookie-change callback. This fires for host
    /// `set_cookie` / `delete_cookie` calls, page-side `document.cookie`
    /// writes observed by scrying's document-start script, and native
    /// `Set-Cookie` response headers observed through WebView2's
    /// `WebResourceResponseReceived` event.
    pub fn set_cookie_change_handler(
        &mut self,
        handler: WebView2CookieChangeHandlerFn,
    ) -> Result<(), WebSurfaceError> {
        let mut slot = self
            .cookie_change_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("cookie_change_handler lock poisoned".into()))?;
        *slot = Some(handler);
        Ok(())
    }

    pub fn clear_cookie_change_handler(&mut self) -> Result<(), WebSurfaceError> {
        let mut slot = self
            .cookie_change_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("cookie_change_handler lock poisoned".into()))?;
        *slot = None;
        Ok(())
    }

    fn cookie_manager(&self) -> Result<ICoreWebView2CookieManager, WebSurfaceError> {
        let webview2: ICoreWebView2_2 = self
            .webview
            .cast()
            .map_err(platform("webview cast to ICoreWebView2_2"))?;
        unsafe { webview2.CookieManager() }.map_err(platform("webview.CookieManager"))
    }

    fn notify_cookie_changed(&self) {
        if let Ok(slot) = self.cookie_change_handler.lock()
            && let Some(handler) = slot.as_ref()
        {
            handler();
        }
    }
}

pub(super) fn install_cookie_change_bridge(webview: &ICoreWebView2) -> Result<(), WebSurfaceError> {
    let script = format!(
        r#"(() => {{
            if (window.__scryingCookieBridgeInstalled) return;
            Object.defineProperty(window, "__scryingCookieBridgeInstalled", {{ value: true }});
            const notify = () => {{
                try {{ window.chrome.webview.postMessage({message:?}); }} catch (_) {{}}
            }};
            let proto = Document.prototype;
            let descriptor = Object.getOwnPropertyDescriptor(proto, "cookie");
            if (!descriptor || !descriptor.configurable || !descriptor.get || !descriptor.set) return;
            Object.defineProperty(proto, "cookie", {{
                configurable: true,
                enumerable: descriptor.enumerable,
                get() {{ return descriptor.get.call(this); }},
                set(value) {{
                    const result = descriptor.set.call(this, value);
                    notify();
                    return result;
                }},
            }});
        }})()"#,
        message = COOKIE_CHANGE_BRIDGE_MESSAGE,
    );
    add_script_to_execute_on_document_created_blocking(webview, script)
}

unsafe fn webview2_cookie_string(
    cookie: &ICoreWebView2Cookie,
    read: unsafe fn(&ICoreWebView2Cookie, *mut PWSTR) -> windows::core::Result<()>,
) -> Result<String, WebSurfaceError> {
    let mut value = PWSTR::null();
    unsafe { read(cookie, &mut value) }.map_err(platform("ICoreWebView2Cookie string field"))?;
    Ok(unsafe { consume_pwstr(value) })
}

unsafe fn cookie_from_webview2(cookie: &ICoreWebView2Cookie) -> Result<Cookie, WebSurfaceError> {
    let name = unsafe { webview2_cookie_string(cookie, ICoreWebView2Cookie::Name)? };
    let value = unsafe { webview2_cookie_string(cookie, ICoreWebView2Cookie::Value)? };
    let domain = unsafe { webview2_cookie_string(cookie, ICoreWebView2Cookie::Domain)? };
    let path = unsafe { webview2_cookie_string(cookie, ICoreWebView2Cookie::Path)? };
    let mut expires = 0.0;
    unsafe { cookie.Expires(&mut expires) }.map_err(platform("ICoreWebView2Cookie.Expires"))?;
    let mut is_session = windows::core::BOOL::default();
    unsafe { cookie.IsSession(&mut is_session) }
        .map_err(platform("ICoreWebView2Cookie.IsSession"))?;
    let mut is_secure = windows::core::BOOL::default();
    unsafe { cookie.IsSecure(&mut is_secure) }.map_err(platform("ICoreWebView2Cookie.IsSecure"))?;
    let mut is_http_only = windows::core::BOOL::default();
    unsafe { cookie.IsHttpOnly(&mut is_http_only) }
        .map_err(platform("ICoreWebView2Cookie.IsHttpOnly"))?;
    Ok(Cookie {
        name,
        value,
        domain,
        path,
        expires_at: if is_session.as_bool() {
            None
        } else {
            Some(expires)
        },
        is_secure: is_secure.as_bool(),
        is_http_only: is_http_only.as_bool(),
        same_site: None,
        partitioned: false,
    })
}

unsafe fn cookies_from_webview2_list(
    list: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2CookieList,
) -> Result<Vec<Cookie>, WebSurfaceError> {
    let mut count = 0;
    unsafe { list.Count(&mut count) }.map_err(platform("ICoreWebView2CookieList.Count"))?;
    let mut cookies = Vec::with_capacity(count as usize);
    for index in 0..count {
        let cookie = unsafe { list.GetValueAtIndex(index) }
            .map_err(platform("ICoreWebView2CookieList.GetValueAtIndex"))?;
        cookies.push(unsafe { cookie_from_webview2(&cookie)? });
    }
    Ok(cookies)
}

unsafe fn webview2_cookie_from(
    manager: &ICoreWebView2CookieManager,
    cookie: &Cookie,
) -> Result<ICoreWebView2Cookie, WebSurfaceError> {
    if cookie.same_site.is_some() {
        return Err(WebSurfaceError::Unsupported(
            "WebView2 cookie manager path does not expose SameSite setters in scrying yet",
        ));
    }
    if cookie.partitioned {
        return Err(WebSurfaceError::Unsupported(
            "WebView2 cookie manager path does not expose Partitioned cookie setters in scrying yet",
        ));
    }
    let name = CoTaskMemPWSTR::from(cookie.name.as_str());
    let value = CoTaskMemPWSTR::from(cookie.value.as_str());
    let domain = CoTaskMemPWSTR::from(cookie.domain.as_str());
    let path = CoTaskMemPWSTR::from(cookie.path.as_str());
    let webview_cookie = unsafe {
        manager.CreateCookie(
            *name.as_ref().as_pcwstr(),
            *value.as_ref().as_pcwstr(),
            *domain.as_ref().as_pcwstr(),
            *path.as_ref().as_pcwstr(),
        )
    }
    .map_err(platform("CookieManager.CreateCookie"))?;
    unsafe { webview_cookie.SetIsSecure(cookie.is_secure) }
        .map_err(platform("ICoreWebView2Cookie.SetIsSecure"))?;
    unsafe { webview_cookie.SetIsHttpOnly(cookie.is_http_only) }
        .map_err(platform("ICoreWebView2Cookie.SetIsHttpOnly"))?;
    if let Some(expires_at) = cookie.expires_at {
        unsafe { webview_cookie.SetExpires(expires_at) }
            .map_err(platform("ICoreWebView2Cookie.SetExpires"))?;
    }
    Ok(webview_cookie)
}

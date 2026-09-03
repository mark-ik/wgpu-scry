// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

impl WebView2CompositionProducer {
    /// Route requests for `https://{host}/...` through a host-provided
    /// resource handler.
    ///
    /// WebView2 does not support arbitrary custom URL schemes the same way
    /// WebKit does, so Windows uses virtual HTTPS hosts. Browser-shape hosts
    /// can register stable app origins such as `mere.local` or
    /// `settings.internal` and serve bytes through the same
    /// [`UrlSchemeResponse`] shape macOS uses for `WKURLSchemeHandler`.
    pub fn register_virtual_host_handler(
        &mut self,
        host: impl Into<String>,
        handler: UrlSchemeHandlerFn,
    ) -> Result<(), WebSurfaceError> {
        let host = normalize_virtual_host(&host.into())?;
        self.ensure_web_resource_requested_handler()?;
        let was_new = {
            let mut handlers = self
                .resource_handlers
                .lock()
                .map_err(|_| WebSurfaceError::Platform("resource handler mutex poisoned".into()))?;
            handlers.insert(host.clone(), handler).is_none()
        };
        if was_new {
            let filter = format!("https://{host}/*");
            let filter = CoTaskMemPWSTR::from(filter.as_str());
            unsafe {
                self.webview
                    .AddWebResourceRequestedFilter(
                        *filter.as_ref().as_pcwstr(),
                        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                    )
                    .map_err(platform("AddWebResourceRequestedFilter"))?;
            }
        }
        Ok(())
    }

    fn ensure_web_resource_requested_handler(&mut self) -> Result<(), WebSurfaceError> {
        if self.web_resource_requested_token.is_some() {
            return Ok(());
        }

        let handlers = self.resource_handlers.clone();
        let environment = self.environment.clone();
        let handler = WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                let request = unsafe { args.Request()? };
                let mut uri = PWSTR::null();
                unsafe { request.Uri(&mut uri)? };
                let url = unsafe { consume_pwstr(uri) };
                let Some(host) = virtual_host_from_https_url(&url) else {
                    return Ok(());
                };
                let handler = handlers
                    .lock()
                    .ok()
                    .and_then(|handlers| handlers.get(&host).cloned());
                if let Some(handler) = handler {
                    let response = handler(&url);
                    let stream = stream_from_bytes(&response.body)?;
                    let headers = web_resource_headers(&response);
                    let reason = CoTaskMemPWSTR::from("OK");
                    let headers = CoTaskMemPWSTR::from(headers.as_str());
                    let web_response = unsafe {
                        environment.CreateWebResourceResponse(
                            &stream,
                            200,
                            *reason.as_ref().as_pcwstr(),
                            *headers.as_ref().as_pcwstr(),
                        )?
                    };
                    unsafe { args.SetResponse(&web_response)? };
                }
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe {
            self.webview
                .add_WebResourceRequested(&handler, &mut token)
                .map_err(platform("add_WebResourceRequested"))?;
        }
        self.web_resource_requested_token = Some(token);
        Ok(())
    }
}

fn normalize_virtual_host(host: &str) -> Result<String, WebSurfaceError> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host.contains('/')
        || host.contains(':')
        || host.contains('?')
        || host.contains('#')
    {
        return Err(WebSurfaceError::Platform(format!(
            "invalid virtual host name: {host:?}"
        )));
    }
    Ok(host)
}

fn virtual_host_from_https_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn web_resource_headers(response: &UrlSchemeResponse) -> String {
    let mut headers = String::new();
    push_web_resource_header(&mut headers, "Content-Type", &response.mime_type);
    push_web_resource_header(
        &mut headers,
        "Content-Length",
        &response.body.len().to_string(),
    );
    for (name, value) in &response.headers {
        push_web_resource_header(&mut headers, name, value);
    }
    headers
}

fn push_web_resource_header(headers: &mut String, name: &str, value: &str) {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return;
    }
    let value = value.replace(['\r', '\n'], " ");
    headers.push_str(name);
    headers.push_str(": ");
    headers.push_str(&value);
    headers.push_str("\r\n");
}

fn stream_from_bytes(bytes: &[u8]) -> windows::core::Result<IStream> {
    let stream: IStream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }?;
    if !bytes.is_empty() {
        let mut written = 0u32;
        unsafe {
            stream
                .Write(
                    bytes.as_ptr() as *const std::ffi::c_void,
                    bytes.len() as u32,
                    Some(&mut written),
                )
                .ok()?;
        }
        if written != bytes.len() as u32 {
            return Err(windows::core::Error::from(E_POINTER));
        }
    }
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None)? };
    Ok(stream)
}

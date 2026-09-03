// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

impl WebView2CompositionProducer {
    pub fn set_auth_handler(
        &mut self,
        handler: WebView2AuthHandlerFn,
    ) -> Result<(), WebSurfaceError> {
        let mut slot = self
            .auth_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("auth_handler lock poisoned".into()))?;
        *slot = Some(handler);
        Ok(())
    }

    pub fn clear_auth_handler(&mut self) -> Result<(), WebSurfaceError> {
        let mut slot = self
            .auth_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("auth_handler lock poisoned".into()))?;
        *slot = None;
        Ok(())
    }

    pub fn set_permission_handler(
        &mut self,
        handler: WebView2PermissionHandlerFn,
    ) -> Result<(), WebSurfaceError> {
        let mut slot = self
            .permission_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("permission_handler lock poisoned".into()))?;
        *slot = Some(handler);
        Ok(())
    }

    pub fn clear_permission_handler(&mut self) -> Result<(), WebSurfaceError> {
        let mut slot = self
            .permission_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("permission_handler lock poisoned".into()))?;
        *slot = None;
        Ok(())
    }
}

pub(super) fn register_basic_auth_handler(
    webview: &ICoreWebView2,
    nav_queue: Arc<Mutex<VecDeque<NavigationEvent>>>,
    host_handler: Arc<Mutex<Option<WebView2AuthHandlerFn>>>,
    download_registry: Arc<Mutex<WebView2DownloadRegistry>>,
) -> Result<i64, WebSurfaceError> {
    let webview10: ICoreWebView2_10 = webview
        .cast()
        .map_err(platform("webview cast to ICoreWebView2_10"))?;
    let handler = BasicAuthenticationRequestedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut uri = PWSTR::null();
        unsafe { args.Uri(&mut uri)? };
        let url = unsafe { consume_pwstr(uri) };
        let mut challenge = PWSTR::null();
        let realm = if unsafe { args.Challenge(&mut challenge) }.is_ok() {
            unsafe { consume_pwstr(challenge) }
        } else {
            String::new()
        };
        let auth_method = "WebView2BasicAuthentication".to_string();
        let host = origin_host_from_url(&url);
        let source = downloads::auth_source_for_webview2_basic_auth(&url, &download_registry);
        if let Ok(mut queue) = nav_queue.lock() {
            queue.push_back(NavigationEvent::AuthChallenged {
                url: url.clone(),
                host: host.clone(),
                auth_method: auth_method.clone(),
                source,
            });
        }
        let disposition = host_handler.lock().ok().and_then(|guard| {
            guard.as_ref().map(|handler| {
                handler(AuthChallenge {
                    url,
                    host,
                    auth_method,
                    realm,
                    source,
                })
            })
        });
        match disposition {
            Some(AuthDisposition::Cancel) | Some(AuthDisposition::RejectProtectionSpace) => {
                unsafe { args.SetCancel(true)? };
            }
            Some(AuthDisposition::UseCredential { username, password }) => {
                let response = unsafe { args.Response()? };
                let username = CoTaskMemPWSTR::from(username.as_str());
                let password = CoTaskMemPWSTR::from(password.as_str());
                unsafe {
                    response.SetUserName(*username.as_ref().as_pcwstr())?;
                    response.SetPassword(*password.as_ref().as_pcwstr())?;
                }
            }
            None | Some(AuthDisposition::PerformDefault) => {}
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe { webview10.add_BasicAuthenticationRequested(&handler, &mut token) }
        .map_err(platform("add_BasicAuthenticationRequested"))?;
    Ok(token)
}

pub(super) fn register_permission_requested_handler(
    webview: &ICoreWebView2,
    host_handler: Arc<Mutex<Option<WebView2PermissionHandlerFn>>>,
) -> Result<i64, WebSurfaceError> {
    let handler = PermissionRequestedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut uri = PWSTR::null();
        unsafe { args.Uri(&mut uri)? };
        let frame_url = unsafe { consume_pwstr(uri) };
        let mut kind = COREWEBVIEW2_PERMISSION_KIND(0);
        unsafe { args.PermissionKind(&mut kind)? };
        let Some(permission_kind) = permission_kind_from_webview2(kind) else {
            return Ok(());
        };
        let request = PermissionRequest {
            origin: origin_from_url(&frame_url),
            frame_url,
            kind: permission_kind,
        };
        let decision = host_handler
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|handler| handler(request)))
            .unwrap_or(PermissionDecision::Prompt);
        let state = permission_decision_to_webview2(decision);
        unsafe { args.SetState(state)? };
        if decision != PermissionDecision::Prompt
            && let Ok(args2) = args.cast::<ICoreWebView2PermissionRequestedEventArgs2>()
        {
            unsafe { args2.SetHandled(true)? };
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe { webview.add_PermissionRequested(&handler, &mut token) }
        .map_err(platform("add_PermissionRequested"))?;
    Ok(token)
}

fn permission_kind_from_webview2(kind: COREWEBVIEW2_PERMISSION_KIND) -> Option<PermissionKind> {
    match kind {
        COREWEBVIEW2_PERMISSION_KIND_CAMERA => Some(PermissionKind::Camera),
        COREWEBVIEW2_PERMISSION_KIND_MICROPHONE => Some(PermissionKind::Microphone),
        COREWEBVIEW2_PERMISSION_KIND_OTHER_SENSORS => Some(PermissionKind::DeviceOrientation),
        _ => None,
    }
}

fn permission_decision_to_webview2(decision: PermissionDecision) -> COREWEBVIEW2_PERMISSION_STATE {
    match decision {
        PermissionDecision::Grant => COREWEBVIEW2_PERMISSION_STATE_ALLOW,
        PermissionDecision::Deny => COREWEBVIEW2_PERMISSION_STATE_DENY,
        PermissionDecision::Prompt => COREWEBVIEW2_PERMISSION_STATE_DEFAULT,
    }
}

fn origin_host_from_url(url: &str) -> String {
    let Some(rest) = url.split_once("://").map(|(_, rest)| rest) else {
        return String::new();
    };
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn origin_from_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return String::new();
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        String::new()
    } else {
        format!("{scheme}://{host}")
    }
}

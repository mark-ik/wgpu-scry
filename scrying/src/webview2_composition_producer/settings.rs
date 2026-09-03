// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

impl WebView2CompositionProducer {
    /// Open the WebView2 DevTools window.
    pub fn open_devtools_window(&self) -> Result<(), WebSurfaceError> {
        unsafe { self.webview.OpenDevToolsWindow() }.map_err(platform("OpenDevToolsWindow"))
    }

    /// Toggle WebView2's page visibility / occlusion state. Browser-shape
    /// consumers call this as tabs become active or inactive.
    pub fn set_visible(&self, visible: bool) -> Result<(), WebSurfaceError> {
        unsafe { self.controller.SetIsVisible(visible) }
            .map_err(platform("controller.SetIsVisible"))
    }

    pub fn set_password_autosave_enabled(&self, enabled: bool) -> Result<(), WebSurfaceError> {
        let settings = self.settings4()?;
        unsafe { settings.SetIsPasswordAutosaveEnabled(enabled) }
            .map_err(platform("Settings4.SetIsPasswordAutosaveEnabled"))
    }

    pub fn set_general_autofill_enabled(&self, enabled: bool) -> Result<(), WebSurfaceError> {
        let settings = self.settings4()?;
        unsafe { settings.SetIsGeneralAutofillEnabled(enabled) }
            .map_err(platform("Settings4.SetIsGeneralAutofillEnabled"))
    }

    /// Apply a partial settings update. `None` fields are left at their
    /// current value.
    pub fn apply_settings(
        &self,
        settings: &crate::WebSurfaceSettings,
    ) -> Result<(), WebSurfaceError> {
        if settings.inactive_scheduling_policy.is_some() {
            return Err(WebSurfaceError::Unsupported(
                "WebView2 exposes SetIsVisible for Page Visibility, but no public inactive-scheduling / hard-pause policy equivalent",
            ));
        }
        if let Some(zoom) = settings.zoom_factor {
            unsafe { self.controller.SetZoomFactor(zoom) }
                .map_err(platform("controller.SetZoomFactor"))?;
        }
        let webview_settings: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings =
            unsafe { self.webview.Settings() }.map_err(platform("webview.Settings"))?;
        if let Some(enabled) = settings.javascript_enabled {
            unsafe { webview_settings.SetIsScriptEnabled(enabled) }
                .map_err(platform("Settings.SetIsScriptEnabled"))?;
        }
        if let Some(enabled) = settings.devtools_enabled {
            unsafe { webview_settings.SetAreDevToolsEnabled(enabled) }
                .map_err(platform("Settings.SetAreDevToolsEnabled"))?;
        }
        if let Some(enabled) = settings.default_context_menus_enabled {
            if let Ok(mut slot) = self.default_context_menus_enabled.lock() {
                *slot = enabled;
            }
            unsafe { webview_settings.SetAreDefaultContextMenusEnabled(enabled) }
                .map_err(platform("Settings.SetAreDefaultContextMenusEnabled"))?;
        }
        if let Some(enabled) = settings.builtin_accelerator_keys_enabled {
            let settings3: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3 =
                webview_settings
                    .cast()
                    .map_err(platform("Settings cast to ICoreWebView2Settings3"))?;
            unsafe { settings3.SetAreBrowserAcceleratorKeysEnabled(enabled) }
                .map_err(platform("Settings3.SetAreBrowserAcceleratorKeysEnabled"))?;
        }
        if let Some(ref ua) = settings.user_agent {
            let settings2: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings2 =
                webview_settings
                    .cast()
                    .map_err(platform("Settings cast to ICoreWebView2Settings2"))?;
            let ua = CoTaskMemPWSTR::from(ua.as_str());
            unsafe { settings2.SetUserAgent(*ua.as_ref().as_pcwstr()) }
                .map_err(platform("Settings2.SetUserAgent"))?;
        }
        Ok(())
    }

    fn settings4(
        &self,
    ) -> Result<
        webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings4,
        WebSurfaceError,
    > {
        let settings: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings =
            unsafe { self.webview.Settings() }.map_err(platform("webview.Settings"))?;
        settings.cast().map_err(|_| {
            WebSurfaceError::Unsupported(
                "WebView2 runtime does not expose ICoreWebView2Settings4 autofill controls",
            )
        })
    }
}

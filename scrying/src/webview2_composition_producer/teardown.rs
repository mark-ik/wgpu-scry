// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

impl Drop for WebView2CompositionProducer {
    fn drop(&mut self) {
        if let Some(state) = self.capture_state.take() {
            let _ = state.pool.RemoveFrameArrived(state.frame_arrived_token);
            let _ = state.session.Close();
            let _ = state.pool.Close();
            let _ = state;
        }
        unsafe {
            let _ = self
                .webview
                .remove_NavigationStarting(self.nav_starting_token);
            let _ = self
                .webview
                .remove_NavigationCompleted(self.nav_completed_token);
            let _ = self.webview.remove_SourceChanged(self.source_changed_token);
            let _ = self
                .webview
                .remove_DocumentTitleChanged(self.title_changed_token);
            let _ = self
                .webview
                .remove_NewWindowRequested(self.new_window_requested_token);
            let _ = self.webview.remove_ProcessFailed(self.process_failed_token);
            if let Ok(webview4) = self.webview.cast::<ICoreWebView2_4>() {
                let _ = webview4.remove_DownloadStarting(self.download_starting_token);
            }
            if let Ok(webview10) = self.webview.cast::<ICoreWebView2_10>() {
                let _ = webview10.remove_BasicAuthenticationRequested(self.basic_auth_token);
            }
            let _ = self
                .webview
                .remove_PermissionRequested(self.permission_requested_token);
            if let Some(token) = self.web_resource_requested_token {
                let _ = self.webview.remove_WebResourceRequested(token);
            }
            let _ = self
                .webview
                .remove_WebMessageReceived(self.web_message_token);
            if let Ok(webview2) = self.webview.cast::<ICoreWebView2_2>() {
                let _ = webview2
                    .remove_WebResourceResponseReceived(self.web_resource_response_received_token);
            }
            if let Ok(webview11) = self.webview.cast::<ICoreWebView2_11>() {
                let _ = webview11.remove_ContextMenuRequested(self.context_menu_requested_token);
            }
            let _ = self
                .controller
                .remove_AcceleratorKeyPressed(self.accelerator_key_pressed_token);
            let _ = self
                .composition_controller
                .remove_CursorChanged(self.cursor_changed_token);
            if let Ok(mut registry) = self.download_registry.lock() {
                for (_, entry) in registry.by_id.drain() {
                    let _ = entry
                        .operation
                        .remove_BytesReceivedChanged(entry.bytes_received_token);
                    let _ = entry
                        .operation
                        .remove_StateChanged(entry.state_changed_token);
                }
            }
            let _ = self.controller.Close();
        }
        // Detach this pane from the shared root so a dropped producer doesn't
        // leave a stale visual behind for the other panes on the same HWND.
        if let Ok(children) = self.composition_root.root_visual.Children() {
            let _ = children.Remove(&self.pane_container);
        }
    }
}

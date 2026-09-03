// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

impl WebView2CompositionProducer {
    pub fn find_in_page(
        &self,
        query: &str,
        options: WebView2FindOptions,
    ) -> Result<(), WebSurfaceError> {
        if let Ok(mut slot) = self.pending_find.lock() {
            *slot = None;
        }
        let environment15 = self
            .environment
            .cast::<ICoreWebView2Environment15>()
            .map_err(platform("environment.cast<ICoreWebView2Environment15>"))?;
        let find_options = unsafe { environment15.CreateFindOptions() }
            .map_err(platform("Environment15.CreateFindOptions"))?;
        let query = CoTaskMemPWSTR::from(query);
        unsafe {
            find_options
                .SetFindTerm(*query.as_ref().as_pcwstr())
                .map_err(platform("FindOptions.SetFindTerm"))?;
            find_options
                .SetIsCaseSensitive(options.case_sensitive)
                .map_err(platform("FindOptions.SetIsCaseSensitive"))?;
            find_options
                .SetShouldHighlightAllMatches(options.highlight_all_matches)
                .map_err(platform("FindOptions.SetShouldHighlightAllMatches"))?;
            find_options
                .SetShouldMatchWord(options.match_word)
                .map_err(platform("FindOptions.SetShouldMatchWord"))?;
            find_options
                .SetSuppressDefaultFindDialog(options.suppress_default_find_dialog)
                .map_err(platform("FindOptions.SetSuppressDefaultFindDialog"))?;
        }

        let webview28 = self
            .webview
            .cast::<ICoreWebView2_28>()
            .map_err(platform("webview.cast<ICoreWebView2_28>"))?;
        let find = unsafe { webview28.Find() }.map_err(platform("WebView2_28.Find"))?;
        let pending = self.pending_find.clone();
        let find_for_completion = find.clone();
        let handler =
            FindStartCompletedHandler::create(Box::new(move |result: windows::core::Result<()>| {
                let next = result
                    .map_err(|err| err.message().to_string())
                    .and_then(|()| unsafe {
                        let mut match_count = 0i32;
                        let mut active_match_index = 0i32;
                        find_for_completion
                            .MatchCount(&mut match_count)
                            .map_err(|err| err.message().to_string())?;
                        find_for_completion
                            .ActiveMatchIndex(&mut active_match_index)
                            .map_err(|err| err.message().to_string())?;
                        Ok(WebView2FindResult {
                            matched: match_count > 0,
                            active_match_index,
                            match_count,
                        })
                    });
                if let Ok(mut slot) = pending.lock() {
                    *slot = Some(next);
                }
                Ok(())
            }));
        unsafe { find.Start(&find_options, &handler) }.map_err(platform("Find.Start"))?;
        if options.backwards {
            unsafe { find.FindPrevious() }.map_err(platform("Find.FindPrevious"))?;
        }
        Ok(())
    }

    pub fn poll_find_match(&self) -> Option<Result<WebView2FindResult, String>> {
        self.pending_find
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    pub fn stop_find(&self) -> Result<(), WebSurfaceError> {
        let webview28 = self
            .webview
            .cast::<ICoreWebView2_28>()
            .map_err(platform("webview.cast<ICoreWebView2_28>"))?;
        let find = unsafe { webview28.Find() }.map_err(platform("WebView2_28.Find"))?;
        unsafe { find.Stop() }.map_err(platform("Find.Stop"))
    }

    pub fn request_pdf(&self) -> Result<(), WebSurfaceError> {
        if let Ok(mut slot) = self.pending_pdf.lock() {
            *slot = None;
        }
        let environment6 = self
            .environment
            .cast::<ICoreWebView2Environment6>()
            .map_err(platform("environment.cast<ICoreWebView2Environment6>"))?;
        let print_settings = unsafe { environment6.CreatePrintSettings() }
            .map_err(platform("Environment6.CreatePrintSettings"))?;
        let webview16 = self
            .webview
            .cast::<ICoreWebView2_16>()
            .map_err(platform("webview.cast<ICoreWebView2_16>"))?;
        let pending = self.pending_pdf.clone();
        let handler = PrintToPdfStreamCompletedHandler::create(Box::new(
            move |result: windows::core::Result<()>, stream: Option<IStream>| {
                let next = result
                    .map_err(|err| err.message().to_string())
                    .and_then(|()| {
                        let stream = stream
                            .ok_or_else(|| "PrintToPdfStream returned no stream".to_string())?;
                        stream_to_bytes(&stream).map_err(|err| err.message().to_string())
                    });
                if let Ok(mut slot) = pending.lock() {
                    *slot = Some(next);
                }
                Ok(())
            },
        ));
        unsafe { webview16.PrintToPdfStream(&print_settings, &handler) }
            .map_err(platform("WebView2_16.PrintToPdfStream"))
    }

    pub fn poll_pdf(&self) -> Option<Result<Vec<u8>, String>> {
        self.pending_pdf
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    pub fn print(&self) -> Result<bool, WebSurfaceError> {
        let webview16 = self
            .webview
            .cast::<ICoreWebView2_16>()
            .map_err(platform("webview.cast<ICoreWebView2_16>"))?;
        unsafe { webview16.ShowPrintUI(COREWEBVIEW2_PRINT_DIALOG_KIND_BROWSER) }
            .map_err(platform("WebView2_16.ShowPrintUI"))?;
        Ok(true)
    }
}

pub(super) fn install_context_menu_bridge(webview: &ICoreWebView2) -> Result<(), WebSurfaceError> {
    let script = format!(
        r##"(() => {{
            if (window.__scryingContextMenuBridgeInstalled) return;
            Object.defineProperty(window, "__scryingContextMenuBridgeInstalled", {{ value: true }});
            const prefix = {prefix:?};
            const clean = value => String(value || "").replace(/[\t\r\n]/g, " ");
            const closest = (node, selector) => {{
                for (let current = node; current && current !== document; current = current.parentElement) {{
                    if (current.matches && current.matches(selector)) return current;
                }}
                return null;
            }};
            window.addEventListener("contextmenu", event => {{
                const target = event.target;
                const link = closest(target, "a[href]");
                const image = closest(target, "img[src]");
                const payload = [
                    clean(location.href),
                    Math.round(event.clientX),
                    Math.round(event.clientY),
                    clean(link && link.href),
                    clean(image && image.src),
                ].join("\t");
                try {{ window.chrome.webview.postMessage(prefix + payload); }} catch (_) {{}}
            }}, true);
        }})()"##,
        prefix = CONTEXT_MENU_BRIDGE_PREFIX,
    );
    add_script_to_execute_on_document_created_blocking(webview, script)
}

pub(super) fn install_drop_detected_bridge(webview: &ICoreWebView2) -> Result<(), WebSurfaceError> {
    let script = format!(
        r##"(() => {{
            if (window.__scryingDropBridgeInstalled) return;
            Object.defineProperty(window, "__scryingDropBridgeInstalled", {{ value: true }});
            const prefix = {prefix:?};
            const clean = value => String(value || "").replace(/[\t\r\n]/g, " ");
            window.addEventListener("drop", event => {{
                const dt = event.dataTransfer;
                if (!dt) return;
                const types = Array.from(dt.types || []);
                const hasFiles = dt.files && dt.files.length > 0;
                const hasUri = types.includes("text/uri-list") || !!dt.getData("text/uri-list");
                const hasImage = types.some(type => String(type).toLowerCase().startsWith("image/"));
                if (!hasFiles && !hasUri && !hasImage) return;
                let primaryUrl = dt.getData("text/uri-list") || "";
                if (primaryUrl.includes("\n")) primaryUrl = primaryUrl.split(/\r?\n/).find(line => line && !line.startsWith("#")) || "";
                if (!primaryUrl) primaryUrl = dt.getData("text/plain") || "";
                const payload = [
                    Math.round(event.clientX),
                    Math.round(event.clientY),
                    dt.files ? dt.files.length : 0,
                    clean(primaryUrl),
                ].join("\t");
                try {{ window.chrome.webview.postMessage(prefix + payload); }} catch (_) {{}}
            }}, true);
        }})()"##,
        prefix = DROP_DETECTED_BRIDGE_PREFIX,
    );
    add_script_to_execute_on_document_created_blocking(webview, script)
}

pub(super) fn install_media_capture_bridge(webview: &ICoreWebView2) -> Result<(), WebSurfaceError> {
    let script = format!(
        r#"(() => {{
            if (window.__scryingMediaCaptureBridgeInstalled) return;
            Object.defineProperty(window, "__scryingMediaCaptureBridgeInstalled", {{ value: true }});
            const prefix = {prefix:?};
            const tracks = new Set();
            const publish = () => {{
                let audio = 0;
                let video = 0;
                for (const track of Array.from(tracks)) {{
                    if (!track || track.readyState === "ended") {{ tracks.delete(track); continue; }}
                    if (track.kind === "audio") audio += 1;
                    if (track.kind === "video") video += 1;
                }}
                try {{ window.chrome.webview.postMessage(`${{prefix}}audio:${{audio}},video:${{video}}`); }} catch (_) {{}}
            }};
            const attach = stream => {{
                if (!stream || !stream.getTracks) return stream;
                for (const track of stream.getTracks()) {{
                    tracks.add(track);
                    track.addEventListener("ended", publish, {{ once: true }});
                }}
                publish();
                return stream;
            }};
            if (navigator.mediaDevices && navigator.mediaDevices.getUserMedia) {{
                const originalGetUserMedia = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
                navigator.mediaDevices.getUserMedia = async constraints => attach(await originalGetUserMedia(constraints));
            }}
        }})()"#,
        prefix = MEDIA_CAPTURE_BRIDGE_PREFIX,
    );
    add_script_to_execute_on_document_created_blocking(webview, script)
}

pub(super) fn install_text_input_bridge(webview: &ICoreWebView2) -> Result<(), WebSurfaceError> {
    let script = format!(
        r##"(() => {{
            if (window.__scryingTextInputBridgeInstalled) return;
            Object.defineProperty(window, "__scryingTextInputBridgeInstalled", {{ value: true }});
            const prefix = {prefix:?};
            let active = null;
            let lastPayload = "";
            let scheduled = false;
            const emit = body => {{
                // Always emit focus transitions: the host needs them to set up
                // IME state even if the per-element payload happens to repeat.
                // Dedup the noisy cases: consecutive blurs (password keeps
                // re-publishing) and identical change pulses (selectionchange
                // / scroll storms with no actual movement).
                if (body === "blurred" && lastPayload === "blurred") return;
                if (body.startsWith("changed\t") && body === lastPayload) return;
                lastPayload = body;
                try {{ window.chrome.webview.postMessage(prefix + body); }} catch (_) {{}}
            }};
            const clean = value => String(value || "").replace(/[\t\r\n]/g, " ");
            const editable = element => {{
                if (!element || element.nodeType !== Node.ELEMENT_NODE) return null;
                const tag = element.tagName ? element.tagName.toLowerCase() : "";
                if (tag === "textarea") return element;
                if (tag === "input") {{
                    const type = (element.getAttribute("type") || "text").toLowerCase();
                    if (!["button", "checkbox", "color", "file", "hidden", "image", "radio", "range", "reset", "submit"].includes(type)) return element;
                }}
                for (let current = element; current && current !== document.documentElement; current = current.parentElement) {{
                    if (current.isContentEditable) return current;
                }}
                return null;
            }};
            const textInputKind = element => {{
                const tag = element.tagName ? element.tagName.toLowerCase() : "";
                if (tag === "textarea") return "textarea";
                if (tag === "input") return "input";
                return "contenteditable";
            }};
            const inputType = element => {{
                const tag = element && element.tagName ? element.tagName.toLowerCase() : "";
                return tag === "input" ? (element.getAttribute("type") || "text").toLowerCase() : "";
            }};
            const autocomplete = element => String(element.getAttribute("autocomplete") || "").toLowerCase();
            const isPasswordLike = element => {{
                const type = inputType(element);
                const complete = autocomplete(element);
                return type === "password" || complete.includes("password") || complete === "one-time-code";
            }};
            const selectionOffsets = element => {{
                if (typeof element.selectionStart === "number" && typeof element.selectionEnd === "number") {{
                    return [element.selectionStart, element.selectionEnd];
                }}
                const selection = window.getSelection && window.getSelection();
                if (!selection || selection.rangeCount === 0) return [0, 0];
                return [0, selection.focusOffset || 0];
            }};
            const caretRectForContentEditable = element => {{
                const selection = window.getSelection && window.getSelection();
                if (!selection || selection.rangeCount === 0) return element.getBoundingClientRect();
                const range = selection.getRangeAt(0).cloneRange();
                range.collapse(false);
                const rect = range.getClientRects()[0] || range.getBoundingClientRect();
                if (rect && (rect.width || rect.height)) return rect;
                return element.getBoundingClientRect();
            }};
            const caretRectForTextControl = element => {{
                const rect = element.getBoundingClientRect();
                const style = getComputedStyle(element);
                const mirror = document.createElement("div");
                const props = [
                    "boxSizing", "width", "height", "overflowX", "overflowY",
                    "borderTopWidth", "borderRightWidth", "borderBottomWidth", "borderLeftWidth",
                    "paddingTop", "paddingRight", "paddingBottom", "paddingLeft",
                    "fontFamily", "fontSize", "fontStyle", "fontWeight", "lineHeight",
                    "letterSpacing", "textTransform", "textIndent", "textAlign",
                    "whiteSpace", "wordSpacing", "wordBreak", "overflowWrap"
                ];
                const tag = element.tagName.toLowerCase();
                const lineHeight = parseFloat(style.lineHeight) || Math.ceil((parseFloat(style.fontSize) || 16) * 1.2);
                mirror.style.position = "fixed";
                mirror.style.visibility = "hidden";
                mirror.style.left = `${{rect.left}}px`;
                mirror.style.top = `${{rect.top}}px`;
                mirror.style.minHeight = `${{rect.height}}px`;
                mirror.style.overflow = "hidden";
                mirror.style.whiteSpace = tag === "textarea" ? "pre-wrap" : "pre";
                mirror.style.overflowWrap = tag === "textarea" ? "break-word" : "normal";
                for (const prop of props) mirror.style[prop] = style[prop];
                const selectionEnd = typeof element.selectionEnd === "number" ? element.selectionEnd : 0;
                let before = (element.value || "").slice(0, selectionEnd);
                if (tag === "textarea" && before.endsWith("\n")) before += "\u200b";
                mirror.textContent = before;
                const marker = document.createElement("span");
                marker.textContent = "\u200b";
                mirror.appendChild(marker);
                document.body.appendChild(mirror);
                const markerRect = marker.getBoundingClientRect();
                document.body.removeChild(mirror);
                const x = markerRect.left - (element.scrollLeft || 0);
                const y = markerRect.top - (element.scrollTop || 0);
                return {{
                    left: Math.max(rect.left, Math.min(rect.right, x)),
                    top: Math.max(rect.top, Math.min(rect.bottom, y)),
                    width: Math.max(1, markerRect.width || 1),
                    height: Math.max(1, markerRect.height || lineHeight)
                }};
            }};
            const caretRect = element => {{
                try {{
                    return element.isContentEditable ? caretRectForContentEditable(element) : caretRectForTextControl(element);
                }} catch (_) {{
                    return element.getBoundingClientRect();
                }}
            }};
            const publish = reason => {{
                if (!active) active = editable(document.activeElement);
                if (!active) return;
                if (isPasswordLike(active)) {{
                    active = null;
                    emit("blurred");
                    return;
                }}
                const rect = caretRect(active);
                const [selectionStart, selectionEnd] = selectionOffsets(active);
                const type = inputType(active);
                const inputMode = active.getAttribute("inputmode") || "";
                const complete = autocomplete(active);
                const kind = textInputKind(active);
                const multiline = kind === "textarea" || kind === "contenteditable";
                emit([
                    reason,
                    clean(kind),
                    clean(type),
                    clean(inputMode),
                    clean(complete),
                    multiline ? "1" : "0",
                    "0",
                    Math.max(0, selectionStart | 0),
                    Math.max(0, selectionEnd | 0),
                    Math.round(rect.left * 1000) / 1000,
                    Math.round(rect.top * 1000) / 1000,
                    Math.round(Math.max(1, rect.width || 1) * 1000) / 1000,
                    Math.round(Math.max(1, rect.height || 1) * 1000) / 1000
                ].join("\t"));
            }};
            const schedule = reason => {{
                if (scheduled) return;
                scheduled = true;
                requestAnimationFrame(() => {{
                    scheduled = false;
                    publish(reason);
                }});
            }};
            document.addEventListener("focusin", event => {{
                const next = editable(event.target);
                if (!next) return;
                if (isPasswordLike(next)) {{
                    active = null;
                    emit("blurred");
                    return;
                }}
                active = next;
                publish("focused");
            }}, true);
            document.addEventListener("focusout", event => {{
                if (!active || event.target !== active) return;
                const next = editable(document.activeElement);
                if (next === active) return;
                active = null;
                emit("blurred");
            }}, true);
            for (const type of ["selectionchange", "input", "compositionstart", "compositionupdate", "compositionend", "scroll", "resize"]) {{
                window.addEventListener(type, () => schedule("changed"), true);
                document.addEventListener(type, () => schedule("changed"), true);
            }}
            try {{
                window.chrome.webview.addEventListener("message", () => schedule(active ? "changed" : "focused"));
            }} catch (_) {{}}
            if (editable(document.activeElement)) {{
                active = editable(document.activeElement);
                publish("focused");
            }}
        }})()"##,
        prefix = TEXT_INPUT_BRIDGE_PREFIX,
    );
    add_script_to_execute_on_document_created_blocking(webview, script)
}

pub(super) fn register_context_menu_requested_handler(
    webview: &ICoreWebView2,
    nav_queue: Arc<Mutex<VecDeque<NavigationEvent>>>,
    default_context_menus_enabled: Arc<Mutex<bool>>,
) -> Result<i64, WebSurfaceError> {
    let webview11 = webview
        .cast::<ICoreWebView2_11>()
        .map_err(platform("webview.cast<ICoreWebView2_11>"))?;
    let handler = ContextMenuRequestedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let mut point = POINT::default();
            let _ = unsafe { args.Location(&mut point) };
            let mut page_url = String::new();
            let mut link_url = None;
            let mut image_url = None;
            if let Ok(target) = unsafe { args.ContextMenuTarget() } {
                page_url = unsafe { read_pwstr_from(|out| target.PageUri(out)) };
                if unsafe { read_bool_from(|out| target.HasLinkUri(out)) } {
                    let uri = unsafe { read_pwstr_from(|out| target.LinkUri(out)) };
                    if !uri.is_empty() {
                        link_url = Some(uri);
                    }
                }
                if unsafe { read_bool_from(|out| target.HasSourceUri(out)) } {
                    let uri = unsafe { read_pwstr_from(|out| target.SourceUri(out)) };
                    if !uri.is_empty() {
                        image_url = Some(uri);
                    }
                }
            }
            let allow_default_menu = default_context_menus_enabled
                .lock()
                .map(|enabled| *enabled)
                .unwrap_or(false);
            unsafe { args.SetHandled(!allow_default_menu)? };
            if let Ok(mut q) = nav_queue.lock() {
                q.push_back(NavigationEvent::ContextMenuRequested {
                    page_url,
                    x: point.x as f64,
                    y: point.y as f64,
                    link_url,
                    image_url,
                });
            }
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe { webview11.add_ContextMenuRequested(&handler, &mut token) }
        .map_err(platform("add_ContextMenuRequested"))?;
    Ok(token)
}

pub(super) fn register_web_resource_response_received_handler(
    webview: &ICoreWebView2,
    cookie_change_handler: Arc<Mutex<Option<WebView2CookieChangeHandlerFn>>>,
) -> Result<i64, WebSurfaceError> {
    let webview2 = webview
        .cast::<ICoreWebView2_2>()
        .map_err(platform("webview.cast<ICoreWebView2_2>"))?;
    let handler = WebResourceResponseReceivedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args
            && let Ok(response) = unsafe { args.Response() }
            && let Ok(headers) = unsafe { response.Headers() }
        {
            if response_headers_have_set_cookie(&headers)
                && let Ok(slot) = cookie_change_handler.lock()
                && let Some(handler) = slot.as_ref()
            {
                handler();
            }
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe { webview2.add_WebResourceResponseReceived(&handler, &mut token) }
        .map_err(platform("add_WebResourceResponseReceived"))?;
    Ok(token)
}

fn response_headers_have_set_cookie(
    headers: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2HttpResponseHeaders,
) -> bool {
    let name = CoTaskMemPWSTR::from("Set-Cookie");
    let mut contains = windows::core::BOOL::default();
    if unsafe { headers.Contains(*name.as_ref().as_pcwstr(), &mut contains) }.is_ok()
        && contains.as_bool()
    {
        return true;
    }
    let Ok(iterator) = (unsafe { headers.GetIterator() }) else {
        return false;
    };
    loop {
        let mut has_current = windows::core::BOOL::default();
        if unsafe { iterator.HasCurrentHeader(&mut has_current) }.is_err() || !has_current.as_bool()
        {
            return false;
        }
        let mut header_name = PWSTR::null();
        let mut header_value = PWSTR::null();
        if unsafe { iterator.GetCurrentHeader(&mut header_name, &mut header_value) }.is_err() {
            return false;
        }
        let header_name = unsafe { consume_pwstr(header_name) };
        let _ = unsafe { consume_pwstr(header_value) };
        if header_name.eq_ignore_ascii_case("set-cookie") {
            return true;
        }
        let mut has_next = windows::core::BOOL::default();
        if unsafe { iterator.MoveNext(&mut has_next) }.is_err() || !has_next.as_bool() {
            return false;
        }
    }
}

pub(super) fn parse_media_capture_bridge_message(message: &str) -> Option<NavigationEvent> {
    let payload = message.strip_prefix(MEDIA_CAPTURE_BRIDGE_PREFIX)?;
    let mut audio_active_tracks = 0u32;
    let mut video_active_tracks = 0u32;
    for part in payload.split(',') {
        if let Some(value) = part.strip_prefix("audio:") {
            audio_active_tracks = value.parse().ok()?;
        } else if let Some(value) = part.strip_prefix("video:") {
            video_active_tracks = value.parse().ok()?;
        }
    }
    Some(NavigationEvent::MediaCaptureStateChanged {
        audio_active_tracks,
        video_active_tracks,
    })
}

pub(super) fn parse_text_input_bridge_message(message: &str) -> Option<NavigationEvent> {
    let payload = message.strip_prefix(TEXT_INPUT_BRIDGE_PREFIX)?;
    if payload == "blurred" {
        return Some(NavigationEvent::TextInputBlurred);
    }
    let mut parts = payload.splitn(13, '\t');
    let reason = parts.next()?;
    let state = TextInputState {
        element_kind: parts.next()?.to_string(),
        input_type: parts.next()?.to_string(),
        input_mode: parts.next()?.to_string(),
        autocomplete: parts.next()?.to_string(),
        is_multiline: parts.next()? == "1",
        is_password: parts.next()? == "1",
        selection_start: parts.next()?.parse().ok()?,
        selection_end: parts.next()?.parse().ok()?,
        caret_rect: TextInputRect {
            x: parts.next()?.parse().ok()?,
            y: parts.next()?.parse().ok()?,
            width: parts.next()?.parse().ok()?,
            height: parts.next()?.parse().ok()?,
        },
    };
    match reason {
        "focused" => Some(NavigationEvent::TextInputFocused { state }),
        "changed" => Some(NavigationEvent::TextInputChanged { state }),
        _ => None,
    }
}

pub(super) fn parse_context_menu_bridge_message(message: &str) -> Option<NavigationEvent> {
    let payload = message.strip_prefix(CONTEXT_MENU_BRIDGE_PREFIX)?;
    let mut parts = payload.splitn(5, '\t');
    let page_url = parts.next()?.to_string();
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let link_url = optional_bridge_string(parts.next()?);
    let image_url = optional_bridge_string(parts.next().unwrap_or_default());
    Some(NavigationEvent::ContextMenuRequested {
        page_url,
        x,
        y,
        link_url,
        image_url,
    })
}

pub(super) fn parse_drop_detected_bridge_message(message: &str) -> Option<NavigationEvent> {
    let payload = message.strip_prefix(DROP_DETECTED_BRIDGE_PREFIX)?;
    let mut parts = payload.splitn(4, '\t');
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let file_count = parts.next()?.parse().ok()?;
    let primary_url = optional_bridge_string(parts.next().unwrap_or_default());
    Some(NavigationEvent::DropDetected {
        x,
        y,
        file_count,
        primary_url,
    })
}

fn optional_bridge_string(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InputPurpose, NavigationEvent, TextInputState};

    fn focused_payload(
        kind: &str,
        input_type: &str,
        input_mode: &str,
        autocomplete: &str,
    ) -> String {
        format!(
            "{prefix}focused\t{kind}\t{input_type}\t{input_mode}\t{autocomplete}\t0\t0\t0\t0\t12.5\t34.0\t100.0\t18.0",
            prefix = TEXT_INPUT_BRIDGE_PREFIX,
        )
    }

    fn parse_focus(payload: &str) -> TextInputState {
        match parse_text_input_bridge_message(payload).expect("parsed event") {
            NavigationEvent::TextInputFocused { state }
            | NavigationEvent::TextInputChanged { state } => state,
            other => panic!("expected text-input state, got {other:?}"),
        }
    }

    #[test]
    fn parses_focused_state_with_autocomplete() {
        let payload = focused_payload("input", "email", "", "username");
        let state = parse_focus(&payload);
        assert_eq!(state.element_kind, "input");
        assert_eq!(state.input_type, "email");
        assert_eq!(state.autocomplete, "username");
        assert!(!state.is_multiline);
        assert!(!state.is_password);
        assert_eq!(state.caret_rect.width, 100.0);
        assert_eq!(state.caret_rect.height, 18.0);
    }

    #[test]
    fn parses_blurred_event() {
        let payload = format!("{prefix}blurred", prefix = TEXT_INPUT_BRIDGE_PREFIX);
        let event = parse_text_input_bridge_message(&payload).expect("parsed event");
        assert!(matches!(event, NavigationEvent::TextInputBlurred));
    }

    #[test]
    fn rejects_unknown_reason() {
        let payload = format!(
            "{prefix}weird\tinput\ttext\t\t\t0\t0\t0\t0\t0\t0\t1\t1",
            prefix = TEXT_INPUT_BRIDGE_PREFIX,
        );
        assert!(parse_text_input_bridge_message(&payload).is_none());
    }

    #[test]
    fn purpose_prefers_inputmode_over_type() {
        let state = parse_focus(&focused_payload("input", "text", "email", ""));
        assert_eq!(state.purpose(), InputPurpose::Email);

        let state = parse_focus(&focused_payload("input", "email", "tel", ""));
        assert_eq!(state.purpose(), InputPurpose::Tel);
    }

    #[test]
    fn purpose_falls_back_to_input_type() {
        for (input_type, expected) in [
            ("text", InputPurpose::Text),
            ("search", InputPurpose::Search),
            ("email", InputPurpose::Email),
            ("url", InputPurpose::Url),
            ("tel", InputPurpose::Tel),
            ("number", InputPurpose::Decimal),
        ] {
            let state = parse_focus(&focused_payload("input", input_type, "", ""));
            assert_eq!(state.purpose(), expected, "input type {input_type}");
        }
    }

    #[test]
    fn purpose_inputmode_numeric_and_decimal_split() {
        let numeric = parse_focus(&focused_payload("input", "text", "numeric", ""));
        assert_eq!(numeric.purpose(), InputPurpose::Numeric);

        let decimal = parse_focus(&focused_payload("input", "text", "decimal", ""));
        assert_eq!(decimal.purpose(), InputPurpose::Decimal);
    }

    #[test]
    fn purpose_inputmode_none_disables_ime() {
        let state = parse_focus(&focused_payload("input", "text", "none", ""));
        assert_eq!(state.purpose(), InputPurpose::Disabled);
    }

    #[test]
    fn purpose_one_time_code_autocomplete() {
        let state = parse_focus(&focused_payload("input", "text", "", "one-time-code"));
        assert_eq!(state.purpose(), InputPurpose::OneTimeCode);
    }

    #[test]
    fn purpose_password_autocomplete_wins_over_type() {
        let state = parse_focus(&focused_payload(
            "input",
            "text",
            "email",
            "current-password",
        ));
        assert_eq!(state.purpose(), InputPurpose::Password);
    }

    #[test]
    fn purpose_defaults_to_text_for_textarea_and_contenteditable() {
        let textarea = parse_focus(&focused_payload("textarea", "", "", ""));
        assert_eq!(textarea.purpose(), InputPurpose::Text);

        let contenteditable = parse_focus(&focused_payload("contenteditable", "", "", ""));
        assert_eq!(contenteditable.purpose(), InputPurpose::Text);
    }
}

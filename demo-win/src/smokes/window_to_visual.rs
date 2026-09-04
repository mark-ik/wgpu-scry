// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use std::collections::HashSet;
use std::sync::mpsc;

use super::super::*;
use super::input::send_system_text;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC, ICoreWebView2, ICoreWebView2Controller,
    ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
    ICoreWebView2WebMessageReceivedEventArgs,
};
use webview2_com::{
    CoTaskMemPWSTR, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, WebMessageReceivedEventHandler,
};
use windows::Win32::Foundation::{E_POINTER, HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, IsWindowVisible, SetForegroundWindow,
};
use windows::core::{PCWSTR, PWSTR};

const WINDOW_TO_VISUAL_HTML: &str = r#"<!doctype html>
<html>
<head>
    <meta charset="utf-8">
    <style>
        html, body {
            margin: 0;
            width: 100%;
            height: 100%;
            background: #10252d;
            color: #f7e7c6;
            font-family: system-ui, sans-serif;
        }
        body {
            display: grid;
            place-items: center;
        }
        main {
            display: grid;
            gap: 10px;
            text-align: center;
        }
        input {
            width: 240px;
            box-sizing: border-box;
            border: 1px solid #8ccfc4;
            border-radius: 4px;
            background: #0d1720;
            color: #f7e7c6;
            padding: 7px 9px;
            font: 15px system-ui, sans-serif;
            letter-spacing: 0;
        }
    </style>
</head>
<body>
    <main>
        <strong>Window-to-Visual Probe</strong>
        <input id="keyboard-smoke" autofocus autocomplete="off" spellcheck="false" aria-label="keyboard smoke input">
    </main>
    <script>
        const input = document.getElementById("keyboard-smoke");
        const post = value => window.chrome.webview.postMessage(value);
        input.focus();
        post("window-to-visual:ready");
        input.addEventListener("input", () => post("window-to-visual:keyboard:" + input.value));
        window.chrome.webview.addEventListener("message", event => {
            if (event.data === "window-to-visual:focus") {
                input.focus();
                post("window-to-visual:focused:" + document.activeElement.id);
            }
        });
    </script>
</body>
</html>"#;

const WINDOW_TO_VISUAL_MULTI_PAGE_COUNT: usize = 3;
const WINDOW_TO_VISUAL_MULTI_CAPTURE_SAMPLES: usize = 5;
const WINDOW_TO_VISUAL_MULTI_AVG_CAPTURE_IMPORT_LIMIT_MS: f64 = 250.0;
const WINDOW_TO_VISUAL_MULTI_MAX_CAPTURE_IMPORT_LIMIT_MS: f64 = 750.0;

pub(crate) fn validate_platform_window_to_visual(
    window: &Window,
    host: &HostWgpuContext,
) -> Result<(), Box<dyn std::error::Error>> {
    const EXPECTED: &str = "scry42";

    let parent_hwnd = HWND(hwnd_from_window(window)?);
    let _hosting_guard = ForcedHostingModeGuard::window_to_visual();

    let user_data_dir = std::env::temp_dir().join("demo-win-window-to-visual-webview2");
    let environment = create_environment(&user_data_dir)?;
    let controller = create_controller(&environment, parent_hwnd)?;
    unsafe {
        controller.SetBounds(RECT {
            left: 40,
            top: 40,
            right: 460,
            bottom: 300,
        })?;
        controller.SetIsVisible(true)?;
    }
    let webview = unsafe { controller.CoreWebView2()? };

    let (message_tx, message_rx) = mpsc::channel::<String>();
    let mut web_message_token = 0;
    unsafe {
        webview.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(
                move |_sender: Option<ICoreWebView2>,
                      args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
                    if let Some(args) = args {
                        let mut raw = PWSTR::null();
                        if args.TryGetWebMessageAsString(&mut raw).is_ok() {
                            let message = CoTaskMemPWSTR::from(raw).to_string();
                            let _ = message_tx.send(message);
                        }
                    }
                    Ok(())
                },
            )),
            &mut web_message_token,
        )?;
    }

    let html = CoTaskMemPWSTR::from(WINDOW_TO_VISUAL_HTML);
    unsafe {
        webview.NavigateToString(*html.as_ref().as_pcwstr())?;
    }
    wait_for_message(
        &message_rx,
        "window-to-visual:ready",
        std::time::Duration::from_secs(5),
    )?;
    let visible_child_count = report_child_windows("window-to-visual", parent_hwnd);
    let focus_message = CoTaskMemPWSTR::from("window-to-visual:focus");
    unsafe {
        webview.PostWebMessageAsString(*focus_message.as_ref().as_pcwstr())?;
    }
    wait_for_message(
        &message_rx,
        "window-to-visual:focused:keyboard-smoke",
        std::time::Duration::from_secs(2),
    )?;

    unsafe {
        controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC)?;
        let _ = SetForegroundWindow(parent_hwnd);
    }
    pump_windows_messages_for(std::time::Duration::from_millis(150));
    send_system_text(EXPECTED)?;

    wait_for_keyboard_value(&message_rx, EXPECTED, std::time::Duration::from_secs(3))?;
    println!("demo-win: window-to-visual: keyboard PASS - DOM input received {EXPECTED:?}");

    let sample = import_parent_capture(parent_hwnd, host)?;
    println!(
        "demo-win: window-to-visual: capture/import took {:.2}ms",
        sample.elapsed.as_secs_f64() * 1000.0
    );
    println!("demo-win: window-to-visual: capture PASS - parent HWND captured and imported");

    unsafe {
        webview.remove_WebMessageReceived(web_message_token)?;
        controller.Close()?;
    }
    if visible_child_count > 0 {
        return Err(format!(
            "window-to-visual exposed {visible_child_count} visible WebView child HWND(s), rejecting as a no-overlay composition target"
        )
        .into());
    }
    println!("demo-win: window-to-visual: PASS");
    Ok(())
}

pub(crate) fn validate_platform_window_to_visual_multi(
    window: &Window,
    host: &HostWgpuContext,
) -> Result<(), Box<dyn std::error::Error>> {
    let parent_hwnd = HWND(hwnd_from_window(window)?);
    let _hosting_guard = ForcedHostingModeGuard::window_to_visual();

    let user_data_dir = std::env::temp_dir().join("demo-win-window-to-visual-multi-webview2");
    let environment = create_environment(&user_data_dir)?;
    let (message_tx, message_rx) = mpsc::channel::<String>();
    let mut pages = Vec::new();

    for index in 0..WINDOW_TO_VISUAL_MULTI_PAGE_COUNT {
        let controller = create_controller(&environment, parent_hwnd)?;
        unsafe {
            controller.SetBounds(multi_page_bounds(index))?;
            controller.SetIsVisible(true)?;
        }
        let webview = unsafe { controller.CoreWebView2()? };
        let page_tx = message_tx.clone();
        let mut web_message_token = 0;
        unsafe {
            webview.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(
                    move |_sender: Option<ICoreWebView2>,
                          args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
                        if let Some(args) = args {
                            let mut raw = PWSTR::null();
                            if args.TryGetWebMessageAsString(&mut raw).is_ok() {
                                let message = CoTaskMemPWSTR::from(raw).to_string();
                                let _ = page_tx.send(message);
                            }
                        }
                        Ok(())
                    },
                )),
                &mut web_message_token,
            )?;
        }

        let html = CoTaskMemPWSTR::from(window_to_visual_multi_html(index).as_str());
        unsafe {
            webview.NavigateToString(*html.as_ref().as_pcwstr())?;
        }
        pages.push(WindowToVisualPage {
            controller,
            webview,
            web_message_token,
        });
    }

    let ready_messages = (0..WINDOW_TO_VISUAL_MULTI_PAGE_COUNT)
        .map(|index| format!("window-to-visual-multi:{index}:ready"))
        .collect::<Vec<_>>();
    wait_for_all_messages(
        &message_rx,
        &ready_messages,
        std::time::Duration::from_secs(8),
    )?;
    let frame_messages = (0..WINDOW_TO_VISUAL_MULTI_PAGE_COUNT)
        .map(|index| format!("window-to-visual-multi:{index}:frame:60"))
        .collect::<Vec<_>>();
    wait_for_all_messages(
        &message_rx,
        &frame_messages,
        std::time::Duration::from_secs(5),
    )?;

    let visible_child_count = report_child_windows("window-to-visual-multi", parent_hwnd);

    pump_windows_messages_for(std::time::Duration::from_millis(250));
    let mut samples = Vec::with_capacity(WINDOW_TO_VISUAL_MULTI_CAPTURE_SAMPLES);
    for _ in 0..WINDOW_TO_VISUAL_MULTI_CAPTURE_SAMPLES {
        samples.push(import_parent_capture(parent_hwnd, host)?);
        pump_windows_messages_for(std::time::Duration::from_millis(16));
    }
    let avg_ms = samples
        .iter()
        .map(|sample| sample.elapsed.as_secs_f64() * 1000.0)
        .sum::<f64>()
        / samples.len() as f64;
    let max_ms = samples
        .iter()
        .map(|sample| sample.elapsed.as_secs_f64() * 1000.0)
        .fold(0.0, f64::max);
    let first = samples
        .first()
        .ok_or("window-to-visual multi capture produced no samples")?;
    println!(
        "demo-win: window-to-visual-multi: captured/imported {} samples at {}x{} {:?}; avg={avg_ms:.2}ms max={max_ms:.2}ms",
        samples.len(),
        first.imported_width,
        first.imported_height,
        first.format
    );
    if avg_ms > WINDOW_TO_VISUAL_MULTI_AVG_CAPTURE_IMPORT_LIMIT_MS
        || max_ms > WINDOW_TO_VISUAL_MULTI_MAX_CAPTURE_IMPORT_LIMIT_MS
    {
        return Err(format!(
            "window-to-visual multi capture/import exceeded perf limit: avg {avg_ms:.2}ms > {:.2}ms or max {max_ms:.2}ms > {:.2}ms",
            WINDOW_TO_VISUAL_MULTI_AVG_CAPTURE_IMPORT_LIMIT_MS,
            WINDOW_TO_VISUAL_MULTI_MAX_CAPTURE_IMPORT_LIMIT_MS
        )
        .into());
    }

    for page in pages {
        unsafe {
            page.webview
                .remove_WebMessageReceived(page.web_message_token)?;
            page.controller.Close()?;
        }
    }
    if visible_child_count > 0 {
        return Err(format!(
            "window-to-visual multi exposed {visible_child_count} visible WebView child HWND(s), rejecting as a no-overlay composition target"
        )
        .into());
    }
    println!(
        "demo-win: window-to-visual-multi: PASS - {WINDOW_TO_VISUAL_MULTI_PAGE_COUNT} live pages animated and parent capture/import stayed within budget"
    );
    Ok(())
}

fn create_environment(
    user_data_dir: &std::path::Path,
) -> Result<ICoreWebView2Environment, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(user_data_dir)?;
    let user_data_dir = user_data_dir.to_string_lossy().into_owned();
    let (tx, rx) = mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            let user_data_dir = CoTaskMemPWSTR::from(user_data_dir.as_str());
            let options = CoreWebView2EnvironmentOptions::default();
            unsafe {
                webview2_com::Microsoft::Web::WebView2::Win32::CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    *user_data_dir.as_ref().as_pcwstr(),
                    &ICoreWebView2EnvironmentOptions::from(options),
                    &handler,
                )
                .map_err(webview2_com::Error::WindowsError)
            }
        }),
        Box::new(
            move |error_code: Result<(), windows::core::Error>,
                  environment: Option<ICoreWebView2Environment>| {
                error_code?;
                tx.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .expect("send over mpsc channel");
                Ok(())
            },
        ),
    )?;
    Ok(rx.recv()??)
}

fn create_controller(
    environment: &ICoreWebView2Environment,
    parent_hwnd: HWND,
) -> Result<ICoreWebView2Controller, Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel();
    let environment = environment.clone();
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            environment
                .CreateCoreWebView2Controller(parent_hwnd, &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(
            move |error_code: Result<(), windows::core::Error>,
                  controller: Option<ICoreWebView2Controller>| {
                error_code?;
                tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .expect("send over mpsc channel");
                Ok(())
            },
        ),
    )?;
    Ok(rx.recv()??)
}

fn wait_for_message(
    message_rx: &mpsc::Receiver<String>,
    expected: &str,
    timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_message = String::new();
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        while let Ok(message) = message_rx.try_recv() {
            last_message = message;
            if last_message == expected {
                return Ok(());
            }
        }
    }
    Err(format!("timed out waiting for {expected:?}; last observed {last_message:?}").into())
}

fn wait_for_all_messages(
    message_rx: &mpsc::Receiver<String>,
    expected: &[String],
    timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    let expected = expected.iter().cloned().collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut last_message = String::new();
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        while let Ok(message) = message_rx.try_recv() {
            last_message = message.clone();
            if expected.contains(&message) {
                seen.insert(message);
                if seen.len() == expected.len() {
                    return Ok(());
                }
            }
        }
    }
    let mut missing = expected.difference(&seen).cloned().collect::<Vec<_>>();
    missing.sort();
    Err(
        format!("timed out waiting for messages {missing:?}; last observed {last_message:?}")
            .into(),
    )
}

fn wait_for_keyboard_value(
    message_rx: &mpsc::Receiver<String>,
    expected: &str,
    timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_value = String::new();
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        while let Ok(message) = message_rx.try_recv() {
            if let Some(value) = message.strip_prefix("window-to-visual:keyboard:") {
                last_value = value.to_string();
                if value == expected {
                    return Ok(());
                }
            }
        }
    }
    Err(format!(
        "window-to-visual keyboard timed out: expected {expected:?}, last observed {last_value:?}"
    )
    .into())
}

fn import_parent_capture(
    parent_hwnd: HWND,
    host: &HostWgpuContext,
) -> Result<CaptureImportSample, Box<dyn std::error::Error>> {
    use scrying::windows_capture::{DxgiSharedHandleBridge, capture_window_frame_once};

    let started = std::time::Instant::now();
    let captured = unsafe {
        capture_window_frame_once(parent_hwnd.0 as *mut _, std::time::Duration::from_secs(3))
    }?;
    let dx12 = DxgiSharedHandleBridge.bridge_shared_handle(captured.shared_frame)?;
    let WebSurfaceFrame::Native(native_frame) = dx12.into_surface_frame() else {
        return Err("window-to-visual capture bridge did not produce a native frame".into());
    };
    let importer = WgpuTextureImporter::new(host.clone());
    let imported = importer.import_frame(native_frame, &ImportOptions::default())?;
    println!(
        "demo-win: window-to-visual: captured {}x{}, imported {:?} {}x{} generation {}",
        captured.content_size.width,
        captured.content_size.height,
        imported.format,
        imported.size.width,
        imported.size.height,
        imported.generation
    );
    Ok(CaptureImportSample {
        elapsed: started.elapsed(),
        imported_width: imported.size.width,
        imported_height: imported.size.height,
        format: imported.format,
    })
}

fn multi_page_bounds(index: usize) -> RECT {
    let left = 24 + (index as i32 * 286);
    RECT {
        left,
        top: 42,
        right: left + 260,
        bottom: 430,
    }
}

fn window_to_visual_multi_html(index: usize) -> String {
    let hue = 35 + index * 85;
    format!(
        r#"<!doctype html>
<html>
<head>
    <meta charset="utf-8">
    <style>
        html, body {{
            margin: 0;
            width: 100%;
            height: 100%;
            overflow: hidden;
            background: hsl({hue} 42% 16%);
            color: #fff7dd;
            font-family: system-ui, sans-serif;
        }}
        body {{
            display: grid;
            place-items: center;
        }}
        main {{
            width: 100%;
            height: 100%;
            display: grid;
            place-items: center;
            background:
                linear-gradient(135deg, hsl({hue} 70% 32%), transparent 48%),
                radial-gradient(circle at var(--x, 20%) 35%, hsl({hue} 80% 62%), transparent 22%);
        }}
        strong {{
            font-size: 20px;
            letter-spacing: 0;
        }}
    </style>
</head>
<body>
    <main><strong>Page {index}</strong></main>
    <script>
        const post = value => window.chrome.webview.postMessage(value);
        let frame = 0;
        post("window-to-visual-multi:{index}:ready");
        function tick() {{
            frame += 1;
            document.documentElement.style.setProperty("--x", (10 + (frame % 80)) + "%");
            if (frame === 60) {{
                post("window-to-visual-multi:{index}:frame:60");
            }}
            requestAnimationFrame(tick);
        }}
        requestAnimationFrame(tick);
    </script>
</body>
</html>"#
    )
}

struct WindowToVisualPage {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    web_message_token: i64,
}

struct CaptureImportSample {
    elapsed: std::time::Duration,
    imported_width: u32,
    imported_height: u32,
    format: wgpu::TextureFormat,
}

#[derive(Debug)]
struct ChildWindowInfo {
    class_name: String,
    visible: bool,
}

fn enumerate_child_windows(parent_hwnd: HWND) -> Vec<ChildWindowInfo> {
    unsafe extern "system" fn enum_child(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let children = unsafe { &mut *(lparam.0 as *mut Vec<ChildWindowInfo>) };
        let mut class_buf = [0u16; 128];
        let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) }.max(0) as usize;
        let class_name = String::from_utf16_lossy(&class_buf[..class_len]);
        let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
        children.push(ChildWindowInfo {
            class_name,
            visible,
        });
        true.into()
    }

    let mut children = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(parent_hwnd),
            Some(enum_child),
            LPARAM((&mut children as *mut Vec<ChildWindowInfo>) as isize),
        );
    }
    children
}

fn report_child_windows(label: &str, parent_hwnd: HWND) -> usize {
    let child_windows = enumerate_child_windows(parent_hwnd);
    let visible_count = child_windows.iter().filter(|child| child.visible).count();
    println!(
        "demo-win: {label}: child_hwnds={} visible={} classes={}",
        child_windows.len(),
        visible_count,
        child_windows
            .iter()
            .map(|child| child.class_name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    visible_count
}

struct ForcedHostingModeGuard {
    previous: Option<String>,
}

impl ForcedHostingModeGuard {
    fn window_to_visual() -> Self {
        let previous = std::env::var("COREWEBVIEW2_FORCED_HOSTING_MODE").ok();
        unsafe {
            std::env::set_var(
                "COREWEBVIEW2_FORCED_HOSTING_MODE",
                "COREWEBVIEW2_HOSTING_MODE_WINDOW_TO_VISUAL",
            );
        }
        Self { previous }
    }
}

impl Drop for ForcedHostingModeGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("COREWEBVIEW2_FORCED_HOSTING_MODE", previous);
            } else {
                std::env::remove_var("COREWEBVIEW2_FORCED_HOSTING_MODE");
            }
        }
    }
}

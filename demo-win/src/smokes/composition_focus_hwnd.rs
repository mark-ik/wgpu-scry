// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::super::*;
use super::input::send_system_text;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, IsWindowVisible, SetForegroundWindow, WINDOW_EX_STYLE, WM_CHAR,
    WM_KEYDOWN, WM_KEYUP, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
};
use windows::core::w;

const PANE_COUNT: usize = 3;
const PANE_WIDTH: u32 = 260;
const PANE_HEIGHT: u32 = 220;

pub(crate) fn validate_platform_composition_focus_hwnd(
    window: &Window,
    host: &HostWgpuContext,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::System::WinRT::{
        CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
        DispatcherQueueOptions,
    };

    let parent_hwnd = HWND(hwnd_from_window(window)?);
    let _dispatcher_queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })
    }
    .ok();

    let hidden = run_focus_hwnd_case(parent_hwnd, host, false, "hidden");
    match &hidden {
        Ok(summary) => {
            println!(
                "demo-win: composition-focus-hwnd: hidden PASS - {} panes captured, focused, and typed",
                summary.panes
            );
            return Ok(());
        }
        Err(error) => {
            println!("demo-win: composition-focus-hwnd: hidden failed: {error}");
        }
    }

    let visible = run_focus_hwnd_case(parent_hwnd, host, true, "visible-1x1");
    match visible {
        Ok(summary) => Err(format!(
            "hidden focus HWND failed, but visible 1x1 focus HWND passed for {} panes; still not a hidden-input-sink proof",
            summary.panes
        )
        .into()),
        Err(visible_error) => Err(format!(
            "composition focus-HWND input sink failed: hidden={}; visible-1x1={visible_error}",
            hidden.err().unwrap()
        )
        .into()),
    }
}

fn run_focus_hwnd_case(
    parent_hwnd: HWND,
    host: &HostWgpuContext,
    child_visible: bool,
    label: &str,
) -> Result<CaseSummary, Box<dyn std::error::Error>> {
    let mut panes = Vec::new();
    for index in 0..PANE_COUNT {
        let child = FocusChildWindow::new(parent_hwnd, index, child_visible)?;
        let config = scrying::PlatformWebSurfaceConfig::new(
            winit::dpi::PhysicalSize::new(PANE_WIDTH, PANE_HEIGHT),
            std::env::temp_dir().join(format!("demo-win-composition-focus-{label}-{index}")),
        )
        .with_diagnostic_backdrop((34 + index as u8 * 30, 66, 92));
        let mut producer =
            unsafe { scrying::PlatformWebSurfaceProducer::new(child.hwnd.0 as *mut _, config)? };
        producer.navigate_to_string(
            &composition_focus_html(index),
            std::time::Duration::from_secs(5),
        )?;
        wait_for_web_message(
            &mut producer,
            &format!("composition-focus:{index}:ready"),
            std::time::Duration::from_secs(2),
        )?;
        panes.push(FocusPane { producer, child });
    }

    for (index, pane) in panes.iter_mut().enumerate() {
        let sample = capture_composition_pane(&mut pane.producer, host)?;
        println!(
            "demo-win: composition-focus-hwnd:{label}: pane {index} captured/imported {:?} {}x{} in {:.2}ms child_visible={}",
            sample.format,
            sample.width,
            sample.height,
            sample.elapsed.as_secs_f64() * 1000.0,
            unsafe { IsWindowVisible(pane.child.hwnd).as_bool() }
        );
    }

    for (index, pane) in panes.iter_mut().enumerate() {
        let expected = format!("p{index}x");
        pane.producer
            .post_web_message(&format!("composition-focus:{index}:focus"))?;
        wait_for_web_message(
            &mut pane.producer,
            &format!("composition-focus:{index}:focused:keyboard-smoke"),
            std::time::Duration::from_secs(2),
        )?;
        pane.producer
            .move_focus(scrying::FocusReason::Programmatic)?;
        // No `send_composition_click` here: the equivalent click in the
        // single-pane `--keyboard-test` was shifting DOM focus off the target
        // (verified 2026-05-13). JS focus + foreground+SetFocus is enough in
        // the single-pane case; testing whether the same pattern carries
        // multi-pane via child HWNDs.
        unsafe {
            let _ = SetForegroundWindow(parent_hwnd);
            let _ = SetFocus(Some(pane.child.hwnd));
        }
        pump_windows_messages_for(std::time::Duration::from_millis(150));
        let focused = unsafe { GetFocus() };
        println!(
            "demo-win: composition-focus-hwnd:{label}: pane {index} focus_hwnd={:p} get_focus={:p}",
            pane.child.hwnd.0 as *mut std::ffi::c_void, focused.0 as *mut std::ffi::c_void
        );
        send_system_text(&expected)?;
        match wait_for_keyboard_value(
            &mut pane.producer,
            index,
            &expected,
            std::time::Duration::from_secs(3),
        ) {
            Ok(()) => continue,
            Err(error) => {
                println!(
                    "demo-win: composition-focus-hwnd:{label}: pane {index} physical SendInput failed: {error}"
                );
            }
        }
        send_raw_keyboard_text(&pane.producer, &expected)?;
        wait_for_keyboard_value(
            &mut pane.producer,
            index,
            &expected,
            std::time::Duration::from_secs(2),
        )?;
    }

    Ok(CaseSummary { panes: panes.len() })
}

fn capture_composition_pane(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    host: &HostWgpuContext,
) -> Result<CaptureSample, Box<dyn std::error::Error>> {
    use scrying::windows_capture::close_shared_handle;

    let started = std::time::Instant::now();
    let captured = producer.acquire_full_frame()?;
    let WebSurfaceFrame::Native(ref native_frame) = captured.frame else {
        return Err("composition focus-HWND capture did not produce a native frame".into());
    };
    let importer = WgpuTextureImporter::new(host.clone());
    let imported = importer.import_frame(native_frame, &ImportOptions::default())?;
    unsafe {
        if !captured.shared_handle.is_null() {
            close_shared_handle(captured.shared_handle)?;
        }
    }
    Ok(CaptureSample {
        elapsed: started.elapsed(),
        width: imported.size.width,
        height: imported.size.height,
        format: imported.format,
    })
}

#[allow(dead_code)] // kept for A/B comparison if multi-pane keyboard regresses
fn send_composition_click(
    producer: &scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    producer.send_mouse_input(scrying::MouseInput {
        kind: scrying::MouseEventKind::Move,
        virtual_keys: scrying::MouseVirtualKeys::default(),
        mouse_data: 0,
        point: (132, 126),
    })?;
    producer.send_mouse_input(scrying::MouseInput {
        kind: scrying::MouseEventKind::LeftButtonDown,
        virtual_keys: scrying::MouseVirtualKeys {
            left_button: true,
            ..scrying::MouseVirtualKeys::default()
        },
        mouse_data: 0,
        point: (132, 126),
    })?;
    producer.send_mouse_input(scrying::MouseInput {
        kind: scrying::MouseEventKind::LeftButtonUp,
        virtual_keys: scrying::MouseVirtualKeys::default(),
        mouse_data: 0,
        point: (132, 126),
    })?;
    Ok(())
}

fn send_raw_keyboard_text(
    producer: &scrying::PlatformWebSurfaceProducer,
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    for character in text.chars() {
        let virtual_key_code = match character {
            'a'..='z' => character.to_ascii_uppercase() as u32,
            'A'..='Z' => character as u32,
            '0'..='9' => character as u32,
            _ => return Err(format!("unsupported raw keyboard character: {character:?}").into()),
        };
        producer.forward_keyboard_message(
            WM_KEYDOWN,
            virtual_key_code as usize,
            diagnostic_keyboard_lparam(false, false),
        )?;
        for unit in character.to_string().encode_utf16() {
            producer.forward_keyboard_message(
                WM_CHAR,
                unit as usize,
                diagnostic_keyboard_lparam(false, false),
            )?;
        }
        producer.forward_keyboard_message(
            WM_KEYUP,
            virtual_key_code as usize,
            diagnostic_keyboard_lparam(true, true),
        )?;
    }
    Ok(())
}

fn diagnostic_keyboard_lparam(previous_down: bool, transition_up: bool) -> isize {
    1 | ((previous_down as isize) << 30) | ((transition_up as isize) << 31)
}

fn wait_for_keyboard_value(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    index: usize,
    expected: &str,
    timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    let prefix = format!("composition-focus:{index}:keyboard:");
    let mut last_value = String::new();
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        while let Some(message) = producer.poll_web_message() {
            if let Some(value) = message.strip_prefix(&prefix) {
                last_value = value.to_string();
                if value == expected {
                    println!(
                        "demo-win: composition-focus-hwnd: pane {index} keyboard PASS - DOM input received {value:?}"
                    );
                    return Ok(());
                }
            }
        }
    }
    Err(format!(
        "pane {index} keyboard timed out: expected {expected:?}, last observed {last_value:?}"
    )
    .into())
}

fn composition_focus_html(index: usize) -> String {
    let hue = 170 + index * 40;
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
            background: hsl({hue} 45% 18%);
            color: #fff7dd;
            font-family: system-ui, sans-serif;
        }}
        body {{ display: grid; place-items: center; }}
        main {{ display: grid; gap: 8px; text-align: center; }}
        input {{ width: 180px; padding: 6px 8px; box-sizing: border-box; }}
    </style>
</head>
<body>
    <main>
        <strong>Composition pane {index}</strong>
        <input id="keyboard-smoke" autofocus autocomplete="off" spellcheck="false">
    </main>
    <script>
        const input = document.getElementById("keyboard-smoke");
        const post = value => window.chrome.webview.postMessage(value);
        input.focus();
        post("composition-focus:{index}:ready");
        input.addEventListener("input", () => post("composition-focus:{index}:keyboard:" + input.value));
        window.chrome.webview.addEventListener("message", event => {{
            if (event.data === "composition-focus:{index}:focus") {{
                input.focus();
                post("composition-focus:{index}:focused:" + document.activeElement.id);
            }}
        }});
    </script>
</body>
</html>"#
    )
}

struct FocusPane {
    producer: scrying::PlatformWebSurfaceProducer,
    child: FocusChildWindow,
}

struct FocusChildWindow {
    hwnd: HWND,
}

impl FocusChildWindow {
    fn new(
        parent_hwnd: HWND,
        index: usize,
        visible: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut style = WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN;
        if visible {
            style |= WS_VISIBLE;
        }
        let x = 8 + index as i32 * 2;
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                style,
                x,
                8,
                1,
                1,
                Some(parent_hwnd),
                None,
                None,
                None,
            )?
        };
        Ok(Self { hwnd })
    }
}

impl Drop for FocusChildWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

struct CaseSummary {
    panes: usize,
}

struct CaptureSample {
    elapsed: std::time::Duration,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

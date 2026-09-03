// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Interactive multi-pane diagnostic for the bridge-months question: "does
//! real user keyboard work when scrying hosts multiple WebViews under one OS
//! window?" Plan 2 / Phase A: N producers share one `CompositionRoot` (one
//! `DesktopWindowTarget` per HWND), each in its own pane container visual —
//! the no-overlay composition model, no wrapper child HWNDs. This mode lets a
//! human click each pane and type to see whether DOM key / input events fire
//! on the focused pane.

use windows::Win32::Foundation::HWND;
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT, DispatcherQueueOptions,
};
use winit::dpi::{PhysicalPosition, PhysicalSize};

const PANE_COUNT: usize = 2;
/// Outer margin and inter-pane gap, in physical pixels. Pane width/height are
/// derived from the actual window `inner_size()` so the WebView lays out at
/// `physical / monitor_scale` CSS pixels with room to spare — no hardcoded
/// physical sizes that go too narrow on a HiDPI display.
const PANE_MARGIN: u32 = 16;
const PANE_GAP: u32 = 16;

pub(crate) struct MultiPaneSession {
    panes: Vec<Pane>,
    #[allow(dead_code)]
    dispatcher_queue: Option<windows::System::DispatcherQueueController>,
}

struct Pane {
    index: usize,
    producer: scrying::PlatformWebSurfaceProducer,
    /// Pane rect in physical pixels relative to the parent HWND client area —
    /// used by mouse routing to decide which producer a click goes to.
    rect: PaneRect,
}

#[derive(Clone, Copy, Debug)]
struct PaneRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl PaneRect {
    fn contains(&self, px: f64, py: f64) -> bool {
        let px = px as i64;
        let py = py as i64;
        px >= self.x as i64
            && px < (self.x + self.width) as i64
            && py >= self.y as i64
            && py < (self.y + self.height) as i64
    }
}

impl MultiPaneSession {
    pub(crate) fn new(
        parent_hwnd: HWND,
        window_size: PhysicalSize<u32>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dispatcher_queue = unsafe {
            CreateDispatcherQueueController(DispatcherQueueOptions {
                dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                threadType: DQTYPE_THREAD_CURRENT,
                apartmentType: DQTAT_COM_STA,
            })
        }
        .ok();

        // Plan 2 / Phase A: one shared CompositionRoot (one DesktopWindowTarget
        // on the winit HWND), N producers attached to it via `new_attached`,
        // each in its own pane container visual positioned by `with_offset`.
        // This is the no-overlay multi-pane composition model — no wrapper
        // child HWNDs.
        let composition_root =
            unsafe { scrying::PlatformCompositionRoot::new(parent_hwnd.0 as *mut _)? };
        println!(
            "demo-win: multi-pane: shared CompositionRoot created on parent HWND=0x{:x}",
            parent_hwnd.0 as usize
        );

        // Auto-fit panes to the real window size (physical px). Working from
        // `inner_size()` keeps the sizing DPI-correct by construction — the
        // single-pane path uses the same trick via `capture_size_for_window`.
        let columns = PANE_COUNT as u32;
        let total_gap = PANE_MARGIN * 2 + PANE_GAP * (columns - 1);
        let pane_width = window_size
            .width
            .saturating_sub(total_gap)
            .checked_div(columns)
            .unwrap_or(0)
            .max(160);
        let pane_height = window_size.height.saturating_sub(PANE_MARGIN * 2).max(160);

        let mut panes = Vec::new();
        for index in 0..PANE_COUNT {
            let x = PANE_MARGIN + index as u32 * (pane_width + PANE_GAP);
            let rect = PaneRect {
                x,
                y: PANE_MARGIN,
                width: pane_width,
                height: pane_height,
            };
            let config = scrying::PlatformWebSurfaceConfig::new(
                PhysicalSize::new(pane_width, pane_height),
                std::env::temp_dir().join(format!("demo-win-multi-pane-{index}")),
            )
            .with_offset(x as f32, PANE_MARGIN as f32)
            .with_diagnostic_backdrop((30 + index as u8 * 60, 66, 92));
            let producer = unsafe {
                scrying::PlatformWebSurfaceProducer::new_attached(&composition_root, config)?
            };
            producer
                .navigate_to_string(&multi_pane_html(index), std::time::Duration::from_secs(5))?;
            println!("demo-win: multi-pane: pane {index} attached + navigated, rect={rect:?}");
            panes.push(Pane {
                index,
                producer,
                rect,
            });
        }

        println!(
            "demo-win: multi-pane: {} panes ready on a shared CompositionRoot. Click a pane and type — `[pane N] dom-keydown:` / `dom-input:` lines indicate DOM keyboard delivery.",
            PANE_COUNT
        );

        Ok(Self {
            panes,
            dispatcher_queue,
        })
    }

    /// Route a mouse event to whichever pane's rect contains `cursor`
    /// (physical px, window-client-relative). Forwards `send_mouse_input` with
    /// pane-local coordinates; on a button press, also `move_focus`es that
    /// pane's producer so it takes keyboard focus — the click-to-focus the
    /// single-pane path does, but per-pane.
    pub(crate) fn forward_mouse(
        &mut self,
        cursor: PhysicalPosition<f64>,
        kind: scrying::MouseEventKind,
        mouse_data: i32,
        virtual_keys: scrying::MouseVirtualKeys,
        press: bool,
    ) {
        let Some(pane) = self
            .panes
            .iter_mut()
            .find(|pane| pane.rect.contains(cursor.x, cursor.y))
        else {
            return;
        };
        let local_x = (cursor.x - pane.rect.x as f64) as i32;
        let local_y = (cursor.y - pane.rect.y as f64) as i32;
        if press {
            let _ = pane.producer.move_focus(scrying::FocusReason::Programmatic);
        }
        let event = scrying::MouseInput {
            kind,
            virtual_keys,
            mouse_data,
            point: (local_x, local_y),
        };
        if let Err(error) = pane.producer.send_mouse_input(event) {
            eprintln!(
                "demo-win: multi-pane: send_mouse_input (pane {}) failed: {error}",
                pane.index
            );
        }
    }

    pub(crate) fn drain_messages(&mut self) {
        for pane in &mut self.panes {
            while let Some(message) = pane.producer.poll_web_message() {
                println!("[pane {}] {message}", pane.index);
            }
            while let Some(event) = pane.producer.poll_navigation_event() {
                match event {
                    scrying::NavigationEvent::Completed { url, success } => {
                        println!(
                            "[pane {}] [nav] completed (success={success}) -> {url}",
                            pane.index
                        );
                    }
                    scrying::NavigationEvent::TextInputFocused { state } => {
                        println!(
                            "[pane {}] [text-input] focused {} type={} purpose={:?}",
                            pane.index,
                            state.element_kind,
                            state.input_type,
                            state.purpose()
                        );
                    }
                    scrying::NavigationEvent::TextInputBlurred => {
                        println!("[pane {}] [text-input] blurred", pane.index);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn multi_pane_html(index: usize) -> String {
    format!(
        r##"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<style>
  html, body {{ margin: 0; height: 100%; background: #17202a; color: #f8f1d8; font: 15px system-ui, sans-serif; }}
  body {{ display: grid; place-items: center; gap: 12px; padding: 20px; box-sizing: border-box; }}
  h1 {{ margin: 0; font-size: 22px; color: #ffbe70; }}
  input {{ width: 240px; padding: 6px 8px; border: 1px solid #8fd2c7; border-radius: 4px; background: #0f1720; color: #f8f1d8; font: 15px system-ui, sans-serif; }}
  p {{ margin: 0; color: #8fd2c7; font-size: 12px; }}
</style>
</head>
<body>
  <h1>pane {index}</h1>
  <input id="keyboard-smoke" autocomplete="off" spellcheck="false" aria-label="pane {index} keyboard input">
  <p>click here, then type</p>
  <script>
    const post = msg => {{ try {{ window.chrome.webview.postMessage(msg); }} catch (_) {{}} }};
    const input = document.getElementById("keyboard-smoke");
    input.addEventListener("input", () => post("keyboard-smoke:" + input.value));
    document.addEventListener("keydown", e => post("dom-keydown:" + (e.target?.id || "?") + ":" + e.key), true);
    document.addEventListener("input", e => post("dom-input:" + (e.target?.id || "?") + ":len=" + (e.target?.value?.length ?? -1)), true);
    document.addEventListener("focusin", e => post("focusin:" + (e.target?.id || "?")), true);
    document.addEventListener("focusout", e => post("focusout:" + (e.target?.id || "?")), true);
    post("ready");
  </script>
</body>
</html>"##
    )
}

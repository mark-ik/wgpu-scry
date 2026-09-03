// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Minimal winit + wgpu host probe for scrying system-webview texture interop.

#[cfg(target_os = "windows")]
mod gpu;
#[cfg(target_os = "windows")]
mod multi_pane;
#[cfg(target_os = "windows")]
mod probe;
#[cfg(target_os = "windows")]
mod renderer;
#[cfg(target_os = "windows")]
mod smokes;
#[cfg(target_os = "windows")]
mod waits;

use std::sync::Arc;

#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(target_os = "windows")]
use scrying::Dx12FenceSynchronizer;
use scrying::{
    HostWgpuContext, ImportOptions, NavigationEvent, TextureImporter, WebSurfaceCapabilities,
    WebSurfaceFrame, WebSurfaceSettings, WgpuTextureImporter,
};
use winit::application::ApplicationHandler;
use winit::event::{Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::Window;

#[cfg(target_os = "windows")]
use gpu::{create_host_device, validate_imported_pixels};
#[cfg(target_os = "windows")]
use probe::{
    composition_contains, drain_composition_events, forward_mouse_to_composition, hwnd_from_window,
    run_platform_composition_visual_probe, run_windows_shared_texture_probe,
    validate_platform_multi_view,
};
#[cfg(target_os = "windows")]
use renderer::WebViewRenderer;
#[cfg(target_os = "windows")]
use smokes::browser::{
    validate_platform_browser_controls, validate_platform_context_menu,
    validate_platform_drop_observability, validate_platform_find,
    validate_platform_media_capture_observability, validate_platform_pdf,
    validate_platform_popup_routing,
};
#[cfg(target_os = "windows")]
use smokes::capture::{validate_platform_capture, validate_platform_scale_resize};
#[cfg(target_os = "windows")]
use smokes::composition_focus_hwnd::validate_platform_composition_focus_hwnd;
#[cfg(target_os = "windows")]
use smokes::input::{
    keyboard_validate_enabled, validate_platform_accelerator_bridge, validate_platform_cdp_input,
    validate_platform_ime_bridge, validate_platform_keyboard_smoke, validate_platform_scripted,
};
#[cfg(target_os = "windows")]
use smokes::network::{
    validate_platform_basic_auth, validate_platform_downloads, validate_platform_permissions,
    validate_platform_process_failure_recovery, validate_platform_virtual_host_routing,
    validate_platform_visibility,
};
#[cfg(target_os = "windows")]
use smokes::profile::{
    validate_platform_cookie_store, validate_platform_incognito_store,
    validate_platform_profile_store,
};
#[cfg(target_os = "windows")]
use smokes::window_to_visual::{
    validate_platform_window_to_visual, validate_platform_window_to_visual_multi,
};
#[cfg(target_os = "windows")]
pub(crate) use waits::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let cli = Cli::parse();
    let pending_keyboard_test = cli.keyboard_test || keyboard_validate_enabled();
    let mut app = App {
        cli,
        state: None,
        pending_keyboard_test,
        keyboard_ready_at: None,
    };
    Ok(event_loop.run_app(&mut app)?)
}

#[derive(Clone, Default)]
struct Cli {
    probe_only: bool,
    scripted: bool,
    cookie_test: bool,
    browser_test: bool,
    profile_test: bool,
    incognito_test: bool,
    popup_test: bool,
    process_test: bool,
    routing_test: bool,
    download_test: bool,
    auth_test: bool,
    permission_test: bool,
    visibility_test: bool,
    keyboard_test: bool,
    cdp_input_test: bool,
    accelerator_test: bool,
    ime_bridge_test: bool,
    composition_focus_hwnd_test: bool,
    window_to_visual_test: bool,
    window_to_visual_multi_test: bool,
    multi_view_test: bool,
    find_test: bool,
    pdf_test: bool,
    context_test: bool,
    drop_test: bool,
    media_test: bool,
    capture_test: bool,
    scale_test: bool,
    multi_pane_input_test: bool,
}

impl Cli {
    fn parse() -> Self {
        let mut cli = Self::default();
        for arg in std::env::args().skip(1) {
            match arg.as_str() {
                "--probe-only" => cli.probe_only = true,
                "--scripted" => cli.scripted = true,
                "--cookie-test" => cli.cookie_test = true,
                "--browser-test" => cli.browser_test = true,
                "--profile-test" => cli.profile_test = true,
                "--incognito-test" => cli.incognito_test = true,
                "--popup-test" => cli.popup_test = true,
                "--process-test" => cli.process_test = true,
                "--routing-test" => cli.routing_test = true,
                "--download-test" => cli.download_test = true,
                "--auth-test" => cli.auth_test = true,
                "--permission-test" => cli.permission_test = true,
                "--visibility-test" => cli.visibility_test = true,
                "--keyboard-test" => cli.keyboard_test = true,
                "--cdp-input-test" => cli.cdp_input_test = true,
                "--accelerator-test" => cli.accelerator_test = true,
                "--ime-bridge-test" => cli.ime_bridge_test = true,
                "--composition-focus-hwnd-test" => cli.composition_focus_hwnd_test = true,
                "--window-to-visual-test" => cli.window_to_visual_test = true,
                "--window-to-visual-multi-test" => cli.window_to_visual_multi_test = true,
                "--multi-view-test" => cli.multi_view_test = true,
                "--find-test" => cli.find_test = true,
                "--pdf-test" => cli.pdf_test = true,
                "--context-test" => cli.context_test = true,
                "--drop-test" => cli.drop_test = true,
                "--media-test" => cli.media_test = true,
                "--capture-test" => cli.capture_test = true,
                "--scale-test" => cli.scale_test = true,
                "--multi-pane-input-test" => cli.multi_pane_input_test = true,
                _ => eprintln!("demo-win: unknown arg: {arg}"),
            }
        }
        cli
    }

    fn one_shot(&self) -> bool {
        self.probe_only
            || self.scripted
            || self.cookie_test
            || self.browser_test
            || self.profile_test
            || self.incognito_test
            || self.popup_test
            || self.process_test
            || self.routing_test
            || self.download_test
            || self.auth_test
            || self.permission_test
            || self.visibility_test
            || self.cdp_input_test
            || self.accelerator_test
            || self.ime_bridge_test
            || self.composition_focus_hwnd_test
            || self.window_to_visual_test
            || self.window_to_visual_multi_test
            || self.multi_view_test
            || self.find_test
            || self.pdf_test
            || self.context_test
            || self.drop_test
            || self.media_test
            || self.capture_test
            || self.scale_test
    }
}

#[derive(Default)]
struct App {
    cli: Cli,
    state: Option<AppState>,
    #[cfg(target_os = "windows")]
    pending_keyboard_test: bool,
    #[cfg(target_os = "windows")]
    keyboard_ready_at: Option<std::time::Instant>,
}

struct AppState {
    window: Arc<Window>,
    _device: wgpu::Device,
    _queue: wgpu::Queue,
    #[cfg(target_os = "windows")]
    renderer: Option<WebViewRenderer>,
    /// Last reported cursor position in physical pixels (host window
    /// coordinates). Used to map mouse input into composition-WebView
    /// space.
    cursor: Option<winit::dpi::PhysicalPosition<f64>>,
    /// Bitmask of currently-held mouse buttons, in the layout
    /// `MouseVirtualKeys` expects.
    mouse_buttons: scrying::MouseVirtualKeys,
    /// Bitmask of currently-held modifier keys (Ctrl / Shift).
    modifiers: scrying::MouseVirtualKeys,
    text_input_active: bool,
    text_input_state: Option<scrying::TextInputState>,
    last_cursor_shape: Option<scrying::CursorShape>,
    #[cfg(target_os = "windows")]
    multi_pane: Option<multi_pane::MultiPaneSession>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        let one_shot = self.cli.one_shot();
        match AppState::new(event_loop, &self.cli) {
            Ok(state) => {
                if one_shot {
                    drop(state);
                    event_loop.exit();
                    std::process::exit(0);
                }
                #[cfg(target_os = "windows")]
                if self.pending_keyboard_test {
                    self.keyboard_ready_at =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(250));
                }
                self.state = Some(state);
            }
            Err(error) => {
                eprintln!("demo-win: initialization failed: {error}");
                event_loop.exit();
                if one_shot {
                    std::process::exit(1);
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            #[cfg(target_os = "windows")]
            WindowEvent::Resized(new_size) => {
                if let Some(state) = self.state.as_mut() {
                    if let Some(renderer) = state.renderer.as_mut() {
                        renderer.resize(new_size);
                    }
                }
            }
            #[cfg(target_os = "windows")]
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(state) = self.state.as_mut() {
                    let new_size = state.window.inner_size();
                    println!(
                        "scale-factor changed: scale={} window={}x{}",
                        scale_factor, new_size.width, new_size.height
                    );
                    if let Some(renderer) = state.renderer.as_mut() {
                        renderer.resize(new_size);
                    }
                }
            }
            #[cfg(target_os = "windows")]
            WindowEvent::RedrawRequested => {
                if let Some(state) = self.state.as_mut() {
                    if let Some(renderer) = state.renderer.as_mut() {
                        if let Err(error) = renderer.render() {
                            eprintln!("demo-win: render failed: {error}");
                        }
                    }
                    drain_composition_events(state);
                    if let Some(session) = state.multi_pane.as_mut() {
                        session.drain_messages();
                    }
                }
            }
            #[cfg(target_os = "windows")]
            WindowEvent::Ime(ime) => {
                if let Some(state) = self.state.as_mut() {
                    if let Err(error) = forward_ime_to_composition(state, ime) {
                        eprintln!("demo-win: IME bridge failed: {error}");
                    }
                }
            }
            #[cfg(target_os = "windows")]
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(state) = self.state.as_mut() {
                    state.cursor = Some(position);
                    if state.multi_pane.is_some() {
                        let virtual_keys = multi_pane_virtual_keys(state);
                        if let Some(session) = state.multi_pane.as_mut() {
                            session.forward_mouse(
                                position,
                                scrying::MouseEventKind::Move,
                                0,
                                virtual_keys,
                                false,
                            );
                        }
                    } else {
                        forward_mouse_to_composition(state, scrying::MouseEventKind::Move, 0);
                    }
                }
            }
            #[cfg(target_os = "windows")]
            WindowEvent::MouseInput {
                state: btn_state,
                button,
                ..
            } => {
                if let Some(state) = self.state.as_mut() {
                    use winit::event::{ElementState, MouseButton};
                    let pressed = matches!(btn_state, ElementState::Pressed);
                    let kind = match (button, pressed) {
                        (MouseButton::Left, true) => {
                            state.mouse_buttons.left_button = true;
                            scrying::MouseEventKind::LeftButtonDown
                        }
                        (MouseButton::Left, false) => {
                            state.mouse_buttons.left_button = false;
                            scrying::MouseEventKind::LeftButtonUp
                        }
                        (MouseButton::Right, true) => {
                            state.mouse_buttons.right_button = true;
                            scrying::MouseEventKind::RightButtonDown
                        }
                        (MouseButton::Right, false) => {
                            state.mouse_buttons.right_button = false;
                            scrying::MouseEventKind::RightButtonUp
                        }
                        (MouseButton::Middle, true) => {
                            state.mouse_buttons.middle_button = true;
                            scrying::MouseEventKind::MiddleButtonDown
                        }
                        (MouseButton::Middle, false) => {
                            state.mouse_buttons.middle_button = false;
                            scrying::MouseEventKind::MiddleButtonUp
                        }
                        _ => return,
                    };
                    if state.multi_pane.is_some() {
                        // Multi-pane: route the click to whichever pane's rect
                        // contains the cursor; `forward_mouse` does the
                        // per-pane `move_focus` on press itself.
                        if let Some(cursor) = state.cursor {
                            let virtual_keys = multi_pane_virtual_keys(state);
                            if let Some(session) = state.multi_pane.as_mut() {
                                session.forward_mouse(cursor, kind, 0, virtual_keys, pressed);
                            }
                        }
                    } else {
                        if pressed {
                            // Click into the composition WebView region also
                            // hands keyboard focus to it.
                            if let (Some(pos), Some(renderer)) =
                                (state.cursor, state.renderer.as_mut())
                                && composition_contains(pos, state.window.inner_size())
                            {
                                let _ = renderer
                                    .captured
                                    .producer
                                    .move_focus(scrying::FocusReason::Programmatic);
                            }
                        }
                        forward_mouse_to_composition(state, kind, 0);
                    }
                }
            }
            #[cfg(target_os = "windows")]
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                if let Some(state) = self.state.as_mut() {
                    let (kind, mouse_data) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => {
                            // 120 units == one wheel notch, per the Win32 convention.
                            if y.abs() >= x.abs() {
                                (scrying::MouseEventKind::Wheel, (y * 120.0) as i32)
                            } else {
                                (scrying::MouseEventKind::HorizontalWheel, (x * 120.0) as i32)
                            }
                        }
                        MouseScrollDelta::PixelDelta(p) => {
                            if p.y.abs() >= p.x.abs() {
                                (scrying::MouseEventKind::Wheel, p.y as i32)
                            } else {
                                (scrying::MouseEventKind::HorizontalWheel, p.x as i32)
                            }
                        }
                    };
                    if state.multi_pane.is_some() {
                        if let Some(cursor) = state.cursor {
                            let virtual_keys = multi_pane_virtual_keys(state);
                            if let Some(session) = state.multi_pane.as_mut() {
                                session.forward_mouse(
                                    cursor,
                                    kind,
                                    mouse_data,
                                    virtual_keys,
                                    false,
                                );
                            }
                        }
                    } else {
                        forward_mouse_to_composition(state, kind, mouse_data);
                    }
                }
            }
            #[cfg(target_os = "windows")]
            WindowEvent::ModifiersChanged(modifiers) => {
                if let Some(state) = self.state.as_mut() {
                    let mods = modifiers.state();
                    state.modifiers.control = mods.control_key();
                    state.modifiers.shift = mods.shift_key();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        #[cfg(target_os = "windows")]
        if self.pending_keyboard_test {
            if let (Some(state), Some(ready_at)) = (self.state.as_mut(), self.keyboard_ready_at) {
                if std::time::Instant::now() >= ready_at {
                    self.pending_keyboard_test = false;
                    let result = state.run_keyboard_smoke();
                    match result {
                        Ok(()) => std::process::exit(0),
                        Err(error) => {
                            eprintln!("demo-win: keyboard smoke failed: {error}");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }
}

impl AppState {
    #[cfg(target_os = "windows")]
    fn run_keyboard_smoke(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let parent_hwnd = hwnd_from_window(&self.window)?;
        let Some(renderer) = self.renderer.as_mut() else {
            return Err("keyboard-test requires a live WebView2 renderer".into());
        };
        validate_platform_keyboard_smoke(
            &mut renderer.captured.producer,
            windows::Win32::Foundation::HWND(parent_hwnd),
        )
    }

    fn new(event_loop: &ActiveEventLoop, cli: &Cli) -> Result<Self, Box<dyn std::error::Error>> {
        // Multi-pane mode lays out N panes side by side, so it wants a wider
        // window than the single-pane default.
        let initial_inner_size = if cli.multi_pane_input_test {
            winit::dpi::PhysicalSize::new(1280, 720)
        } else {
            winit::dpi::PhysicalSize::new(900, 600)
        };
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title("demo-win")
                    .with_inner_size(initial_inner_size),
            )?,
        );

        let (instance, device, queue, adapter_info) = pollster::block_on(create_host_device())?;
        let host = HostWgpuContext::new(device.clone(), queue.clone());
        let capabilities = WebSurfaceCapabilities::probe(Some(&host));

        println!("wgpu adapter: {}", adapter_info.name);
        println!("wgpu backend: {:?}", host.backend);
        println!("system webview backend: {:?}", capabilities.backend);
        println!("preferred surface mode: {:?}", capabilities.preferred_mode);
        println!(
            "imported texture support: {:?}",
            capabilities.imported_texture
        );
        println!(
            "native overlay support: {:?}",
            capabilities.native_child_overlay
        );
        println!("CPU snapshot support: {:?}", capabilities.cpu_snapshot);
        println!("reason: {}", capabilities.reason);

        #[cfg(target_os = "windows")]
        run_windows_shared_texture_probe(&window, &host)?;

        // Opt into the explicit-fence cross-API sync path
        // (wgpu D3D12 `Wait` on a `D3D12_FENCE_FLAG_SHARED` fence the WebView2
        // producer signals after `CopyResource`). Disabled by default so the
        // existing keyed-mutex + CPU-spin path stays the verified default.
        // Setting `WEBVIEW_FENCE_SYNC=1` enables it. Mutually exclusive with
        // `WEBVIEW_READBACK_VALIDATE` because that path uses a separate
        // importer that doesn't carry the synchronizer.
        #[cfg(target_os = "windows")]
        let fence_synchronizer: Option<Dx12FenceSynchronizer> = if std::env::var(
            "WEBVIEW_FENCE_SYNC",
        )
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some()
        {
            if std::env::var("WEBVIEW_READBACK_VALIDATE")
                .ok()
                .filter(|v| !v.is_empty() && v != "0")
                .is_some()
            {
                println!(
                    "WEBVIEW_FENCE_SYNC ignored: incompatible with WEBVIEW_READBACK_VALIDATE (separate importer)"
                );
                None
            } else {
                let sync = Dx12FenceSynchronizer::new(&host)?;
                println!(
                    "WEBVIEW_FENCE_SYNC enabled: shared fence handle {:p}",
                    sync.shared_handle().0
                );
                Some(sync)
            }
        } else {
            None
        };

        #[cfg(target_os = "windows")]
        let fence_handle = fence_synchronizer
            .as_ref()
            .map(|s| s.shared_handle().0 as *mut std::ffi::c_void);

        #[cfg(target_os = "windows")]
        let multi_pane = if cli.multi_pane_input_test {
            let parent_hwnd = windows::Win32::Foundation::HWND(hwnd_from_window(&window)?);
            Some(multi_pane::MultiPaneSession::new(
                parent_hwnd,
                window.inner_size(),
            )?)
        } else {
            None
        };

        #[cfg(target_os = "windows")]
        let captured = if cli.multi_pane_input_test {
            None
        } else if cli.multi_view_test {
            validate_platform_multi_view(event_loop, &window)?;
            None
        } else if cli.composition_focus_hwnd_test {
            validate_platform_composition_focus_hwnd(&window, &host)?;
            None
        } else if cli.window_to_visual_multi_test {
            validate_platform_window_to_visual_multi(&window, &host)?;
            None
        } else if cli.window_to_visual_test {
            validate_platform_window_to_visual(&window, &host)?;
            None
        } else {
            run_platform_composition_visual_probe(&window, &host, fence_handle, cli)?
        };

        #[cfg(target_os = "windows")]
        let renderer = if cli.one_shot() || cli.multi_pane_input_test {
            None
        } else {
            match captured {
                Some(captured) => Some(WebViewRenderer::new(
                    instance,
                    window.clone(),
                    host.clone(),
                    captured,
                    fence_synchronizer,
                )?),
                None => {
                    drop(instance);
                    None
                }
            }
        };

        Ok(Self {
            window,
            _device: device,
            _queue: queue,
            #[cfg(target_os = "windows")]
            renderer,
            cursor: None,
            mouse_buttons: scrying::MouseVirtualKeys::default(),
            modifiers: scrying::MouseVirtualKeys::default(),
            text_input_active: false,
            text_input_state: None,
            last_cursor_shape: None,
            #[cfg(target_os = "windows")]
            multi_pane,
        })
    }
}

/// Build the `MouseVirtualKeys` mask for a multi-pane mouse event from the
/// host's tracked modifier and button state — mirrors the inline construction
/// in `probe::forward_mouse_to_composition` for the single-pane path.
#[cfg(target_os = "windows")]
fn multi_pane_virtual_keys(state: &AppState) -> scrying::MouseVirtualKeys {
    let mut virtual_keys = state.modifiers;
    virtual_keys.left_button = state.mouse_buttons.left_button;
    virtual_keys.right_button = state.mouse_buttons.right_button;
    virtual_keys.middle_button = state.mouse_buttons.middle_button;
    virtual_keys
}

#[cfg(target_os = "windows")]
fn forward_ime_to_composition(
    state: &mut AppState,
    ime: Ime,
) -> Result<(), Box<dyn std::error::Error>> {
    if !state.text_input_active {
        // Surfacing this is load-bearing for the manual real-IME run: if the
        // host window receives WM_IME_COMPOSITION before TextInputFocused has
        // fired, we silently drop the composition.
        println!("[ime] dropped (no text-input focus): {ime:?}");
        return Ok(());
    }
    let Some(renderer) = state.renderer.as_mut() else {
        return Ok(());
    };
    match ime {
        Ime::Enabled => {
            println!("[ime] enabled");
        }
        Ime::Disabled => {
            println!("[ime] disabled — clearing composition");
            renderer
                .captured
                .producer
                .set_ime_composition("", 0, 0, 0, 0)?;
        }
        Ime::Preedit(text, cursor_range) => {
            if text.is_empty() {
                println!("[ime] preedit cleared");
                renderer
                    .captured
                    .producer
                    .set_ime_composition("", 0, 0, 0, 0)?;
            } else {
                let (start, end) = cursor_range.unwrap_or((text.len(), text.len()));
                let selection_start = utf16_units_for_byte_index(&text, start);
                let selection_end = utf16_units_for_byte_index(&text, end);
                println!(
                    "[ime] preedit {text:?} cursor=({selection_start}..{selection_end} utf16)"
                );
                renderer.captured.producer.set_ime_composition(
                    &text,
                    selection_start,
                    selection_end,
                    0,
                    0,
                )?;
            }
        }
        Ime::Commit(text) => {
            if text.is_empty() {
                println!("[ime] commit (empty) — ignored");
            } else {
                println!("[ime] commit {text:?}");
                renderer.captured.producer.insert_text(&text)?;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn utf16_units_for_byte_index(text: &str, byte_index: usize) -> i32 {
    let clamped = byte_index.min(text.len());
    let boundary = if text.is_char_boundary(clamped) {
        clamped
    } else {
        let mut idx = clamped;
        while idx > 0 && !text.is_char_boundary(idx) {
            idx -= 1;
        }
        idx
    };
    text[..boundary].encode_utf16().count() as i32
}

// Bounded producer dimensions used by smokes that assert against fixed
// coordinates (textarea caret bounds, profile multi-view layout,
// capture-test resize cycles). Interactive runs size the producer to the
// host window; see `run_platform_composition_visual_probe`.
#[cfg(target_os = "windows")]
const SMOKE_PROBE_X: f32 = 450.0;
#[cfg(target_os = "windows")]
const SMOKE_PROBE_Y: f32 = 48.0;
#[cfg(target_os = "windows")]
const SMOKE_PROBE_WIDTH: f32 = 420.0;
#[cfg(target_os = "windows")]
const SMOKE_PROBE_HEIGHT: f32 = 260.0;

#[cfg(target_os = "windows")]
fn browser_test_html(label: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
    <meta charset="utf-8">
    <title>Scrying Browser Test {label}</title>
    <style>
        html, body {{
            margin: 0;
            width: 100%;
            min-height: 100%;
            background: #11242a;
            color: #f6ead0;
            font-family: system-ui, sans-serif;
        }}
        body {{
            box-sizing: border-box;
            padding: 18px;
        }}
        main {{
            display: grid;
            gap: 8px;
        }}
    </style>
</head>
<body>
    <main>
        <h1>Browser Test {label}</h1>
        <p>WebView2 history and lifecycle probe.</p>
    </main>
    <script>
        const post = value => window.chrome.webview.postMessage(value);
        window.addEventListener("pageshow", () => post("browser-test:ready:{label}"));
    </script>
</body>
</html>"#
    )
}

/// Half the window's width and full height for the WebView capture target.
/// Pairs with the demo's right-half overlay layout: the live composition
/// visual sits on the right, and the wgpu surface fills the whole window
/// behind it with the captured texture stretched, so resize keeps the two
/// in sync.
#[cfg(target_os = "windows")]
fn capture_size_for_window(
    window_size: winit::dpi::PhysicalSize<u32>,
) -> winit::dpi::PhysicalSize<u32> {
    let w = (window_size.width / 2).max(120);
    let h = window_size.height.max(120);
    winit::dpi::PhysicalSize::new(w, h)
}

/// Top-left of the WinComp overlay relative to the parent window. Pairs with
/// `capture_size_for_window`: pin the right-half overlay flush against the
/// window's right edge so it tracks the intended layout as the window resizes.
#[cfg(target_os = "windows")]
fn capture_offset_for_window(window_size: winit::dpi::PhysicalSize<u32>) -> (f32, f32) {
    let capture = capture_size_for_window(window_size);
    let x = window_size.width.saturating_sub(capture.width) as f32;
    (x, 0.0)
}

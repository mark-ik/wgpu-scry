use super::*;

pub(crate) fn composition_contains(
    pos: winit::dpi::PhysicalPosition<f64>,
    window_size: winit::dpi::PhysicalSize<u32>,
) -> bool {
    let x = pos.x as f32;
    let y = pos.y as f32;
    let (offset_x, offset_y) = capture_offset_for_window(window_size);
    let capture_size = capture_size_for_window(window_size);
    x >= offset_x
        && x < offset_x + capture_size.width as f32
        && y >= offset_y
        && y < offset_y + capture_size.height as f32
}

pub(crate) fn forward_mouse_to_composition(
    state: &mut AppState,
    kind: scrying::MouseEventKind,
    mouse_data: i32,
) {
    let Some(pos) = state.cursor else {
        return;
    };
    let Some(renderer) = state.renderer.as_mut() else {
        return;
    };
    let window_size = state.window.inner_size();
    if !composition_contains(pos, window_size) {
        return;
    }
    let (offset_x, offset_y) = capture_offset_for_window(window_size);
    let local_x = (pos.x as f32 - offset_x) as i32;
    let local_y = (pos.y as f32 - offset_y) as i32;
    let mut virtual_keys = state.modifiers;
    virtual_keys.left_button = state.mouse_buttons.left_button;
    virtual_keys.right_button = state.mouse_buttons.right_button;
    virtual_keys.middle_button = state.mouse_buttons.middle_button;
    let event = scrying::MouseInput {
        kind,
        virtual_keys,
        mouse_data,
        point: (local_x, local_y),
    };
    if let Err(error) = renderer.captured.producer.send_mouse_input(event) {
        eprintln!("demo-win: send_mouse_input failed: {error}");
    }
}

pub(crate) fn drain_composition_events(state: &mut AppState) {
    let mut events = Vec::new();
    let mut cursor_shapes = Vec::new();
    {
        let Some(renderer) = state.renderer.as_mut() else {
            return;
        };
        while let Some(event) = renderer.captured.producer.poll_navigation_event() {
            events.push(event);
        }
        while let Some(message) = renderer.captured.producer.poll_web_message() {
            println!("[web message] {message}");
        }
        while let Some(shape) = renderer.captured.producer.poll_cursor_shape() {
            cursor_shapes.push(shape);
        }
    }
    for shape in cursor_shapes {
        if state.last_cursor_shape.as_ref() != Some(&shape) {
            println!("[cursor] {shape:?}");
            state.last_cursor_shape = Some(shape);
        }
    }

    for event in events {
        match event {
            scrying::NavigationEvent::Starting { url } => {
                cancel_text_input_state(state, "navigation starting");
                println!("[nav] starting -> {url}");
            }
            scrying::NavigationEvent::SourceChanged { url } => {
                println!("[nav] source changed -> {url}");
            }
            scrying::NavigationEvent::Completed { url, success } => {
                println!("[nav] completed (success={success}) -> {url}");
            }
            scrying::NavigationEvent::TitleChanged { title } => {
                println!("[nav] title -> {title}");
            }
            scrying::NavigationEvent::NewWindowRequested { url } => {
                println!("[nav] new window requested -> {url}");
            }
            scrying::NavigationEvent::ContentProcessTerminated => {
                cancel_text_input_state(state, "content process terminated");
                println!("[nav] content process terminated");
            }
            scrying::NavigationEvent::TextInputFocused { state: input_state }
            | scrying::NavigationEvent::TextInputChanged { state: input_state } => {
                apply_text_input_state(state, input_state);
            }
            scrying::NavigationEvent::TextInputBlurred => {
                cancel_text_input_state(state, "blurred");
            }
            _ => {}
        }
    }
}

pub(crate) fn apply_text_input_state(state: &mut AppState, input_state: scrying::TextInputState) {
    if input_state.is_password {
        cancel_text_input_state(state, "password-like input suppressed");
        return;
    }

    let purpose = input_state.purpose();
    if purpose == scrying::InputPurpose::Disabled {
        cancel_text_input_state(state, "inputmode=none suppresses IME");
        return;
    }

    state.text_input_active = true;
    state.text_input_state = Some(input_state.clone());
    state.window.set_ime_allowed(true);
    let (position, size) = text_input_ime_area(&state.window, input_state.caret_rect);
    state.window.set_ime_cursor_area(position, size);
    // winit 0.30's `set_ime_purpose` is documented as unsupported on Windows;
    // higher-level hosts that own a TSF text store should consult
    // `TextInputState::purpose()` directly to drive input-scope mapping.
    println!(
        "[text-input] {} type={} mode={} autocomplete={} purpose={:?} caret=({}, {}) {}x{}",
        input_state.element_kind,
        input_state.input_type,
        input_state.input_mode,
        input_state.autocomplete,
        purpose,
        position.x,
        position.y,
        size.width,
        size.height
    );
}

pub(crate) fn cancel_text_input_state(state: &mut AppState, reason: &str) {
    if let Some(renderer) = state.renderer.as_mut() {
        let _ = renderer
            .captured
            .producer
            .set_ime_composition("", 0, 0, 0, 0);
    }
    state.text_input_active = false;
    state.text_input_state = None;
    state.window.set_ime_allowed(false);
    println!("[text-input] {reason}");
}

pub(crate) fn text_input_ime_area(
    window: &Window,
    caret_rect: scrying::TextInputRect,
) -> (
    winit::dpi::LogicalPosition<f64>,
    winit::dpi::LogicalSize<f64>,
) {
    let scale_factor = window.scale_factor().max(1.0);
    let (offset_x, offset_y) = capture_offset_for_window(window.inner_size());
    let position = winit::dpi::LogicalPosition::new(
        offset_x as f64 / scale_factor + caret_rect.x,
        offset_y as f64 / scale_factor + caret_rect.y,
    );
    let size = winit::dpi::LogicalSize::new(caret_rect.width.max(1.0), caret_rect.height.max(1.0));
    (position, size)
}

const COMPOSITION_WEBVIEW_PROBE_HTML: &str = r#"<!doctype html>
<html>
<head>
    <meta charset="utf-8">
    <style>
        html, body {
            margin: 0;
            width: 100%;
            height: 100%;
            overflow: hidden;
            background: #17202a;
            color: #f8f1d8;
            font-family: system-ui, sans-serif;
        }
        body {
            display: grid;
            place-items: center;
            position: relative;
        }
        main {
            display: grid;
            gap: 10px;
            text-align: center;
            z-index: 1;
        }
        h1 {
            margin: 0;
            font-size: 26px;
            font-weight: 650;
            letter-spacing: 0;
        }
        p {
            margin: 0;
            color: #ffbe70;
            font-size: 14px;
        }
        input, textarea {
            width: 220px;
            justify-self: center;
            box-sizing: border-box;
            border: 1px solid #8fd2c7;
            border-radius: 4px;
            background: #0f1720;
            color: #f8f1d8;
            padding: 6px 8px;
            font: 15px system-ui, sans-serif;
            letter-spacing: 0;
        }
        textarea {
            height: 54px;
            resize: none;
            line-height: 20px;
        }
        #matrix {
            display: grid;
            grid-template-columns: repeat(7, minmax(0, 1fr));
            gap: 4px;
            justify-content: center;
            margin-top: 4px;
        }
        #matrix input {
            width: 100%;
            padding: 2px 4px;
            font: 11px system-ui, sans-serif;
        }
        #tick {
            position: absolute;
            top: 8px;
            left: 8px;
            font-size: 13px;
            color: #8fd2c7;
            font-variant-numeric: tabular-nums;
        }
        @keyframes sweep {
            0%   { transform: translateX(0); background: #ff6b6b; }
            50%  { background: #56cfe1; }
            100% { transform: translateX(calc(100% - 24px)); background: #ff6b6b; }
        }
        #bar {
            position: absolute;
            bottom: 12px;
            left: 12px;
            right: 12px;
            height: 6px;
            border-radius: 3px;
            background: #2c3e50;
            overflow: hidden;
        }
        #bar::after {
            content: "";
            display: block;
            width: 24px;
            height: 100%;
            background: #ff6b6b;
            animation: sweep 2.4s linear infinite;
        }
    </style>
</head>
<body>
    <div id="tick">frame 0</div>
    <main>
        <h1>Scrying Composition Probe</h1>
        <p>Rendered through the selected system-webview producer.</p>
        <input id="keyboard-smoke" autofocus autocomplete="off" spellcheck="false" aria-label="keyboard smoke input">
        <textarea id="ime-textarea" autocomplete="off" spellcheck="false" aria-label="IME textarea">first line&#10;second line&#10;third line</textarea>
        <input id="password-smoke" type="password" autocomplete="current-password" aria-label="password suppression input">
        <div id="matrix">
            <input id="purpose-search" type="search" aria-label="purpose search input">
            <input id="purpose-email" type="email" aria-label="purpose email input">
            <input id="purpose-url" type="url" aria-label="purpose url input">
            <input id="purpose-tel" type="tel" aria-label="purpose tel input">
            <input id="purpose-number" type="number" aria-label="purpose number input">
            <input id="purpose-numeric-mode" inputmode="numeric" aria-label="purpose numeric inputmode">
            <input id="purpose-disabled" inputmode="none" aria-label="purpose disabled inputmode">
        </div>
    </main>
    <div id="bar"></div>
    <script>
        let n = 0;
        const tick = document.getElementById("tick");
        const input = document.getElementById("keyboard-smoke");
        input.focus();
        input.addEventListener("input", () => {
            window.__scryingKeyboardSmokeLastInput = input.value;
            window.chrome.webview.postMessage("keyboard-smoke:" + input.value);
        });
        input.addEventListener("keydown", event => {
            window.chrome.webview.postMessage("keyboard-smoke:keydown:" + event.key);
        });
        input.addEventListener("compositionstart", event => {
            window.chrome.webview.postMessage("keyboard-smoke:compositionstart:" + event.data);
        });
        input.addEventListener("compositionupdate", event => {
            window.chrome.webview.postMessage("keyboard-smoke:compositionupdate:" + event.data);
        });
        input.addEventListener("compositionend", event => {
            window.chrome.webview.postMessage("keyboard-smoke:compositionend:" + event.data);
        });
        window.chrome.webview.addEventListener("message", event => {
            if (event.data === "keyboard-smoke:focus") {
                input.focus();
                window.chrome.webview.postMessage("keyboard-smoke:focused:" + document.activeElement.id);
            }
        });
        const post = msg => { try { window.chrome.webview.postMessage(msg); } catch (_) {} };
        const idOf = el => (el && el.id) ? el.id : (el ? el.tagName.toLowerCase() : "?");
        const isPasswordEl = el => {
            if (!el || el.tagName !== "INPUT") return false;
            return (el.getAttribute("type") || "").toLowerCase() === "password";
        };
        document.addEventListener("keydown", event => {
            const target = event.target;
            const key = isPasswordEl(target) ? "<redacted>" : event.key;
            post("dom-keydown:" + idOf(target) + ":" + key);
        }, true);
        document.addEventListener("input", event => {
            const target = event.target;
            const len = (target && typeof target.value === "string") ? target.value.length : -1;
            post("dom-input:" + idOf(target) + ":len=" + len);
        }, true);
        document.addEventListener("compositionstart", event => post("dom-comp-start:" + idOf(event.target)), true);
        document.addEventListener("compositionend", event => post("dom-comp-end:" + idOf(event.target)), true);
        function loop() {
            n++;
            tick.textContent = "frame " + n;
            requestAnimationFrame(loop);
        }
        requestAnimationFrame(loop);
    </script>
</body>
</html>"#;

const COMPOSITION_WEBVIEW_SCRIPTED_HTML: &str = r#"<!doctype html>
<html>
<head>
    <meta charset="utf-8">
    <style>
        html, body {
            margin: 0;
            width: 100%;
            min-height: 100%;
            background: #17202a;
            color: #f8f1d8;
            font-family: system-ui, sans-serif;
        }
        body {
            box-sizing: border-box;
            padding: 18px;
        }
        input {
            box-sizing: border-box;
            width: 220px;
            border: 1px solid #8fd2c7;
            border-radius: 4px;
            background: #0f1720;
            color: #f8f1d8;
            padding: 6px 8px;
            font: 15px system-ui, sans-serif;
            letter-spacing: 0;
        }
        #scroll-target {
            margin-top: 12px;
            height: 520px;
            border-top: 1px solid #8fd2c7;
            color: #ffbe70;
        }
    </style>
</head>
<body>
    <h1>Scrying Windows Scripted Probe</h1>
    <input id="scripted-input" autofocus autocomplete="off" spellcheck="false" aria-label="scripted input">
    <div id="scroll-target">scroll target</div>
    <script>
        const post = value => window.chrome.webview.postMessage(value);
        const input = document.getElementById("scripted-input");
        input.focus();
        window.chrome.webview.addEventListener("message", event => {
            post("scripted:host-echo:" + event.data);
        });
        window.addEventListener("wheel", () => post("scripted:wheel"));
        input.addEventListener("input", () => post("scripted:input:" + input.value));
        post("scripted:ready");
    </script>
</body>
</html>"#;

fn composition_probe_html(scripted: bool) -> &'static str {
    if scripted {
        COMPOSITION_WEBVIEW_SCRIPTED_HTML
    } else {
        COMPOSITION_WEBVIEW_PROBE_HTML
    }
}

pub(crate) fn run_windows_shared_texture_probe(
    window: &Window,
    host: &HostWgpuContext,
) -> Result<(), Box<dyn std::error::Error>> {
    use scrying::windows_capture::{
        D3D11SharedTextureFactory, DxgiSharedHandleBridge, capture_window_frame_once,
        close_shared_handle, probe_graphics_capture_prerequisites,
    };

    let graphics_capture = probe_graphics_capture_prerequisites()?;
    println!(
        "GraphicsCapture probe: session_supported={} winrt_d3d_device={} free_threaded_frame_pool={}",
        graphics_capture.session_supported,
        graphics_capture.winrt_d3d_device_created,
        graphics_capture.free_threaded_frame_pool_created
    );

    let factory = D3D11SharedTextureFactory::new_hardware()?;
    let shared = factory.create_shared_texture_frame(
        winit::dpi::PhysicalSize::new(64, 64),
        wgpu::TextureFormat::Bgra8Unorm,
        1,
    )?;
    let handle = shared.shared_handle;
    let dx12_frame = DxgiSharedHandleBridge.bridge_shared_handle(shared)?;
    println!("D3D11 shared texture probe: exported NT handle {handle:p}");

    let surface_frame = dx12_frame.into_surface_frame();
    let WebSurfaceFrame::Native(native_frame) = surface_frame else {
        return Err("D3D11 shared texture bridge did not produce a native frame".into());
    };
    let importer = WgpuTextureImporter::new(host.clone());
    let imported = importer.import_frame(&native_frame, &ImportOptions::default())?;
    println!(
        "D3D11 shared texture probe: imported {:?} {}x{} generation {}",
        imported.format, imported.size.width, imported.size.height, imported.generation
    );

    unsafe {
        close_shared_handle(handle)?;
    }

    let hwnd = hwnd_from_window(window)?;
    let captured = match unsafe {
        capture_window_frame_once(hwnd, std::time::Duration::from_secs(5))
    } {
        Ok(frame) => frame,
        Err(first_error) => {
            eprintln!(
                "GraphicsCapture window probe: first acquire failed ({first_error}); retrying once"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
            unsafe { capture_window_frame_once(hwnd, std::time::Duration::from_secs(5)) }.map_err(
                |second_error| {
                    format!(
                        "GraphicsCapture window probe failed twice: first={first_error}; second={second_error}"
                    )
                },
            )?
        }
    };
    let captured_handle = captured.shared_frame.shared_handle;
    let captured_dx12 = DxgiSharedHandleBridge.bridge_shared_handle(captured.shared_frame)?;
    let captured_surface_frame = captured_dx12.into_surface_frame();
    let WebSurfaceFrame::Native(captured_native_frame) = captured_surface_frame else {
        return Err("captured window bridge did not produce a native frame".into());
    };
    let captured_imported =
        importer.import_frame(&captured_native_frame, &ImportOptions::default())?;
    println!(
        "GraphicsCapture window probe: captured {}x{}, imported {:?} {}x{} generation {}",
        captured.content_size.width,
        captured.content_size.height,
        captured_imported.format,
        captured_imported.size.width,
        captured_imported.size.height,
        captured_imported.generation
    );
    unsafe {
        close_shared_handle(captured_handle)?;
    }

    Ok(())
}

pub(crate) fn hwnd_from_window(
    window: &Window,
) -> Result<*mut std::ffi::c_void, Box<dyn std::error::Error>> {
    let handle = window.window_handle()?.as_raw();
    match handle {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get() as *mut std::ffi::c_void),
        other => Err(format!("expected Win32 raw window handle, got {other:?}").into()),
    }
}

pub(crate) fn validate_platform_multi_view(
    event_loop: &ActiveEventLoop,
    primary_window: &Window,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::System::WinRT::{
        CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
        DispatcherQueueOptions,
    };

    let _dispatcher_queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })
    }
    .ok();

    let secondary_window = event_loop.create_window(
        Window::default_attributes()
            .with_title("demo-win secondary")
            .with_inner_size(winit::dpi::PhysicalSize::new(640, 420)),
    )?;

    let primary_config = scrying::PlatformWebSurfaceConfig::new(
        winit::dpi::PhysicalSize::new(360, 260),
        std::env::temp_dir().join("demo-win-multi-view-primary"),
    )
    .with_offset(24.0, 24.0)
    .with_diagnostic_backdrop((50, 70, 92));
    let secondary_config = scrying::PlatformWebSurfaceConfig::new(
        winit::dpi::PhysicalSize::new(360, 260),
        std::env::temp_dir().join("demo-win-multi-view-secondary"),
    )
    .with_offset(24.0, 24.0)
    .with_diagnostic_backdrop((80, 64, 72));

    let primary_hwnd = hwnd_from_window(primary_window)?;
    let secondary_hwnd = hwnd_from_window(&secondary_window)?;
    let mut primary =
        unsafe { scrying::PlatformWebSurfaceProducer::new(primary_hwnd, primary_config)? };
    let mut secondary =
        unsafe { scrying::PlatformWebSurfaceProducer::new(secondary_hwnd, secondary_config)? };

    primary.navigate_to_string(
        &multi_view_html("primary"),
        std::time::Duration::from_secs(5),
    )?;
    secondary.navigate_to_string(
        &multi_view_html("secondary"),
        std::time::Duration::from_secs(5),
    )?;
    wait_for_web_message(
        &mut primary,
        "multi-view:primary:ready",
        std::time::Duration::from_secs(2),
    )?;
    wait_for_web_message(
        &mut secondary,
        "multi-view:secondary:ready",
        std::time::Duration::from_secs(2),
    )?;

    drop(secondary);
    drop(primary);
    drop(secondary_window);
    println!(
        "demo-win: multi-view-test: PASS - two simultaneous WebView2 composition producers ran on separate HWNDs"
    );
    Ok(())
}

fn multi_view_html(label: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>{label}</title></head>
<body style="margin:0;display:grid;place-items:center;height:100vh;background:#17202a;color:#f8f1d8;font:16px system-ui,sans-serif;">
<main>{label}</main>
<script>window.chrome.webview.postMessage("multi-view:{label}:ready");</script>
</body>
</html>"#
    )
}

pub(crate) struct CapturedComposition {
    /// The most recently imported WebView texture; `None` until the renderer's
    /// first `try_acquire_frame` lands a frame. The probe path no longer
    /// blocks on an initial acquire (it was an intermittent pump-hang).
    pub(crate) imported: Option<scrying::ImportedTexture>,
    pub(crate) producer: scrying::PlatformWebSurfaceProducer,
    #[allow(dead_code)]
    pub(crate) dispatcher_queue: Option<windows::System::DispatcherQueueController>,
}

pub(crate) fn run_platform_composition_visual_probe(
    window: &Window,
    host: &HostWgpuContext,
    fence_shared_handle: Option<*mut std::ffi::c_void>,
    cli: &Cli,
) -> Result<Option<CapturedComposition>, Box<dyn std::error::Error>> {
    use scrying::windows_capture::close_shared_handle;
    use windows::Win32::System::WinRT::{
        CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
        DispatcherQueueOptions,
    };

    let parent_hwnd = hwnd_from_window(window)?;
    let dispatcher_queue = match unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })
    } {
        Ok(controller) => Some(controller),
        Err(error) => {
            println!(
                "CompositionController visual probe: dispatcher queue setup returned {error}; continuing"
            );
            None
        }
    };

    // Keep the interactive demo's profile durable, but give every one-shot
    // assertion process a fresh profile. Reusing the interactive profile made
    // WebView2 remember permission decisions and bypass PermissionRequested on
    // later hardware runs.
    let user_data_dir = if cli.one_shot() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        std::env::temp_dir().join(format!(
            "demo-win-composition-webview2-smoke-{}-{nonce}",
            std::process::id()
        ))
    } else {
        std::env::temp_dir().join("demo-win-composition-webview2")
    };
    // Smokes assume the bounded constants (textarea caret bounds, profile
    // multi-view layout, capture-test fixed sizes); interactive runs deserve a
    // producer that already matches the resize-time layout (right-half of the
    // host window) so the live visual hit-test region matches the rendered
    // pixels without requiring a manual resize first.
    let (initial_size, initial_offset_x, initial_offset_y) = if cli.one_shot() {
        (
            winit::dpi::PhysicalSize::new(SMOKE_PROBE_WIDTH as u32, SMOKE_PROBE_HEIGHT as u32),
            SMOKE_PROBE_X,
            SMOKE_PROBE_Y,
        )
    } else {
        let window_size = window.inner_size();
        let capture = capture_size_for_window(window_size);
        let (x, y) = capture_offset_for_window(window_size);
        (capture, x, y)
    };
    let mut config = scrying::PlatformWebSurfaceConfig::new(initial_size, user_data_dir.clone())
        .with_offset(initial_offset_x, initial_offset_y)
        .with_diagnostic_backdrop((27, 86, 96));
    if let Some(handle) = fence_shared_handle {
        config = config.with_fence_shared_handle(handle);
    }
    if cli.incognito_test {
        config = config.non_persistent();
    }

    let producer = unsafe { scrying::PlatformWebSurfaceProducer::new(parent_hwnd, config)? };
    producer.navigate_to_string(
        composition_probe_html(cli.scripted),
        std::time::Duration::from_secs(5),
    )?;
    println!("CompositionController visual probe: navigation completed");

    let mut producer = producer;
    if cli.scripted {
        validate_platform_scripted(&mut producer)?;
    }
    if cli.cdp_input_test {
        validate_platform_cdp_input(&mut producer)?;
    }
    if cli.accelerator_test {
        validate_platform_accelerator_bridge(
            &mut producer,
            windows::Win32::Foundation::HWND(parent_hwnd),
        )?;
    }
    if cli.ime_bridge_test {
        validate_platform_ime_bridge(&mut producer, window)?;
    }
    if cli.cookie_test {
        validate_platform_cookie_store(&mut producer)?;
    }
    if cli.browser_test {
        validate_platform_browser_controls(&mut producer)?;
    }
    if cli.popup_test {
        validate_platform_popup_routing(&mut producer)?;
    }
    if cli.routing_test {
        validate_platform_virtual_host_routing(&mut producer)?;
    }
    if cli.process_test {
        validate_platform_process_failure_recovery(&mut producer)?;
    }
    if cli.download_test {
        validate_platform_downloads(&mut producer)?;
    }
    if cli.auth_test {
        validate_platform_basic_auth(&mut producer)?;
    }
    if cli.permission_test {
        validate_platform_permissions(&mut producer)?;
    }
    if cli.visibility_test {
        validate_platform_visibility(&mut producer)?;
    }
    if cli.find_test {
        validate_platform_find(&mut producer)?;
    }
    if cli.pdf_test {
        validate_platform_pdf(&mut producer)?;
    }
    if cli.context_test {
        validate_platform_context_menu(&mut producer)?;
    }
    if cli.drop_test {
        validate_platform_drop_observability(&mut producer)?;
    }
    if cli.media_test {
        validate_platform_media_capture_observability(&mut producer)?;
    }
    if cli.capture_test {
        validate_platform_capture(&mut producer, host)?;
        return Ok(None);
    }
    if cli.scale_test {
        validate_platform_scale_resize(&mut producer, host)?;
        return Ok(None);
    }
    if cli.profile_test {
        validate_platform_profile_store(producer, parent_hwnd, user_data_dir)?;
        return Ok(None);
    }
    if cli.incognito_test {
        validate_platform_incognito_store(producer, parent_hwnd, user_data_dir)?;
        return Ok(None);
    }
    let imported = if std::env::var("WEBVIEW_READBACK_VALIDATE")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some()
    {
        let mut producer_for_readback = producer;
        let captured = producer_for_readback.acquire_full_frame()?;
        let importer = WgpuTextureImporter::new(host.clone());
        let WebSurfaceFrame::Native(ref native_frame) = captured.frame else {
            return Err("WebView2 composition producer did not emit a native frame".into());
        };
        let imported = importer.import_frame(native_frame, &ImportOptions::default())?;
        println!(
            "GraphicsCapture WebView2 CompositionController WebView target visual: captured {}x{}, imported {:?} {}x{} generation {}",
            captured.content_size.width,
            captured.content_size.height,
            imported.format,
            imported.size.width,
            imported.size.height,
            imported.generation
        );
        let html_background_rgb = (0x17u8, 0x20u8, 0x2au8);
        validate_imported_pixels(&imported, &host.device, &host.queue, html_background_rgb)?;
        unsafe {
            close_shared_handle(captured.shared_handle)?;
        }
        return Ok(Some(CapturedComposition {
            imported: Some(imported),
            producer: producer_for_readback,
            dispatcher_queue,
        }));
    } else {
        eprintln!(
            "WebView readback: skipped (set WEBVIEW_READBACK_VALIDATE=1 to enable startup pixel check + initial acquire). Renderer will perform first acquire on its own."
        );
        None
    };

    Ok(Some(CapturedComposition {
        imported,
        producer,
        dispatcher_queue,
    }))
}

use super::super::*;

pub(crate) fn keyboard_validate_enabled() -> bool {
    std::env::var("WEBVIEW_KEYBOARD_VALIDATE")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some()
}

pub(crate) fn validate_platform_keyboard_smoke(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    parent_hwnd: windows::Win32::Foundation::HWND,
) -> Result<(), Box<dyn std::error::Error>> {
    const EXPECTED: &str = "scry42";

    drain_web_messages(producer);
    producer.post_web_message("keyboard-smoke:focus")?;
    wait_for_web_message(
        producer,
        "keyboard-smoke:focused:keyboard-smoke",
        std::time::Duration::from_secs(2),
    )?;
    producer.move_focus(scrying::FocusReason::Programmatic)?;
    pump_windows_messages_for(std::time::Duration::from_millis(150));
    // No mouse-click sequence here: --accelerator-test passes the same
    // SendInput path with no mouse click, so the click was a confound.
    focus_host_window(parent_hwnd)?;
    send_system_text(EXPECTED)?;

    let outcome =
        poll_for_expected_keyboard_value(producer, EXPECTED, std::time::Duration::from_secs(2));
    match outcome {
        Some(value) if value == EXPECTED => {
            println!(
                "demo-win: keyboard-test: PASS - SendInput delivered {value:?} to DOM in pure CompositionController"
            );
            Ok(())
        }
        other => {
            // Failure path: dump HWND tree + GetFocus so the next reader can
            // see what state the smoke was in. Historically (pre-2026-05-13)
            // this smoke included a `producer.send_mouse_input` sequence
            // before SendInput that shifted DOM focus off the target input;
            // removing that fixed it. If you see this message again, check
            // whether something new is interfering with focus after
            // `keyboard-smoke:focused` confirms the JS focus.
            log_keyboard_focus_diagnostics(parent_hwnd, "fail diagnostics");
            let observed = other.unwrap_or_default();
            Err(format!(
                "WebView2 SendInput smoke timed out: expected {EXPECTED:?}, last observed {observed:?}. SendInput-to-DOM is known to work in pure CompositionController (verified 2026-05-13); investigate what disturbed focus."
            )
            .into())
        }
    }
}

fn poll_for_expected_keyboard_value(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    expected: &str,
    timeout: std::time::Duration,
) -> Option<String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut latest = None;
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        while let Some(message) = producer.poll_web_message() {
            if let Some(value) = message.strip_prefix("keyboard-smoke:") {
                let value = value.to_string();
                if value == expected {
                    return Some(value);
                }
                latest = Some(value);
            }
        }
    }
    latest
}

fn log_keyboard_focus_diagnostics(parent_hwnd: windows::Win32::Foundation::HWND, label: &str) {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetParent};

    println!("demo-win: keyboard-test diagnostics [{label}]:");
    let foreground = unsafe { GetForegroundWindow() };
    let focused = unsafe { GetFocus() };
    println!(
        "  GetForegroundWindow=0x{:x}  GetFocus=0x{:x}  parent_hwnd=0x{:x}",
        foreground.0 as usize, focused.0 as usize, parent_hwnd.0 as usize
    );
    let mut tree = Vec::new();
    enumerate_descendants(parent_hwnd, &mut tree);
    for (depth, hwnd, class, visible) in tree {
        let indent = " ".repeat(depth * 2 + 2);
        let parent =
            unsafe { GetParent(hwnd) }.unwrap_or(windows::Win32::Foundation::HWND::default());
        let focus_mark = if hwnd == focused { " [FOCUS]" } else { "" };
        let fg_mark = if hwnd == foreground { " [FG]" } else { "" };
        let vis = if visible { "vis" } else { "hidden" };
        println!(
            "{indent}HWND=0x{:x} parent=0x{:x} class={class:?} {vis}{focus_mark}{fg_mark}",
            hwnd.0 as usize, parent.0 as usize
        );
    }
}

fn enumerate_descendants(
    root: windows::Win32::Foundation::HWND,
    out: &mut Vec<(usize, windows::Win32::Foundation::HWND, String, bool)>,
) {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, GetClassNameW, GetParent, IsWindowVisible,
    };
    use windows::core::BOOL;

    struct Ctx<'a> {
        out: &'a mut Vec<(usize, HWND, String, bool)>,
        root: HWND,
    }

    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        let mut name = [0u16; 256];
        let n = unsafe { GetClassNameW(hwnd, &mut name) };
        let class = String::from_utf16_lossy(&name[..n as usize]);
        let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
        // Walk up to root to compute depth.
        let mut depth = 0;
        let mut current = hwnd;
        while current != ctx.root {
            let parent = unsafe { GetParent(current) }.unwrap_or(HWND::default());
            if parent == HWND::default() {
                break;
            }
            current = parent;
            depth += 1;
        }
        ctx.out.push((depth, hwnd, class, visible));
        BOOL::from(true)
    }

    let mut ctx = Ctx { out, root };
    unsafe {
        let _ = EnumChildWindows(Some(root), Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.out.sort_by_key(|&(d, h, _, _)| (d, h.0 as usize));
}

pub(crate) fn validate_platform_cdp_input(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut route_results = Vec::new();

    route_results.push(run_cdp_dispatch_key_event_route(producer)?);
    route_results.push(run_cdp_insert_text_route(producer)?);
    route_results.push(run_cdp_ime_set_composition_route(producer)?);
    route_results.push(run_cdp_runtime_evaluate_route(producer)?);
    route_results.push(run_execute_script_dom_route(producer)?);

    let bridge_result = producer.execute_script_with_result(
        r#"(() => {
            window.chrome.webview.postMessage("keyboard-smoke:execute-script-message");
            return "posted";
        })()"#,
        std::time::Duration::from_secs(2),
    )?;
    if !bridge_result.contains("posted") {
        return Err(format!(
            "ExecuteScript message bridge probe returned unexpected payload {bridge_result}"
        )
        .into());
    }
    wait_for_web_message(
        producer,
        "keyboard-smoke:execute-script-message",
        std::time::Duration::from_secs(2),
    )?;

    let hash_result = producer.execute_script_with_result(
        r##"(() => {
            window.location.hash = "scrying-script-nav";
            window.chrome.webview.postMessage("keyboard-smoke:hash:" + window.location.hash);
            return window.location.href;
        })()"##,
        std::time::Duration::from_secs(2),
    )?;
    if !hash_result.contains("#scrying-script-nav") {
        return Err(format!(
            "ExecuteScript hash-navigation probe returned unexpected URL {hash_result}"
        )
        .into());
    }
    wait_for_web_message(
        producer,
        "keyboard-smoke:hash:#scrying-script-nav",
        std::time::Duration::from_secs(2),
    )?;

    let has_dom_text_route = route_results
        .iter()
        .any(|result| result.produced_dom_text && result.error.is_none());
    if !has_dom_text_route {
        return Err(format!(
            "no CDP/ExecuteScript route produced DOM input; route results: {}",
            format_route_results(&route_results)
        )
        .into());
    }

    println!(
        "demo-win: cdp-input-test: PASS - {}; ExecuteScript direct message and hash navigation probes passed",
        format_route_results(&route_results)
    );
    Ok(())
}

pub(crate) fn validate_platform_accelerator_bridge(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    parent_hwnd: windows::Win32::Foundation::HWND,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_F3};

    reset_keyboard_probe_input(producer)?;
    drain_navigation_events(producer);
    producer.move_focus(scrying::FocusReason::Programmatic)?;
    pump_windows_messages_for(std::time::Duration::from_millis(150));
    focus_host_window(parent_hwnd)?;
    send_system_key_chord(&[VK_CONTROL], VK_F3)?;

    let event =
        wait_for_accelerator_key(producer, VK_F3.0 as u32, std::time::Duration::from_secs(2))?;
    if !matches!(
        event.kind,
        scrying::KeyEventKind::Down | scrying::KeyEventKind::Up
    ) {
        return Err(format!("unexpected accelerator key kind: {:?}", event.kind).into());
    }

    println!(
        "demo-win: accelerator-test: PASS - WebView2 AcceleratorKeyPressed observed virtual key {} (browser accelerator enabled: {:?})",
        event.virtual_key_code, event.browser_accelerator_key_enabled
    );
    Ok(())
}

pub(crate) fn validate_platform_ime_bridge(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    window: &winit::window::Window,
) -> Result<(), Box<dyn std::error::Error>> {
    reset_keyboard_probe_input(producer)?;
    let state = wait_for_text_input_focus(producer, std::time::Duration::from_secs(2))?;
    if state.element_kind != "input" {
        return Err(format!("expected focused input text state, got {state:?}").into());
    }
    if !state.caret_rect.x.is_finite()
        || !state.caret_rect.y.is_finite()
        || state.caret_rect.width <= 0.0
        || state.caret_rect.height <= 0.0
    {
        return Err(format!("invalid text input caret rect: {:?}", state.caret_rect).into());
    }

    window.set_ime_allowed(true);
    let (ime_position, ime_size) = crate::probe::text_input_ime_area(window, state.caret_rect);
    window.set_ime_cursor_area(ime_position, ime_size);

    producer.set_ime_composition("文", 1, 1, 0, 0)?;
    wait_for_web_message_prefix(
        producer,
        "keyboard-smoke:composition",
        std::time::Duration::from_secs(1),
    )?;
    producer.set_ime_composition("", 0, 0, 0, 0)?;

    reset_keyboard_probe_input(producer)?;
    producer.insert_text("字")?;
    wait_for_web_message(
        producer,
        "keyboard-smoke:字",
        std::time::Duration::from_secs(1),
    )?;
    window.set_ime_allowed(false);

    drain_navigation_events(producer);
    producer.execute_script_with_result(
        r#"(() => {
            const textarea = document.getElementById('ime-textarea');
            textarea.focus();
            textarea.scrollTop = textarea.scrollHeight;
            textarea.setSelectionRange(textarea.value.length, textarea.value.length);
            textarea.dispatchEvent(new Event('input', { bubbles: true }));
            return 'textarea-focused';
        })()"#,
        std::time::Duration::from_secs(2),
    )?;
    let textarea_state = wait_for_text_input_focus(producer, std::time::Duration::from_secs(2))?;
    if textarea_state.element_kind != "textarea" || !textarea_state.is_multiline {
        return Err(
            format!("expected multiline textarea text state, got {textarea_state:?}").into(),
        );
    }
    if textarea_state.caret_rect.y < 0.0
        || textarea_state.caret_rect.y > SMOKE_PROBE_HEIGHT as f64
        || textarea_state.caret_rect.height <= 0.0
    {
        return Err(format!(
            "invalid textarea caret rect after scroll: {:?}",
            textarea_state.caret_rect
        )
        .into());
    }

    let purpose_matrix: &[(&str, scrying::InputPurpose)] = &[
        ("purpose-search", scrying::InputPurpose::Search),
        ("purpose-email", scrying::InputPurpose::Email),
        ("purpose-url", scrying::InputPurpose::Url),
        ("purpose-tel", scrying::InputPurpose::Tel),
        ("purpose-number", scrying::InputPurpose::Decimal),
        ("purpose-numeric-mode", scrying::InputPurpose::Numeric),
        ("purpose-disabled", scrying::InputPurpose::Disabled),
    ];
    for (element_id, expected) in purpose_matrix {
        drain_navigation_events(producer);
        let script = format!(
            r#"(() => {{
                const el = document.getElementById({element_id:?});
                el.focus();
                el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                return {element_id:?} + '-focused';
            }})()"#
        );
        producer.execute_script_with_result(&script, std::time::Duration::from_secs(2))?;
        let state = wait_for_text_input_focus(producer, std::time::Duration::from_secs(2))?;
        let actual = state.purpose();
        if actual != *expected {
            return Err(format!(
                "ime-bridge-test purpose matrix: #{element_id} expected {expected:?}, got {actual:?} (state={state:?})"
            )
            .into());
        }
    }

    drain_navigation_events(producer);
    producer.execute_script_with_result(
        r#"(() => {
            document.getElementById('password-smoke').focus();
            return 'password-focused';
        })()"#,
        std::time::Duration::from_secs(2),
    )?;
    wait_for_text_input_blur(producer, std::time::Duration::from_secs(2))?;

    println!(
        "demo-win: ime-bridge-test: PASS - focused editable caret state mapped to winit IME area, CDP composition and commit reached DOM; textarea scroll rect, purpose matrix ({purposes}), and password suppression passed",
        purposes = purpose_matrix
            .iter()
            .map(|(id, purpose)| format!("{id}->{purpose:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

fn run_cdp_dispatch_key_event_route(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<CdpRouteResult, Box<dyn std::error::Error>> {
    const EXPECTED: &str = "abc";
    reset_keyboard_probe_input(producer)?;
    let mut error = None;
    for ch in EXPECTED.chars() {
        if let Err(route_error) = dispatch_cdp_key(producer, ch) {
            error = Some(route_error.to_string());
            break;
        }
    }
    let produced_dom_text = wait_for_web_message(
        producer,
        "keyboard-smoke:abc",
        std::time::Duration::from_secs(1),
    )
    .is_ok();
    Ok(CdpRouteResult {
        name: "Input.dispatchKeyEvent",
        produced_dom_text,
        observed_composition: false,
        error,
    })
}

fn run_cdp_insert_text_route(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<CdpRouteResult, Box<dyn std::error::Error>> {
    const EXPECTED: &str = "insert42";
    reset_keyboard_probe_input(producer)?;
    let params = format!(r#"{{"text":{EXPECTED:?}}}"#);
    let error = producer
        .call_devtools_protocol_method_blocking(
            "Input.insertText",
            &params,
            std::time::Duration::from_secs(2),
        )
        .err()
        .map(|error| error.to_string());
    let produced_dom_text = wait_for_web_message(
        producer,
        "keyboard-smoke:insert42",
        std::time::Duration::from_secs(1),
    )
    .is_ok();
    Ok(CdpRouteResult {
        name: "Input.insertText",
        produced_dom_text,
        observed_composition: false,
        error,
    })
}

fn run_cdp_ime_set_composition_route(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<CdpRouteResult, Box<dyn std::error::Error>> {
    reset_keyboard_probe_input(producer)?;
    let params = r#"{"text":"文","selectionStart":1,"selectionEnd":1,"replacementStart":0,"replacementEnd":0}"#;
    let error = producer
        .call_devtools_protocol_method_blocking(
            "Input.imeSetComposition",
            params,
            std::time::Duration::from_secs(2),
        )
        .err()
        .map(|error| error.to_string());
    let observed_composition = wait_for_web_message_prefix(
        producer,
        "keyboard-smoke:composition",
        std::time::Duration::from_secs(1),
    )
    .is_ok();
    let produced_dom_text = wait_for_web_message(
        producer,
        "keyboard-smoke:文",
        std::time::Duration::from_millis(250),
    )
    .is_ok();
    let _ = producer.call_devtools_protocol_method_blocking(
        "Input.imeSetComposition",
        r#"{"text":"","selectionStart":0,"selectionEnd":0,"replacementStart":0,"replacementEnd":0}"#,
        std::time::Duration::from_millis(750),
    );
    Ok(CdpRouteResult {
        name: "Input.imeSetComposition",
        produced_dom_text,
        observed_composition,
        error,
    })
}

fn run_cdp_runtime_evaluate_route(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<CdpRouteResult, Box<dyn std::error::Error>> {
    const EXPECTED: &str = "runtime42";
    reset_keyboard_probe_input(producer)?;
    let expression = dom_insert_expression(EXPECTED);
    let params =
        format!(r#"{{"expression":{expression:?},"awaitPromise":true,"userGesture":true}}"#);
    let error = producer
        .call_devtools_protocol_method_blocking(
            "Runtime.evaluate",
            &params,
            std::time::Duration::from_secs(2),
        )
        .err()
        .map(|error| error.to_string());
    let produced_dom_text = wait_for_web_message(
        producer,
        "keyboard-smoke:runtime42",
        std::time::Duration::from_secs(1),
    )
    .is_ok();
    Ok(CdpRouteResult {
        name: "Runtime.evaluate",
        produced_dom_text,
        observed_composition: false,
        error,
    })
}

fn run_execute_script_dom_route(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<CdpRouteResult, Box<dyn std::error::Error>> {
    const EXPECTED: &str = "script42";
    reset_keyboard_probe_input(producer)?;
    let script = dom_insert_expression(EXPECTED);
    let error = producer
        .execute_script_with_result(&script, std::time::Duration::from_secs(2))
        .err()
        .map(|error| error.to_string());
    let produced_dom_text = wait_for_web_message(
        producer,
        "keyboard-smoke:script42",
        std::time::Duration::from_secs(1),
    )
    .is_ok();
    Ok(CdpRouteResult {
        name: "ExecuteScript",
        produced_dom_text,
        observed_composition: false,
        error,
    })
}

fn reset_keyboard_probe_input(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    drain_web_messages(producer);
    producer.post_web_message("keyboard-smoke:focus")?;
    wait_for_web_message(
        producer,
        "keyboard-smoke:focused:keyboard-smoke",
        std::time::Duration::from_secs(2),
    )?;
    let result = producer.execute_script_with_result(
        r#"(() => {
            const input = document.getElementById("keyboard-smoke");
            input.value = "";
            input.focus();
            return document.activeElement && document.activeElement.id;
        })()"#,
        std::time::Duration::from_secs(2),
    )?;
    if !result.contains("keyboard-smoke") {
        return Err(format!("keyboard probe reset focused unexpected element: {result}").into());
    }
    drain_web_messages(producer);
    Ok(())
}

fn dispatch_cdp_key(
    producer: &scrying::PlatformWebSurfaceProducer,
    ch: char,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some((virtual_key, code, key)) = cdp_key_fields(ch) else {
        return Err(format!("unsupported CDP key character: {ch:?}").into());
    };
    let text = ch.to_string();
    let raw_down = format!(
        r#"{{"type":"rawKeyDown","windowsVirtualKeyCode":{virtual_key},"nativeVirtualKeyCode":{virtual_key},"code":{code:?},"key":{key:?},"unmodifiedText":{text:?}}}"#
    );
    producer.call_devtools_protocol_method_blocking(
        "Input.dispatchKeyEvent",
        &raw_down,
        std::time::Duration::from_secs(2),
    )?;
    let char_event = format!(
        r#"{{"type":"char","windowsVirtualKeyCode":{virtual_key},"nativeVirtualKeyCode":{virtual_key},"code":{code:?},"key":{key:?},"text":{text:?},"unmodifiedText":{text:?}}}"#
    );
    producer.call_devtools_protocol_method_blocking(
        "Input.dispatchKeyEvent",
        &char_event,
        std::time::Duration::from_secs(2),
    )?;
    let key_up = format!(
        r#"{{"type":"keyUp","windowsVirtualKeyCode":{virtual_key},"nativeVirtualKeyCode":{virtual_key},"code":{code:?},"key":{key:?}}}"#
    );
    producer.call_devtools_protocol_method_blocking(
        "Input.dispatchKeyEvent",
        &key_up,
        std::time::Duration::from_secs(2),
    )?;
    Ok(())
}

fn cdp_key_fields(ch: char) -> Option<(u32, String, String)> {
    match ch {
        'a'..='z' => {
            let upper = ch.to_ascii_uppercase();
            Some((upper as u32, format!("Key{upper}"), ch.to_string()))
        }
        'A'..='Z' => Some((ch as u32, format!("Key{ch}"), ch.to_string())),
        '0'..='9' => Some((ch as u32, format!("Digit{ch}"), ch.to_string())),
        _ => None,
    }
}

fn dom_insert_expression(text: &str) -> String {
    format!(
        r#"(() => {{
            const text = {text:?};
            const active = document.activeElement;
            if (!active) return "no-active-element";
            if ("value" in active) {{
                active.value = "";
                if (typeof active.setSelectionRange === "function") {{
                    active.setSelectionRange(0, 0);
                }}
            }} else {{
                active.textContent = "";
            }}
            const usedCommand = typeof document.execCommand === "function" &&
                document.execCommand("insertText", false, text);
            if (!usedCommand) {{
                if ("value" in active) {{
                    active.value = text;
                    if (typeof active.setSelectionRange === "function") {{
                        active.setSelectionRange(text.length, text.length);
                    }}
                }} else {{
                    active.textContent = text;
                }}
            }}
            const inputEvent = typeof InputEvent === "function" ?
                new InputEvent("input", {{
                    bubbles: true,
                    data: text,
                    inputType: "insertText"
                }}) :
                new Event("input", {{ bubbles: true }});
            active.dispatchEvent(inputEvent);
            return "value" in active ? active.value : active.textContent;
        }})()"#
    )
}

struct CdpRouteResult {
    name: &'static str,
    produced_dom_text: bool,
    observed_composition: bool,
    error: Option<String>,
}

fn format_route_results(results: &[CdpRouteResult]) -> String {
    results
        .iter()
        .map(|result| {
            let status = if let Some(error) = &result.error {
                format!("error={error}")
            } else if result.produced_dom_text {
                "DOM input observed".to_string()
            } else if result.observed_composition {
                "composition observed".to_string()
            } else {
                "completed without DOM input".to_string()
            };
            format!("{}: {status}", result.name)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn focus_host_window(
    parent_hwnd: windows::Win32::Foundation::HWND,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::System::Threading::AttachThreadInput;
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let foreground = unsafe { GetForegroundWindow() };
        let host_thread = unsafe { GetWindowThreadProcessId(parent_hwnd, None) };
        let foreground_thread = unsafe { GetWindowThreadProcessId(foreground, None) };
        // A service-launched self-hosted runner can leave another window's
        // input queue in front. Attach only long enough to perform the normal
        // foreground handoff; setting Win32 focus here breaks WebView2's
        // CompositionController focus established above.
        let attached = host_thread != 0
            && foreground_thread != 0
            && host_thread != foreground_thread
            && unsafe { AttachThreadInput(host_thread, foreground_thread, true).as_bool() };
        unsafe {
            let _ = BringWindowToTop(parent_hwnd);
            let _ = SetForegroundWindow(parent_hwnd);
            if attached {
                let _ = AttachThreadInput(host_thread, foreground_thread, false);
            }
        }
        pump_windows_messages_for(std::time::Duration::from_millis(50));
        let foreground = unsafe { GetForegroundWindow() };
        if foreground == parent_hwnd {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "physical keyboard gate could not acquire foreground: expected host HWND 0x{:x}, current foreground HWND 0x{:x}",
                parent_hwnd.0 as usize, foreground.0 as usize
            )
            .into());
        }
    }
}

pub(crate) fn send_system_text(text: &str) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT, VkKeyScanW,
    };

    let mut inputs = Vec::new();
    for unit in text.encode_utf16() {
        let key_scan = unsafe { VkKeyScanW(unit) };
        if key_scan == -1 {
            return Err(format!("keyboard-test: no virtual key mapping for U+{unit:04X}").into());
        }
        let virtual_key = VIRTUAL_KEY((key_scan as u16) & 0x00FF);
        let shift_state = ((key_scan as u16) >> 8) & 0x00FF;
        let mut modifiers = Vec::new();
        if shift_state & 0x01 != 0 {
            modifiers.push(VK_SHIFT);
        }
        if shift_state & 0x02 != 0 {
            modifiers.push(VK_CONTROL);
        }
        if shift_state & 0x04 != 0 {
            modifiers.push(VK_MENU);
        }

        for modifier in &modifiers {
            push_key_input(&mut inputs, *modifier, false);
        }
        push_key_input(&mut inputs, virtual_key, false);
        push_key_input(&mut inputs, virtual_key, true);
        for modifier in modifiers.iter().rev() {
            push_key_input(&mut inputs, *modifier, true);
        }
    }

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(format!(
            "keyboard-test: SendInput sent {sent} of {} events",
            inputs.len()
        )
        .into());
    }
    Ok(())
}

pub(crate) fn send_system_key_chord(
    modifiers: &[windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY],
    key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, SendInput};

    let mut inputs = Vec::new();
    for modifier in modifiers {
        push_key_input(&mut inputs, *modifier, false);
    }
    push_key_input(&mut inputs, key, false);
    push_key_input(&mut inputs, key, true);
    for modifier in modifiers.iter().rev() {
        push_key_input(&mut inputs, *modifier, true);
    }

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(format!(
            "accelerator-test: SendInput sent {sent} of {} events",
            inputs.len()
        )
        .into());
    }
    Ok(())
}

fn push_key_input(
    inputs: &mut Vec<windows::Win32::UI::Input::KeyboardAndMouse::INPUT>,
    virtual_key: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
    key_up: bool,
) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    };

    inputs.push(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: virtual_key,
                wScan: 0,
                dwFlags: if key_up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

pub(crate) fn validate_platform_scripted(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    const MESSAGE: &str = "ping-from-demo-win";
    const EXPECTED_ECHO: &str = "scripted:host-echo:ping-from-demo-win";

    wait_for_web_message(
        producer,
        "scripted:ready",
        std::time::Duration::from_secs(2),
    )?;
    producer.post_web_message(MESSAGE)?;
    wait_for_web_message(producer, EXPECTED_ECHO, std::time::Duration::from_secs(2))?;

    producer.send_mouse_input(scrying::MouseInput {
        kind: scrying::MouseEventKind::Move,
        virtual_keys: scrying::MouseVirtualKeys::default(),
        mouse_data: 0,
        point: (32, 32),
    })?;
    producer.send_mouse_input(scrying::MouseInput {
        kind: scrying::MouseEventKind::Wheel,
        virtual_keys: scrying::MouseVirtualKeys::default(),
        mouse_data: 120,
        point: (32, 32),
    })?;

    producer.move_focus(scrying::FocusReason::Programmatic)?;
    send_scripted_key_pair(producer, 'a')?;
    send_scripted_key_pair(producer, 'b')?;
    send_scripted_key_pair(producer, 'c')?;
    wait_for_web_message(
        producer,
        "scripted:input:abc",
        std::time::Duration::from_secs(2),
    )?;

    println!(
        "demo-win: scripted: PASS - JS message round-trip plus mouse and CDP-backed keyboard dispatch verified"
    );
    Ok(())
}

fn send_scripted_key_pair(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    ch: char,
) -> Result<(), Box<dyn std::error::Error>> {
    let virtual_key_code = match ch {
        'a'..='z' => ch.to_ascii_uppercase() as u32,
        'A'..='Z' => ch as u32,
        '0'..='9' => ch as u32,
        _ => return Err(format!("unsupported scripted keyboard character: {ch:?}").into()),
    };
    let characters = ch.to_string();
    producer.send_keyboard_input(scrying::KeyboardInput {
        kind: scrying::KeyEventKind::Down,
        virtual_key_code,
        characters: characters.clone(),
        characters_ignoring_modifiers: characters,
        modifiers: scrying::KeyModifierFlags::default(),
        is_repeat: false,
    })?;
    producer.send_keyboard_input(scrying::KeyboardInput {
        kind: scrying::KeyEventKind::Up,
        virtual_key_code,
        characters: String::new(),
        characters_ignoring_modifiers: String::new(),
        modifiers: scrying::KeyModifierFlags::default(),
        is_repeat: false,
    })?;
    Ok(())
}

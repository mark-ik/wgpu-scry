//! Minimal Linux runtime probe for scrying's WebKitGTK producer.
//!
//! Hosts a [`WebKitGtkProducer`] in an offscreen WebKit page, drives a
//! navigation, takes a CPU-RGBA snapshot, and writes it to disk.
//!
//! ```sh
//! cargo run -p demo-linux                                  # default HTML page → snapshot.png
//! cargo run -p demo-linux -- --url https://example.com
//! cargo run -p demo-linux -- --snapshot-test               # exit-1 on empty / missing frame
//! cargo run -p demo-linux -- --probe-only                  # capability probe + exit
//! ```

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use dpi::PhysicalSize;
use scrying::webkitgtk_producer::{WebKitGtkProducer, WebKitGtkProducerConfig};
use scrying::{
    Cookie, CursorShape, DragEventKind, DragInput, FocusReason, KeyEventKind, KeyModifierFlags,
    KeyboardInput, MouseEventKind, MouseInput, MouseVirtualKeys, NavigationEvent,
    UrlSchemeHandlerFn, UrlSchemeResponse, WebSurfaceCapabilities, WebSurfaceFrame,
    WebSurfaceProducer,
};

const DEFAULT_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>scrying linux smoke</title></head>
<body style="margin:0;display:flex;align-items:center;justify-content:center;
height:100vh;background:linear-gradient(135deg,#1e293b,#0f172a);color:#facc15;
font:bold 64px system-ui,sans-serif">scrying · linux</body></html>"#;

const SCRIPTED_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>scrying scripted</title></head>
<body><script>
// Echo every host → page message back with an "echo:" prefix.
window.chrome.webview.addEventListener('message', function(e) {
    window.chrome.webview.postMessage('echo:' + e.data);
});
// Tell the host we're loaded.
window.chrome.webview.postMessage('hello from page');
</script></body></html>"#;

const INPUT_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>scrying input</title></head>
<body>
<button id="btn" style="position:absolute;left:100px;top:100px;width:200px;height:60px">click target</button>
<script>
var btn = document.getElementById('btn');
btn.addEventListener('mousedown', function(e) {
    window.chrome.webview.postMessage('mousedown@' + e.clientX + ',' + e.clientY + ' trusted=' + e.isTrusted);
});
btn.addEventListener('mouseup', function(e) {
    window.chrome.webview.postMessage('mouseup@' + e.clientX + ',' + e.clientY + ' trusted=' + e.isTrusted);
});
document.addEventListener('keydown', function(e) {
    window.chrome.webview.postMessage('keydown:' + e.key + ' trusted=' + e.isTrusted);
});
</script></body></html>"#;

fn main() -> ExitCode {
    // WebKitGTK 2.40+ uses a DMABUF-based renderer plus accelerated
    // compositing by default. Both paths require GDK to successfully
    // create a GL context, which can fail with `GDK is not able to
    // create a GL context: The current backend does not support
    // OpenGL` on some GTK 3 + Wayland setups even when GL itself works
    // fine for other processes. The CPU snapshot path
    // (`webkit_web_view_get_snapshot` → cairo `ImageSurface`) does not
    // benefit from accelerated compositing, so force the software
    // rendering path. Hosts that need AC for a future GPU capture path
    // should leave these unset and ensure GDK can create a GL context
    // on their target session.
    // Safety: env-var writes must happen before any other thread spawns;
    // `main` is single-threaded at this point.
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("demo-linux: {err}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    output_path: PathBuf,
    url: Option<String>,
    snapshot_test: bool,
    probe_only: bool,
    scripted: bool,
    input_test: bool,
    cookie_test: bool,
    scheme_test: bool,
    popup_test: bool,
    download_test: bool,
    cursor_test: bool,
    ime_test: bool,
    drag_test: bool,
    text_test: bool,
    width: u32,
    height: u32,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut out = Args {
            output_path: "scrying-linux-snapshot.png".into(),
            url: None,
            snapshot_test: false,
            probe_only: false,
            scripted: false,
            input_test: false,
            cookie_test: false,
            scheme_test: false,
            popup_test: false,
            download_test: false,
            cursor_test: false,
            ime_test: false,
            drag_test: false,
            text_test: false,
            width: 800,
            height: 600,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--out" => {
                    out.output_path = args.next().ok_or("--out needs a path")?.into();
                }
                "--url" => {
                    out.url = Some(args.next().ok_or("--url needs a value")?);
                }
                "--width" => {
                    out.width = args
                        .next()
                        .ok_or("--width needs a value")?
                        .parse()
                        .map_err(|e| format!("invalid --width: {e}"))?;
                }
                "--height" => {
                    out.height = args
                        .next()
                        .ok_or("--height needs a value")?
                        .parse()
                        .map_err(|e| format!("invalid --height: {e}"))?;
                }
                "--snapshot-test" => out.snapshot_test = true,
                "--probe-only" => out.probe_only = true,
                "--scripted" => out.scripted = true,
                "--input-test" => out.input_test = true,
                "--cookie-test" => out.cookie_test = true,
                "--scheme-test" => out.scheme_test = true,
                "--popup-test" => out.popup_test = true,
                "--download-test" => out.download_test = true,
                "--cursor-test" => out.cursor_test = true,
                "--ime-test" => out.ime_test = true,
                "--drag-test" => out.drag_test = true,
                "--text-test" => out.text_test = true,
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                _ => return Err(format!("unknown arg: {arg}")),
            }
        }
        Ok(out)
    }
}

fn print_help() {
    println!("demo-linux — WebKitGTK runtime probe for scrying");
    println!();
    println!("USAGE: demo-linux [--url URL] [--out PATH] [--width N] [--height N]");
    println!("                  [--snapshot-test] [--scripted] [--input-test] [--cookie-test]");
    println!("                  [--scheme-test] [--popup-test] [--download-test]");
    println!("                  [--cursor-test] [--ime-test] [--drag-test] [--text-test]");
    println!("                  [--probe-only]");
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Capability probe first — exercises detect() + probe() against the
    // current build's feature flags.
    let caps = WebSurfaceCapabilities::probe(None);
    println!("backend: {:?}", caps.backend);
    println!("preferred mode: {:?}", caps.preferred_mode);
    println!("CPU snapshot: {:?}", caps.cpu_snapshot);
    println!("reason: {}", caps.reason);
    if args.probe_only {
        return Ok(());
    }

    let data_dir = std::env::temp_dir().join("scrying-demo-linux-data");
    let config =
        WebKitGtkProducerConfig::new(PhysicalSize::new(args.width, args.height), &data_dir);
    let mut producer = if args.scheme_test || args.download_test {
        let mut schemes: HashMap<String, UrlSchemeHandlerFn> = HashMap::new();
        // The `scry://` scheme serves either an HTML page (for the
        // scheme test) or a fixed-bytes attachment when the path
        // starts with `/download/` (for the download test).
        schemes.insert(
            "scry".to_string(),
            Arc::new(|uri: &str| {
                if uri.contains("/download/") {
                    UrlSchemeResponse {
                        mime_type: "application/octet-stream".to_string(),
                        body: b"scrying download payload\n".to_vec(),
                        headers: vec![(
                            "Content-Disposition".to_string(),
                            "attachment; filename=\"scry.bin\"".to_string(),
                        )],
                    }
                } else {
                    let body = format!(
                        "<!doctype html><html><body><script>\
                         window.chrome.webview.postMessage('scheme served: {uri}');\
                         </script></body></html>"
                    );
                    UrlSchemeResponse {
                        mime_type: "text/html".to_string(),
                        body: body.into_bytes(),
                        headers: vec![("X-Scry-Source".to_string(), "demo-linux".to_string())],
                    }
                }
            }),
        );
        WebKitGtkProducer::new_with_url_schemes(config, schemes)?
    } else {
        WebKitGtkProducer::new(config)?
    };

    let nav_timeout = Duration::from_secs(5);

    if args.scripted {
        return run_scripted(&mut producer, nav_timeout);
    }
    if args.input_test {
        return run_input_test(&mut producer, nav_timeout);
    }
    if args.cookie_test {
        return run_cookie_test(&producer);
    }
    if args.scheme_test {
        return run_scheme_test(&mut producer, nav_timeout);
    }
    if args.popup_test {
        return run_popup_test(&mut producer, nav_timeout);
    }
    if args.download_test {
        return run_download_test(&producer);
    }
    if args.cursor_test {
        return run_cursor_test(&mut producer, nav_timeout);
    }
    if args.ime_test {
        return run_ime_test(&mut producer, nav_timeout);
    }
    if args.drag_test {
        return run_drag_test(&mut producer, nav_timeout);
    }
    if args.text_test {
        return run_text_test(&mut producer, nav_timeout);
    }

    match &args.url {
        Some(url) => {
            println!("navigating to {url}");
            producer.navigate_to_url(url, nav_timeout)?;
        }
        None => {
            println!("navigating to inline HTML");
            producer.navigate_to_string(DEFAULT_HTML, nav_timeout)?;
        }
    }
    println!("committed: {:?}", producer.committed_uri());

    let frame = producer.acquire_frame()?;
    match frame {
        WebSurfaceFrame::CpuRgba {
            size,
            pixels,
            generation,
        } => {
            println!(
                "CpuRgba snapshot: {}x{} gen={}",
                size.width, size.height, generation
            );
            if args.snapshot_test {
                if size.width == 0 || size.height == 0 {
                    return Err("FAIL: empty snapshot".into());
                }
                let nonzero = pixels.as_raw().iter().any(|b| *b != 0);
                if !nonzero {
                    return Err("FAIL: snapshot is all-zero (WebKit did not paint?)".into());
                }
                println!("PASS: snapshot has non-zero pixel data");
            }
            pixels.save(&args.output_path)?;
            println!("wrote {}", args.output_path.display());
        }
        other => {
            return Err(
                format!("FAIL: expected CpuRgba frame, got mode {:?}", other.mode()).into(),
            );
        }
    }
    Ok(())
}

const DRAG_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"></head>
<body style="margin:0">
<div id="zone" style="position:absolute;left:50px;top:50px;width:300px;height:200px;background:#fef3c7;color:#92400e;display:flex;align-items:center;justify-content:center;font:bold 18px system-ui"
     ondragenter="window.chrome.webview.postMessage('dragenter@' + event.clientX + ',' + event.clientY)"
     ondragover="event.preventDefault()"
     ondrop="event.preventDefault(); window.chrome.webview.postMessage('drop@' + event.clientX + ',' + event.clientY)">
drop zone
</div>
</body></html>"#;

const TEXT_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"></head>
<body style="margin:0">
<input id="t" autofocus
       style="position:absolute;left:50px;top:50px;width:300px;height:40px;font:16px system-ui"
       oninput="window.chrome.webview.postMessage('value=' + this.value)">
</body></html>"#;

/// Drag-and-drop forwarding smoke. Loads a page with a drop zone
/// listening for `dragenter` / `drop` events; sends the same trio
/// through `send_drag_input` (Enter → Over → Drop) and asserts the
/// page-side handlers observe both endpoints. JS-synthesis path
/// only — `event.dataTransfer.files` is empty.
fn run_drag_test(
    producer: &mut WebKitGtkProducer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("loading drag-test page");
    producer.navigate_to_string(DRAG_HTML, nav_timeout)?;

    let target = (200, 150);
    let no_mods = MouseVirtualKeys::default();
    println!("sending DragInput::Enter @ {target:?}");
    producer.send_drag_input(DragInput {
        kind: DragEventKind::Enter,
        virtual_keys: no_mods,
        point: target,
        allowed_effects: 0,
    })?;
    match producer
        .wait_for_web_message(Duration::from_secs(3))
        .as_deref()
    {
        Some("dragenter@200,150") => println!("PASS: dragenter reached the drop zone"),
        other => return Err(format!("FAIL: dragenter — got {other:?}").into()),
    }

    // Required for drop to fire — page must preventDefault on dragover.
    producer.send_drag_input(DragInput {
        kind: DragEventKind::Over,
        virtual_keys: no_mods,
        point: target,
        allowed_effects: 0,
    })?;

    println!("sending DragInput::Drop @ {target:?}");
    producer.send_drag_input(DragInput {
        kind: DragEventKind::Drop,
        virtual_keys: no_mods,
        point: target,
        allowed_effects: 0,
    })?;
    match producer
        .wait_for_web_message(Duration::from_secs(3))
        .as_deref()
    {
        Some("drop@200,150") => {
            println!("PASS: drop reached the drop zone");
            Ok(())
        }
        other => Err(format!("FAIL: drop — got {other:?}").into()),
    }
}

/// IME-commit / send_text smoke. Loads a page with an `<input
/// autofocus>` that postMessages its value on every `input` event;
/// pushes the string "hi" through `send_text` and asserts the page
/// observes the accumulated value.
fn run_text_test(
    producer: &mut WebKitGtkProducer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("loading text-test page");
    producer.navigate_to_string(TEXT_HTML, nav_timeout)?;

    // Drain any stale focus events from the autofocus on page load.
    while producer.poll_navigation_event().is_some() {}
    while producer.poll_web_message().is_some() {}

    println!("sending text \"hi\"");
    producer.send_text("hi")?;

    // Expect two messages: "value=h", "value=hi". The page fires
    // `input` after each keystroke updates the field. Drain them and
    // assert the final state.
    let mut last_value: Option<String> = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match producer.wait_for_web_message(Duration::from_millis(200)) {
            Some(msg) => {
                if let Some(rest) = msg.strip_prefix("value=") {
                    last_value = Some(rest.to_string());
                }
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
            }
        }
        if last_value.as_deref() == Some("hi") {
            break;
        }
    }
    match last_value.as_deref() {
        Some("hi") => {
            println!("PASS: send_text round-tripped \"hi\" into the focused input");
            Ok(())
        }
        Some(other) => Err(format!("FAIL: input value ended as {other:?}, expected \"hi\"").into()),
        None => Err("FAIL: input element never observed any keystrokes".into()),
    }
}

const CURSOR_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"></head>
<body style="margin:0">
<a href="https://example.com" id="lnk"
   style="position:absolute;left:100px;top:100px;width:200px;height:50px;
   background:#e0f2fe;color:#075985;display:block;text-align:center;
   line-height:50px;text-decoration:none;font:bold 18px system-ui">hover me</a>
</body></html>"#;

const IME_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"></head>
<body style="margin:0">
<input id="email" type="email" inputmode="email" autocomplete="email"
       autofocus placeholder="email"
       style="position:absolute;left:50px;top:50px;width:300px;height:40px;
       font:16px system-ui">
</body></html>"#;

/// Cursor-shape reporting smoke. Moves the mouse over a styled
/// anchor and asserts `poll_cursor_shape` returns
/// [`CursorShape::Pointer`] — surfaced via
/// `mouse-target-changed`'s hit-test context.
fn run_cursor_test(
    producer: &mut WebKitGtkProducer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("loading cursor-test page");
    producer.navigate_to_string(CURSOR_HTML, nav_timeout)?;

    let no_mods = MouseVirtualKeys::default();
    println!("moving mouse over plain background first to establish baseline");
    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::Move,
        virtual_keys: no_mods,
        mouse_data: 0,
        point: (500, 400),
    })?;

    println!("moving mouse over the link @ (200, 125)");
    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::Move,
        virtual_keys: no_mods,
        mouse_data: 0,
        point: (200, 125),
    })?;
    let shape = producer.wait_for_cursor_shape(Duration::from_secs(3), |s| {
        matches!(s, CursorShape::Pointer)
    });
    match shape {
        Some(CursorShape::Pointer) => {
            println!("PASS: hovering link surfaced CursorShape::Pointer");
            Ok(())
        }
        Some(other) => Err(format!("FAIL: got unexpected cursor shape {other:?}").into()),
        None => Err("FAIL: never observed Pointer cursor".into()),
    }
}

/// IME / focus-tracking smoke. Loads a page with an `<input
/// type="email" autofocus>`, asserts the injected JS focus observer
/// posts `TextInputFocused` with the right element-kind / input-type
/// metadata.
fn run_ime_test(
    producer: &mut WebKitGtkProducer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("loading ime-test page");
    producer.navigate_to_string(IME_HTML, nav_timeout)?;

    let evt = producer.wait_for_navigation_event(Duration::from_secs(3), |e| {
        matches!(e, NavigationEvent::TextInputFocused { .. })
    });
    let state = match evt {
        Some(NavigationEvent::TextInputFocused { state }) => state,
        Some(other) => return Err(format!("FAIL: unexpected event {other:?}").into()),
        None => return Err("FAIL: TextInputFocused never fired".into()),
    };
    println!(
        "TextInputFocused element_kind={} input_type={} input_mode={} autocomplete={}",
        state.element_kind, state.input_type, state.input_mode, state.autocomplete
    );
    if state.element_kind == "input"
        && state.input_type == "email"
        && state.input_mode == "email"
        && state.autocomplete == "email"
    {
        println!("PASS: IME focus state matches the page's <input type=\"email\">");
        Ok(())
    } else {
        Err(format!("FAIL: IME focus state mismatch — {state:?}").into())
    }
}

/// Download lifecycle smoke. Pre-writes a known payload to a temp
/// file, asks WebKit to download it (via `webkit_web_view_download_uri`
/// against the `file://` URL), polls for `DownloadStarted` +
/// `DownloadFinished` events, and verifies the destination file
/// matches.
///
/// `file://` is used (rather than the `scry://` custom scheme) because
/// WebKit's download path bypasses custom URI scheme handlers — those
/// fire on resource-load only. A round-trip through the network
/// process needs a "real" scheme.
fn run_download_test(producer: &WebKitGtkProducer) -> Result<(), Box<dyn std::error::Error>> {
    let payload = b"scrying download payload\n";
    let source_path = std::env::temp_dir().join("scrying-download-source.bin");
    std::fs::write(&source_path, payload)?;
    let download_url = format!("file://{}", source_path.display());
    println!("kicking off download of {download_url}");
    producer.download_url(&download_url)?;

    let started = producer.wait_for_navigation_event(Duration::from_secs(3), |e| {
        matches!(e, NavigationEvent::DownloadStarted { .. })
    });
    let destination = match started {
        Some(NavigationEvent::DownloadStarted {
            id,
            url,
            destination_path,
            ..
        }) => {
            println!("DownloadStarted id={id:?} url={url} dest={destination_path:?}");
            destination_path
        }
        Some(other) => return Err(format!("FAIL: unexpected event {other:?}").into()),
        None => return Err("FAIL: DownloadStarted never fired".into()),
    };

    let finished = producer.wait_for_navigation_event(Duration::from_secs(5), |e| {
        matches!(e, NavigationEvent::DownloadFinished { .. })
    });
    match finished {
        Some(NavigationEvent::DownloadFinished {
            id, error: None, ..
        }) => {
            println!("DownloadFinished id={id:?} (no error)");
        }
        Some(NavigationEvent::DownloadFinished {
            id, error: Some(e), ..
        }) => {
            return Err(format!("FAIL: download id={id:?} reported error {e}").into());
        }
        Some(other) => return Err(format!("FAIL: unexpected event {other:?}").into()),
        None => return Err("FAIL: DownloadFinished never fired".into()),
    }

    // Confirm the bytes round-tripped to the producer's downloads
    // directory.
    let on_disk = std::fs::read(&destination)
        .map_err(|e| format!("FAIL: cannot read {destination:?}: {e}"))?;
    if on_disk == payload {
        println!(
            "PASS: downloaded file has expected payload ({} bytes)",
            on_disk.len()
        );
        Ok(())
    } else {
        Err(format!(
            "FAIL: payload mismatch — got {} bytes, expected {}",
            on_disk.len(),
            payload.len()
        )
        .into())
    }
}

const POPUP_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"></head>
<body>
<a id="lnk" href="https://example.com/popup-target" target="_blank"
   style="position:absolute;left:50px;top:50px;width:300px;height:60px;
   background:#fef3c7;display:block;text-align:center;line-height:60px;
   text-decoration:none;color:#92400e;font:bold 20px system-ui">popup link</a>
</body></html>"#;

/// New-window / popup intercept smoke. Clicks a `target="_blank"`
/// anchor via our native GdkEvent path (isTrusted=true, counts as a
/// user gesture so WebKit doesn't pop-block) and asserts that
/// `connect_create` fires with the popup URL — surfaced as a
/// `NavigationEvent::NewWindowRequested`.
fn run_popup_test(
    producer: &mut WebKitGtkProducer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("loading popup-test page");
    producer.navigate_to_string(POPUP_HTML, nav_timeout)?;

    // Drain any nav events from the load itself so we don't get a
    // stale match from the predicate.
    while let Some(_) = producer.poll_navigation_event() {}

    // Centre of the anchor (x=50..350, y=50..110).
    let target = (200, 80);
    let no_mods = MouseVirtualKeys::default();
    println!("clicking popup anchor @ {target:?}");
    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::LeftButtonDown,
        virtual_keys: no_mods,
        mouse_data: 0,
        point: target,
    })?;
    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::LeftButtonUp,
        virtual_keys: no_mods,
        mouse_data: 0,
        point: target,
    })?;

    let evt = producer.wait_for_navigation_event(Duration::from_secs(3), |e| {
        matches!(e, NavigationEvent::NewWindowRequested { .. })
    });
    match evt {
        Some(NavigationEvent::NewWindowRequested { url })
            if url == "https://example.com/popup-target" =>
        {
            println!("PASS: NewWindowRequested fired with the popup URL");
            Ok(())
        }
        Some(NavigationEvent::NewWindowRequested { url }) => {
            Err(format!("FAIL: unexpected popup URL: {url:?}").into())
        }
        Some(other) => Err(format!("FAIL: unexpected event {other:?}").into()),
        None => Err("FAIL: NewWindowRequested never fired".into()),
    }
}

/// Custom URL scheme smoke. The producer was built with a `scry://`
/// scheme handler that returns an HTML body postMessage-ing the
/// served URI back; navigating to `scry://hello` should result in
/// the host observing that message.
fn run_scheme_test(
    producer: &mut WebKitGtkProducer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("navigating to scry://hello");
    producer.navigate_to_url("scry://hello", nav_timeout)?;
    match producer
        .wait_for_web_message(Duration::from_secs(3))
        .as_deref()
    {
        Some("scheme served: scry://hello") => {
            println!("PASS: scry:// scheme handler served the page");
            Ok(())
        }
        Some(other) => Err(format!("FAIL: unexpected scheme message {other:?}").into()),
        None => Err("FAIL: scheme handler never delivered a page-side message".into()),
    }
}

/// Cookie store round-trip smoke. Sets a cookie, reads it back via
/// `request_cookies_for_url`, asserts the value matches; then
/// deletes and re-reads to confirm absence. No navigation involved
/// — exercises the `WebsiteDataManager` cookie store directly.
fn run_cookie_test(producer: &WebKitGtkProducer) -> Result<(), Box<dyn std::error::Error>> {
    let url = "http://test.local/path";
    let cookie = Cookie {
        name: "scrying_test".to_string(),
        value: "phase2d".to_string(),
        domain: "test.local".to_string(),
        path: "/".to_string(),
        expires_at: None,
        is_secure: false,
        is_http_only: false,
        same_site: None,
        partitioned: false,
    };

    println!("setting cookie scrying_test=phase2d for {url}");
    producer.set_cookie(&cookie)?;

    let cookies = producer.request_cookies_for_url(url)?;
    println!("got {} cookie(s) for {url}", cookies.len());
    match cookies.iter().find(|c| c.name == "scrying_test") {
        Some(c) if c.value == "phase2d" => {
            println!("PASS: cookie round-tripped (name=scrying_test value=phase2d)")
        }
        Some(c) => {
            return Err(format!("FAIL: cookie value differs — got {:?}", c.value).into());
        }
        None => return Err("FAIL: cookie not present after set_cookie".into()),
    }

    println!("deleting cookie");
    producer.delete_cookie(&cookie)?;

    let after = producer.request_cookies_for_url(url)?;
    if after.iter().any(|c| c.name == "scrying_test") {
        return Err("FAIL: cookie still present after delete_cookie".into());
    }
    println!("PASS: cookie absent after delete_cookie");
    Ok(())
}

/// Synthesized input smoke. Loads a page with mouse + keyboard
/// handlers that postMessage back, then drives `send_mouse_input` /
/// `send_keyboard_input` and asserts the page-side listeners observed
/// the synthesized events.
fn run_input_test(
    producer: &mut WebKitGtkProducer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("loading input-test page");
    producer.navigate_to_string(INPUT_HTML, nav_timeout)?;

    // Focus the page so document-level keyboard listeners have a
    // target. `move_focus` is a no-op for handlers attached to
    // `document` itself, but worth calling — it'll matter once we
    // upgrade to native GdkEvent dispatch.
    producer.move_focus(FocusReason::Programmatic)?;

    // Centre of the button (x=100..300, y=100..160).
    let target = (200, 130);
    let no_mods = MouseVirtualKeys::default();

    println!("sending LeftButtonDown @ {target:?}");
    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::LeftButtonDown,
        virtual_keys: no_mods,
        mouse_data: 0,
        point: target,
    })?;
    match producer
        .wait_for_web_message(Duration::from_secs(2))
        .as_deref()
    {
        Some("mousedown@200,130 trusted=true") => {
            println!("PASS: mousedown — isTrusted=true (native GdkEvent path)")
        }
        Some("mousedown@200,130 trusted=false") => {
            println!("PASS (degraded): mousedown — isTrusted=false (JS fallback path)")
        }
        other => return Err(format!("FAIL: mousedown — got {other:?}").into()),
    }

    println!("sending LeftButtonUp @ {target:?}");
    producer.send_mouse_input(MouseInput {
        kind: MouseEventKind::LeftButtonUp,
        virtual_keys: no_mods,
        mouse_data: 0,
        point: target,
    })?;
    match producer
        .wait_for_web_message(Duration::from_secs(2))
        .as_deref()
    {
        Some("mouseup@200,130 trusted=true") => {
            println!("PASS: mouseup — isTrusted=true (native GdkEvent path)")
        }
        Some("mouseup@200,130 trusted=false") => {
            println!("PASS (degraded): mouseup — isTrusted=false (JS fallback path)")
        }
        other => return Err(format!("FAIL: mouseup — got {other:?}").into()),
    }

    println!("sending keydown 'a'");
    producer.send_keyboard_input(KeyboardInput {
        kind: KeyEventKind::Down,
        virtual_key_code: 0x41, // 'A' physical key
        characters: "a".to_string(),
        characters_ignoring_modifiers: "a".to_string(),
        modifiers: KeyModifierFlags::default(),
        is_repeat: false,
    })?;
    match producer
        .wait_for_web_message(Duration::from_secs(2))
        .as_deref()
    {
        Some("keydown:a trusted=true") => {
            println!("PASS: keydown — isTrusted=true (native GdkEvent path)")
        }
        Some("keydown:a trusted=false") => {
            println!("PASS (degraded): keydown — isTrusted=false (JS fallback path)")
        }
        other => return Err(format!("FAIL: keydown — got {other:?}").into()),
    }
    Ok(())
}

/// Bidirectional JS-messaging smoke. The page sends `"hello from page"`
/// at load time; the host then posts `"ping"` and the page echoes
/// `"echo:ping"` back. Both round-trips must complete or the mode
/// fails with a non-zero exit.
fn run_scripted(
    producer: &mut WebKitGtkProducer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("loading scripted page");
    producer.navigate_to_string(SCRIPTED_HTML, nav_timeout)?;

    let msg = producer.wait_for_web_message(Duration::from_secs(3));
    match msg.as_deref() {
        Some("hello from page") => println!("PASS: page → host initial message arrived"),
        Some(other) => {
            return Err(format!("FAIL: expected 'hello from page', got {other:?}").into());
        }
        None => return Err("FAIL: page → host initial message timed out".into()),
    }

    println!("posting 'ping' to page");
    producer.post_web_message("ping")?;

    let echo = producer.wait_for_web_message(Duration::from_secs(3));
    match echo.as_deref() {
        Some("echo:ping") => println!("PASS: host → page round-trip arrived"),
        Some(other) => {
            return Err(format!("FAIL: expected 'echo:ping', got {other:?}").into());
        }
        None => return Err("FAIL: host → page round-trip timed out".into()),
    }
    Ok(())
}

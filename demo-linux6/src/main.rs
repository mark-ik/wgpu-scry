//! Minimal Linux runtime probe for scrying's WebKitGTK 6.0 / GTK 4
//! producer (Phase 5).
//!
//! Sibling to [`demo-linux`] which targets the GTK 3 / WebKitGTK 4.1
//! line. This binary uses the parallel `webkit6_producer` module
//! behind the `webkit6` feature flag on scrying.
//!
//! ```sh
//! cargo run -p demo-linux6                              # default HTML page → snapshot.png
//! cargo run -p demo-linux6 -- --probe-only              # capability probe + exit
//! cargo run -p demo-linux6 -- --snapshot-test           # exit-1 on empty/zero-pixel snapshot
//! cargo run -p demo-linux6 -- --url https://example.com # real-page snapshot
//! cargo run -p demo-linux6 -- --scripted                # host ↔ page postMessage round-trip
//! cargo run -p demo-linux6 -- --cookie-test             # cookie store set / get / delete
//! cargo run -p demo-linux6 -- --scheme-test             # custom myscheme:// → page handler
//! cargo run -p demo-linux6 -- --input-test              # synthesized click → page handler
//! cargo run -p demo-linux6 -- --download-test           # data: URL download lifecycle
//! ```
//!
//! Subcommands deliberately mirror the subset of [`demo-linux`]'s flag
//! surface that the webkit6 producer's current capabilities support
//! (Phase A.1–A.8). Missing analogs vs. demo-linux:
//!
//! - `--cursor-test` / `--ime-test`: both require a real visible
//!   display with hover or focus delivery. Deferred — the producer's
//!   `mouse-target-changed` / `scryIme` plumbing is unit-test covered
//!   in scrying itself; an end-to-end runtime test belongs with a
//!   non-headless harness.
//! - `--popup-test` / `--drag-test` / `--text-test`: input on webkit6
//!   is JS-event synthesis only (no `gtk_main_do_event` in GTK 4), so
//!   the page-side `isTrusted` guard makes popup interception
//!   unreliable; drag is the synthesis-only path the parity matrix
//!   marks ✘ on webkit6; `send_text` doesn't exist on the webkit6
//!   producer.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use dpi::PhysicalSize;
use scrying::webkit6_producer::{WebKit6Producer, WebKit6ProducerConfig};
use scrying::{
    Cookie, KeyEventKind, KeyModifierFlags, KeyboardInput, MouseEventKind, MouseInput,
    MouseVirtualKeys, NavigationEvent, UrlSchemeHandlerFn, UrlSchemeResponse,
    WebSurfaceCapabilities, WebSurfaceFrame, WebSurfaceProducer,
};

const DEFAULT_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>scrying linux6 smoke</title></head>
<body style="margin:0;display:flex;align-items:center;justify-content:center;
height:100vh;background:linear-gradient(135deg,#1e3a8a,#1e293b);color:#a5f3fc;
font:bold 64px system-ui,sans-serif">scrying · linux · gtk4</body></html>"#;

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
    // Same WebKit env-var workaround as `demo-linux` — disables
    // accelerated compositing / DMABUF renderer so GDK doesn't try
    // (and on some Wayland sessions, fail) to create a GL context
    // we don't actually need for CPU snapshot.
    // Safety: env-var writes must happen before any other thread
    // spawns; `main` is single-threaded at this point.
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("demo-linux6: {err}");
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
    download_test: bool,
    width: u32,
    height: u32,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut out = Args {
            output_path: "scrying-linux6-snapshot.png".into(),
            url: None,
            snapshot_test: false,
            probe_only: false,
            scripted: false,
            input_test: false,
            cookie_test: false,
            scheme_test: false,
            download_test: false,
            width: 800,
            height: 600,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--out" => out.output_path = args.next().ok_or("--out needs a path")?.into(),
                "--url" => out.url = Some(args.next().ok_or("--url needs a value")?),
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
                "--download-test" => out.download_test = true,
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
    println!("demo-linux6 — WebKitGTK 6.0 / GTK 4 runtime probe for scrying");
    println!();
    println!("USAGE: demo-linux6 [--url URL] [--out PATH] [--width N] [--height N]");
    println!("                   [--snapshot-test] [--scripted] [--input-test] [--cookie-test]");
    println!("                   [--scheme-test] [--download-test] [--probe-only]");
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let caps = WebSurfaceCapabilities::probe(None);
    println!("backend: {:?}", caps.backend);
    println!("preferred mode: {:?}", caps.preferred_mode);
    println!("CPU snapshot: {:?}", caps.cpu_snapshot);
    println!("reason: {}", caps.reason);
    if args.probe_only {
        return Ok(());
    }

    let data_dir = std::env::temp_dir().join("scrying-demo-linux6-data");
    let config = WebKit6ProducerConfig::new(PhysicalSize::new(args.width, args.height), &data_dir);
    let mut producer = if args.scheme_test {
        let mut schemes: HashMap<String, UrlSchemeHandlerFn> = HashMap::new();
        // The `myscheme://` handler returns a tiny HTML body that
        // postMessages the served URI back to the host via the A.1
        // script-message bridge. Mirrors demo-linux's `scry://` setup.
        schemes.insert(
            "myscheme".to_string(),
            Arc::new(|uri: &str| {
                let body = format!(
                    "<!doctype html><html><body><script>\
                     window.chrome.webview.postMessage('scheme served: {uri}');\
                     </script></body></html>"
                );
                UrlSchemeResponse {
                    mime_type: "text/html".to_string(),
                    body: body.into_bytes(),
                    headers: vec![("X-Scry-Source".to_string(), "demo-linux6".to_string())],
                }
            }),
        );
        WebKit6Producer::new_with_url_schemes(config, schemes)?
    } else {
        WebKit6Producer::new(config)?
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
    if args.download_test {
        return run_download_test(&producer);
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

/// Bidirectional JS-messaging smoke (A.1 script-message bridge). The
/// page sends `"hello from page"` at load time; the host then posts
/// `"ping"` and the page echoes `"echo:ping"` back. Both round-trips
/// must complete or the mode fails with a non-zero exit. Mirrors
/// `demo-linux`'s `--scripted` mode.
fn run_scripted(
    producer: &mut WebKit6Producer,
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

/// Cookie store round-trip smoke (A.2 cookies). Sets a cookie, reads
/// it back via `request_cookies_for_url`, asserts the value matches;
/// then deletes and re-reads to confirm absence. No navigation
/// involved — exercises the `NetworkSession::cookie_manager()` API
/// directly. Mirrors `demo-linux`'s `--cookie-test` mode.
fn run_cookie_test(producer: &WebKit6Producer) -> Result<(), Box<dyn std::error::Error>> {
    let url = "http://test.local/path";
    let cookie = Cookie {
        name: "scrying_test".to_string(),
        value: "phaseA".to_string(),
        domain: "test.local".to_string(),
        path: "/".to_string(),
        expires_at: None,
        is_secure: false,
        is_http_only: false,
    };

    println!("setting cookie scrying_test=phaseA for {url}");
    producer.set_cookie(&cookie)?;

    let cookies = producer.request_cookies_for_url(url)?;
    println!("got {} cookie(s) for {url}", cookies.len());
    match cookies.iter().find(|c| c.name == "scrying_test") {
        Some(c) if c.value == "phaseA" => {
            println!("PASS: cookie round-tripped (name=scrying_test value=phaseA)")
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

/// Custom URL scheme smoke (A.3 scheme handlers). The producer was
/// built with a `myscheme://` scheme handler that returns an HTML body
/// postMessage-ing the served URI back; navigating to
/// `myscheme://test` should result in the host observing that message
/// through the A.1 script-message bridge. Mirrors `demo-linux`'s
/// `--scheme-test` mode (with a renamed scheme to keep the docs honest
/// about which producer is exercised).
fn run_scheme_test(
    producer: &mut WebKit6Producer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("navigating to myscheme://test");
    producer.navigate_to_url("myscheme://test", nav_timeout)?;
    match producer
        .wait_for_web_message(Duration::from_secs(3))
        .as_deref()
    {
        Some("scheme served: myscheme://test") => {
            println!("PASS: myscheme:// handler served the page");
            Ok(())
        }
        Some(other) => Err(format!("FAIL: unexpected scheme message {other:?}").into()),
        None => Err("FAIL: scheme handler never delivered a page-side message".into()),
    }
}

/// Synthesized input smoke (A.7 input forwarding + A.1 script-message
/// bridge for the assertion). Loads a page with mouse + keyboard
/// handlers that postMessage back, then drives `send_mouse_input` /
/// `send_keyboard_input` and asserts the page-side listeners observed
/// the synthesized events.
///
/// **Empirical asymmetry vs. `demo-linux`:** webkit6 ships JS-event
/// synthesis only (GTK 4 dropped `gtk_main_do_event`), so the
/// `isTrusted=true` "native GdkEvent" branch the GTK 3 producer can
/// take doesn't exist here. We expect `trusted=false` exclusively and
/// fail on `trusted=true` rather than degrading to it. Same `?` vs `✔`
/// distinction the parity matrix draws.
fn run_input_test(
    producer: &mut WebKit6Producer,
    nav_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("loading input-test page");
    producer.navigate_to_string(INPUT_HTML, nav_timeout)?;

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
        Some("mousedown@200,130 trusted=false") => {
            println!("PASS: mousedown — isTrusted=false (JS-synthesis path; webkit6 has no native fallback)")
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
        Some("mouseup@200,130 trusted=false") => {
            println!("PASS: mouseup — isTrusted=false (JS-synthesis path)")
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
        Some("keydown:a trusted=false") => {
            println!("PASS: keydown — isTrusted=false (JS-synthesis path)")
        }
        other => return Err(format!("FAIL: keydown — got {other:?}").into()),
    }
    Ok(())
}

/// Download lifecycle smoke (A.6 downloads). Pre-writes a known
/// payload to a temp file, asks WebKit to download it (via
/// `webkit_web_view_download_uri` against the `file://` URL), polls
/// for `DownloadStarted` + `DownloadFinished` events, and verifies the
/// destination file matches.
///
/// `file://` is used (rather than a `myscheme://` custom scheme)
/// because WebKit's download path bypasses custom URI scheme handlers
/// — those fire on resource-load only. A round-trip through the
/// network process needs a "real" scheme. Mirrors the trick
/// `demo-linux`'s `--download-test` uses.
fn run_download_test(producer: &WebKit6Producer) -> Result<(), Box<dyn std::error::Error>> {
    let payload = b"scrying download payload\n";
    let source_path = std::env::temp_dir().join("scrying-download-source-linux6.bin");
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

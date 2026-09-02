//! Minimal winit host for scrying's macOS WKWebView producer.
//!
//! Opens a window, hosts a `WkWebViewProducer` against the window's
//! `NSView`, drives the input / event / snapshot / SCK-capture paths
//! so we can verify the producer works at runtime.
//!
//! See `README.md` for the full description of CLI flags
//! (`--probe-snapshot`, `--scripted`, `--capture`, `--capture --dump-every N`,
//! `--capture --resize-test`).

#![cfg(target_os = "macos")]

mod render;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use render::WgpuRender;
use scrying::CaptureStatus;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use scrying::wkwebview_producer::{
    FindOptions, UrlSchemeHandlerFn, UrlSchemeResponse, WkWebViewProducer, WkWebViewProducerConfig,
};
use scrying::{
    AuthDisposition, Cookie, DownloadDecision, DownloadId, KeyEventKind, KeyModifierFlags,
    KeyboardInput, MouseEventKind, MouseInput, MouseVirtualKeys, NavigationEvent, PointerDevice,
    PointerEventKind, PointerInput, WebSurfaceProducer,
};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::Key;
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::window::{Window, WindowAttributes};

const INITIAL_URL: &str = "https://example.com";

fn demo_data_root() -> std::io::Result<PathBuf> {
    let root = std::env::var_os("SCRY_DEMO_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    if root.is_absolute() {
        Ok(root)
    } else {
        Ok(std::env::current_dir()?.join(root))
    }
}

/// Body served by the `--download-test` HTTP server. 256 KiB of a
/// known repeating pattern. Bumped from 64 KiB so phase E's slow
/// streaming path (8 KiB chunks at 200 ms apart → ~6.4 s total
/// for 256 KiB) gives WebKit enough buffered body bytes to capture
/// resume_data on cancel.
fn download_test_body() -> Vec<u8> {
    const SIZE: usize = 256 * 1024;
    (0..SIZE).map(|i| (i % 251) as u8).collect()
}

/// URLs the loopback HTTP server exposes for `--download-test`.
struct DownloadTestUrls {
    /// Plain download — `Content-Disposition: attachment`, no auth.
    plain: String,
    /// Auth-required download — first request gets a 401 + a
    /// `WWW-Authenticate: Basic` challenge; second request with
    /// `Authorization: Basic <user:pass>` (the test's expected
    /// credentials) gets the body.
    auth_required: String,
    /// Slow download with `Accept-Ranges: bytes` — streams the
    /// body in small chunks with sleeps between them so the test
    /// can cancel mid-transfer and exercise resume. Honors
    /// `Range: bytes=N-` so `WKWebView::resumeDownloadFromResumeData:`
    /// can resume from the offset WebKit captured.
    slow_resumable: String,
}

const DOWNLOAD_AUTH_USER: &str = "scrying-test";
const DOWNLOAD_AUTH_PASS: &str = "open-sesame";

/// Spin up a single-purpose loopback HTTP server that serves the
/// `--download-test` payload at two paths:
///
/// - `/download` — `Content-Disposition: attachment` plain
///   response. WebKit promotes the navigation to a download
///   directly; no auth.
/// - `/download-auth` — same body, but the first request gets a
///   401 + `WWW-Authenticate: Basic realm="scrying-test-realm"`
///   so WebKit fires
///   `WKDownloadDelegate::download:didReceiveAuthenticationChallenge:`.
///   Requests carrying the expected basic-auth header get the body.
///
/// Returns both URLs. The server keeps running for the rest of
/// the process's lifetime; the OS reaps the listener thread on
/// exit.
fn start_download_test_server(body: Vec<u8>) -> std::io::Result<DownloadTestUrls> {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let urls = DownloadTestUrls {
        plain: format!("http://{}/download", addr),
        auth_required: format!("http://{}/download-auth", addr),
        slow_resumable: format!("http://{}/download-slow", addr),
    };
    let body = Arc::new(body);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = Arc::clone(&body);
            std::thread::spawn(move || {
                // Pull the HTTP request: read until we see a blank
                // line (end of headers). 4 KiB cap is plenty for
                // a no-body GET with one Authorization header.
                let mut buf = [0u8; 4096];
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let request = String::from_utf8_lossy(&buf[..n]);

                // Path = first whitespace-delimited token after the method.
                let path = request
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");

                let needs_auth = path.starts_with("/download-auth");
                let has_valid_auth = needs_auth
                    && request.lines().any(|h| {
                        let lower = h.to_ascii_lowercase();
                        lower.starts_with("authorization: basic ") && {
                            // RFC 7617: "Basic <base64(user:pass)>"
                            let token = &h[h.rfind(' ').map(|i| i + 1).unwrap_or(h.len())..];
                            base64_decode(token.trim())
                                .map(|decoded| {
                                    decoded
                                        == format!("{DOWNLOAD_AUTH_USER}:{DOWNLOAD_AUTH_PASS}")
                                            .as_bytes()
                                })
                                .unwrap_or(false)
                        }
                    });

                if needs_auth && !has_valid_auth {
                    let header = "HTTP/1.1 401 Unauthorized\r\n\
                                  WWW-Authenticate: Basic realm=\"scrying-test-realm\"\r\n\
                                  Content-Length: 0\r\n\
                                  Connection: close\r\n\
                                  \r\n";
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.flush();
                    return;
                }

                // The /download-slow path streams in 8 KiB
                // chunks with 50 ms sleeps so the test client
                // can cancel mid-transfer. It also honors
                // `Range: bytes=N-` so a follow-up
                // `resumeDownloadFromResumeData:` from WebKit can
                // continue from where the cancel landed.
                let is_slow = path.starts_with("/download-slow");
                let range_offset = if is_slow {
                    request
                        .lines()
                        .find_map(|l| {
                            let lower = l.to_ascii_lowercase();
                            lower.strip_prefix("range: bytes=").and_then(|rest| {
                                let end = rest.find('-').unwrap_or(rest.len());
                                rest[..end].trim().parse::<u64>().ok()
                            })
                        })
                        .unwrap_or(0) as usize
                } else {
                    0
                };

                let slice = &body[range_offset.min(body.len())..];
                let total_len = body.len();
                // Stable ETag + Last-Modified — WebKit's resume
                // validator pins on these headers when capturing
                // / re-issuing resume requests.
                const ETAG: &str = "\"scrying-test-etag-deadbeef\"";
                const LAST_MODIFIED: &str = "Mon, 01 Jan 2024 00:00:00 GMT";
                let header = if is_slow && range_offset > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\n\
                         Content-Type: application/octet-stream\r\n\
                         Content-Length: {len}\r\n\
                         Content-Range: bytes {start}-{last}/{total}\r\n\
                         Accept-Ranges: bytes\r\n\
                         ETag: {etag}\r\n\
                         Last-Modified: {last_modified}\r\n\
                         Content-Disposition: attachment; filename=\"scrying-download.bin\"\r\n\
                         Connection: close\r\n\
                         \r\n",
                        len = slice.len(),
                        start = range_offset,
                        last = total_len - 1,
                        total = total_len,
                        etag = ETAG,
                        last_modified = LAST_MODIFIED,
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/octet-stream\r\n\
                         Content-Length: {}\r\n\
                         Accept-Ranges: bytes\r\n\
                         ETag: {etag}\r\n\
                         Last-Modified: {last_modified}\r\n\
                         Content-Disposition: attachment; filename=\"scrying-download.bin\"\r\n\
                         Connection: close\r\n\
                         \r\n",
                        slice.len(),
                        etag = ETAG,
                        last_modified = LAST_MODIFIED,
                    )
                };
                let _ = stream.write_all(header.as_bytes());
                if is_slow {
                    // 8 KiB chunks at 200 ms apart → ~1.6 s total
                    // for the 64 KiB body. Slow enough that
                    // WebKit's internal buffering doesn't collapse
                    // every chunk into a single didWriteData
                    // callback, so the test sees Progress events
                    // mid-transfer and can cancel before
                    // Finished fires.
                    for chunk in slice.chunks(8 * 1024) {
                        if stream.write_all(chunk).is_err() {
                            return;
                        }
                        let _ = stream.flush();
                        std::thread::sleep(Duration::from_millis(200));
                    }
                } else {
                    let _ = stream.write_all(slice);
                    let _ = stream.flush();
                }
            });
        }
    });
    Ok(urls)
}

/// Minimal RFC 4648 base64 decoder. Avoids pulling in the `base64`
/// crate just for the auth header check. Returns `None` on
/// invalid input (illegal characters, bad padding).
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in bytes {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// `--browser-test` URL scheme handler. Routes by path within the
/// `scrying-test://` scheme to canned HTML responses that drive the
/// browser-class state machine. Each response embeds known marker
/// text so JS-side and host-side assertions can verify what loaded.
fn browser_test_scheme_handler() -> UrlSchemeHandlerFn {
    Arc::new(|url: &str| -> UrlSchemeResponse {
        let body = if url.contains("/history-1") {
            r#"<!doctype html><body><h1 id="m">history-page-1</h1>
            <script>
              if (window.chrome && window.chrome.webview) {
                window.chrome.webview.postMessage('loaded:history-1');
              }
            </script></body>"#
        } else if url.contains("/history-2") {
            r#"<!doctype html><body><h1 id="m">history-page-2</h1>
            <script>
              if (window.chrome && window.chrome.webview) {
                window.chrome.webview.postMessage('loaded:history-2');
              }
            </script></body>"#
        } else if url.contains("/find-target") {
            r#"<!doctype html><body>
            <p>scrying-find-marker</p>
            <script>
              if (window.chrome && window.chrome.webview) {
                window.chrome.webview.postMessage('loaded:find-target');
              }
            </script>
            </body>"#
        } else if url.contains("/download") {
            // Download-test payload. `Content-Disposition: attachment`
            // is the canonical signal that promotes the navigation
            // to a download via
            // `webView:navigationResponse:didBecomeDownload:`. The
            // body is a known-size pattern so the host-side test
            // can verify what landed on disk.
            return UrlSchemeResponse {
                mime_type: "application/octet-stream".into(),
                body: download_test_body(),
                headers: Vec::new(),
            }
            .with_header(
                "Content-Disposition",
                "attachment; filename=\"scrying-download.bin\"",
            );
        } else if url.contains("/pointer") {
            // Pointer-event observer page. Captures pointerdown /
            // pointermove / pointerup / pointerleave on the document
            // and posts back a one-line summary per event so the
            // host can assert which kinds arrived. The full-window
            // styling guarantees the document element is the
            // synthesized event's target regardless of where the
            // (x, y) lands in the WKWebView's frame.
            r#"<!doctype html>
            <html><head><style>html, body { margin: 0; height: 100vh; width: 100vw; }</style></head>
            <body>
            <script>
              function send(kind, e) {
                if (window.chrome && window.chrome.webview) {
                  var msg = 'ptr:' + kind
                    + ':' + Math.round(e.clientX) + ',' + Math.round(e.clientY)
                    + ':' + (e.pointerType || 'unknown');
                  window.chrome.webview.postMessage(msg);
                }
              }
              document.addEventListener('pointerdown', function(e) { send('down', e); });
              document.addEventListener('pointermove', function(e) { send('move', e); });
              document.addEventListener('pointerup',   function(e) { send('up',   e); });
              document.addEventListener('pointerleave',function(e) { send('leave', e); });
              if (window.chrome && window.chrome.webview) {
                window.chrome.webview.postMessage('ready');
              }
            </script>
            </body></html>"#
        } else {
            r#"<!doctype html><body>scrying-test fallback</body>"#
        };
        UrlSchemeResponse {
            mime_type: "text/html".into(),
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    })
}

/// Offline HTML page used by `--scripted`. Contains an input box
/// (id=`text`) so synthetic key events can change the value, a
/// scrollable region so synthetic scroll-wheel events change
/// `window.scrollY`, and a `chrome.webview` listener that echoes any
/// message it receives back as `echo:<payload>` for round-trip
/// verification.
const SCRIPTED_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>scrying-test</title></head>
<body style="margin:0;font-family:monospace;background:#1c1c1c;color:#f0f0f0;">
<h2 style="margin:8px;">scrying scripted-input test</h2>
<div style="margin:8px;">
  <input id="text" placeholder="type here"
         style="font-size:18px;width:60%;padding:6px;background:#2a2a2a;color:#f0f0f0;border:1px solid #555;"
         autofocus />
</div>
<div id="status" style="margin:8px;padding:8px;background:#333;">status: idle</div>
<div id="scroll-track" style="margin:8px;padding:8px;background:#444;">scroll-y: 0</div>
<div style="height:200vh;background:linear-gradient(#222,#888);
            display:flex;align-items:center;justify-content:center;color:#000;
            font-size:24px;">scrollable region</div>
<script>
(function() {
  var statusEl = document.getElementById('status');
  var scrollEl = document.getElementById('scroll-track');
  var textEl = document.getElementById('text');
  var post = function(msg) {
    if (window.chrome && window.chrome.webview) {
      window.chrome.webview.postMessage(msg);
    }
  };
  textEl.addEventListener('input', function() {
    statusEl.textContent = 'status: typed=' + textEl.value;
    post('typed:' + textEl.value);
  });
  window.addEventListener('scroll', function() {
    var y = Math.round(window.scrollY);
    scrollEl.textContent = 'scroll-y: ' + y;
    post('scrolled:' + y);
  });
  if (window.chrome && window.chrome.webview) {
    window.chrome.webview.addEventListener('message', function(e) {
      statusEl.textContent = 'status: host-said=' + e.data;
      post('echo:' + e.data);
    });
  }
  post('ready');
})();
</script>
</body></html>"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::from_args(std::env::args());
    // Headless test runs use `Prohibited` so the test process
    // doesn't claim the active-app slot or register a Dock icon —
    // important so the developer's frontmost app keeps focus during
    // a `bash scripts/test-mac.sh` and so CI macOS runners don't
    // open a visible window mid-test. Visible runs use the
    // `Regular` default so the demo behaves like a normal app.
    let mut event_loop_builder = EventLoop::builder();
    if cli.is_headless() {
        event_loop_builder.with_activation_policy(ActivationPolicy::Prohibited);
    }
    let event_loop = event_loop_builder.build()?;
    // Probe-snapshot, capture, and scripted modes need `Poll` so
    // `about_to_wait` fires regularly enough to advance their state
    // machines / request redraws. Plain overlay mode can sleep on
    // `Wait`.
    if cli.probe_snapshot
        || cli.capture
        || cli.scripted
        || cli.profile_test
        || cli.two_tabs
        || cli.browser_test
        || cli.interaction_state_test
        || cli.pointer_input_test
        || cli.incognito_test
        || cli.download_test
    {
        event_loop.set_control_flow(ControlFlow::Poll);
    } else {
        event_loop.set_control_flow(ControlFlow::Wait);
    }
    let mut app = App { cli, state: None };
    Ok(event_loop.run_app(&mut app)?)
}

#[derive(Clone, Copy, Default)]
struct Cli {
    probe_snapshot: bool,
    /// Run the demo in capture mode: positions the WKWebView at the
    /// left half of the window, sets up wgpu rendering of the
    /// imported texture in the rest of the surface, and kicks off
    /// `start_capture_async` shortly after launch.
    capture: bool,
    /// Run the autonomous scripted-input mode: loads an offline HTML
    /// page with known elements (input box + JS message listener +
    /// scrollable region), drives `post_web_message`, scroll-wheel
    /// events, and synthetic keyboard input on a timed schedule, and
    /// asserts that the JS side observes the expected effects via
    /// `poll_web_message`.
    scripted: bool,
    /// In capture mode, dump every Nth imported texture to
    /// `demo-mac-frame-NNNN.png` via wgpu readback. Default `0`
    /// means "no dumps." Use `--dump-every 30` to dump twice per
    /// second at 60 FPS.
    dump_every: u64,
    /// In capture mode, override the SCK pipeline's color
    /// configuration. Accepts `srgb` (default), `p3`
    /// (`ColorPipeline::DisplayP3`, 8-bit BGRA + Display P3
    /// gamut), or `hdr` (`ColorPipeline::Hdr16f`, 16-bit float
    /// RGBA + extended-linear-DisplayP3). Visual difference vs
    /// `srgb` requires a P3 / HDR-capable display *and* the
    /// demo's wgpu surface configured for the matching format
    /// (the demo's surface is currently `Bgra8Unorm`, so HDR
    /// over-bright values clamp at present; use cadence-probe
    /// metrics + the `--dump-every` PNG dumps to verify the
    /// pipeline is alive end-to-end).
    color_pipeline: scrying::ColorPipeline,
    /// In capture mode, programmatically resize the window mid-run
    /// to exercise slice N's `SCStream::updateConfiguration:` path.
    /// The window cycles 1024→1280→1024 over the run.
    resize_test: bool,
    /// Run the cookie / per-profile-data-store persistence test.
    /// Uses `target/demo-mac-profile-test` as the data dir (so it
    /// doesn't collide with the regular demo profile). Loads an
    /// inline test page with a stable base URL, reads
    /// `document.cookie`, and either reports the existing cookie
    /// (proving persistence from a prior run) or sets a fresh one
    /// (priming for the next run). Exits as soon as the JS handshake
    /// completes.
    profile_test: bool,
    /// Construct two `WkWebViewProducer` instances against the same
    /// NSView, navigate each to a different URL, drain events from
    /// both, and exit cleanly. Validates the producer is safe to
    /// instantiate multiple times in one process — the foundational
    /// architectural requirement for browser-shape consumers (each
    /// tab is its own producer).
    two_tabs: bool,
    /// Drive items 1, 3, 4, 9 of the browser-class roadmap (history
    /// controls, settings, custom URL schemes, find / PDF) on a
    /// timed schedule and assert deterministic effects. Items 2, 5,
    /// 6, 8 need network or harder triggers and aren't covered.
    browser_test: bool,
    /// Round-trip `serialize_interaction_state` / `restore_interaction_state`.
    /// Loads three pages (A → B → C), serializes, navigates back to
    /// A, restores, and asserts the WebView ends up at C with the
    /// full A–B–C back-forward history intact.
    interaction_state_test: bool,
    /// Synthesizes pointer events (Down → Update → Up → Leave) and
    /// asserts the JS-side `pointerdown` / `pointermove` /
    /// `pointerup` / `pointerleave` listeners observe each one.
    /// Verifies `send_pointer_input` reaches the WKWebView and
    /// drives Pointer Events on the JS side.
    pointer_input_test: bool,
    /// Stand up two producers in one process: one with
    /// `non_persistent = true`, one persistent at a separate
    /// `data_dir`. Sets a uniquely-named cookie on the incognito
    /// producer and asserts the persistent producer's cookie store
    /// does not see it — proves the incognito flag really wires
    /// `WKWebsiteDataStore::nonPersistentDataStore` and that
    /// non-persistent stores don't leak into persistent ones.
    incognito_test: bool,
    /// Drive the downloads pipeline (browser-class item 8) end to
    /// end: serves an octet-stream payload via the
    /// `scrying-test://download` scheme, observes
    /// `DownloadStarted` / `DownloadProgress` / `DownloadFinished`
    /// events with non-empty IDs and paths, verifies the bytes on
    /// disk, then exercises `set_download_handler` returning
    /// `Cancel` and `cancel_download(unknown_id)`.
    download_test: bool,
    /// Smoke-test the SCK capture pipeline: navigates to a known
    /// page, kicks off `start_capture_async`, polls
    /// `capture_status` until `Live`, acquires several frames via
    /// `try_acquire_frame`, and asserts each frame's size matches
    /// the configured capture region.
    ///
    /// Requires Screen Recording permission. Held out of the
    /// default `scripts/test-mac.sh` runner because permission
    /// can't be granted from inside the test process — CI must
    /// pre-grant via tccutil. Run manually: `cargo run -p
    /// demo-mac -- --capture-test`.
    capture_test: bool,
    /// Force the demo window to remain visible even when the test
    /// mode would normally run headless. Useful for debugging a
    /// failing test by watching the WKWebView in real time.
    visible: bool,
}

impl Cli {
    /// True when this CLI configuration runs without a visible
    /// window or a focus-stealing Dock-app activation. All
    /// assertion-style \`--*-test\` modes default to headless so
    /// they don't disrupt the developer's session and can run on
    /// CI without a Dock-icon-flash; \`--visible\` overrides.
    fn is_headless(self) -> bool {
        if self.visible {
            return false;
        }
        self.scripted
            || self.browser_test
            || self.interaction_state_test
            || self.pointer_input_test
            || self.incognito_test
            || self.download_test
    }
}

impl Cli {
    fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut cli = Cli::default();
        let mut iter = args.into_iter().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--probe-snapshot" => cli.probe_snapshot = true,
                "--capture" => cli.capture = true,
                "--scripted" => cli.scripted = true,
                "--dump-every" => {
                    let value = iter.next().unwrap_or_default();
                    cli.dump_every = value.parse().unwrap_or_else(|_| {
                        eprintln!("demo-mac: --dump-every needs a positive integer, got '{value}'");
                        0
                    });
                }
                "--color-pipeline" => {
                    let value = iter.next().unwrap_or_default();
                    cli.color_pipeline = match value.as_str() {
                        "srgb" => scrying::ColorPipeline::Srgb,
                        "p3" | "displayp3" | "display-p3" => scrying::ColorPipeline::DisplayP3,
                        "hdr" | "hdr16f" => scrying::ColorPipeline::Hdr16f,
                        other => {
                            eprintln!(
                                "demo-mac: --color-pipeline expects srgb|p3|hdr, got '{other}' — keeping default (srgb)"
                            );
                            scrying::ColorPipeline::Srgb
                        }
                    };
                }
                "--resize-test" => cli.resize_test = true,
                "--profile-test" => cli.profile_test = true,
                "--two-tabs" => cli.two_tabs = true,
                "--browser-test" => cli.browser_test = true,
                "--interaction-state-test" => cli.interaction_state_test = true,
                "--pointer-input-test" => cli.pointer_input_test = true,
                "--incognito-test" => cli.incognito_test = true,
                "--download-test" => cli.download_test = true,
                "--capture-test" => {
                    // Implies --capture so the existing capture
                    // setup (WgpuRender + half-window webview +
                    // capture_kickoff_at) lights up. SCK needs a
                    // visible window so we also force --visible
                    // even though this is a *_test mode.
                    cli.capture_test = true;
                    cli.capture = true;
                    cli.visible = true;
                }
                "--visible" => cli.visible = true,
                _ => eprintln!("demo-mac: unknown arg: {arg}"),
            }
        }
        cli
    }
}

struct App {
    cli: Cli,
    state: Option<AppState>,
}

struct AppState {
    window: Arc<Window>,
    producer: WkWebViewProducer,
    render: Option<WgpuRender>,
    capture_kickoff_at: Option<Duration>,
    capture_started: bool,
    /// If `Some`, indices into the resize cycle ((width, height),
    /// elapsed-time-trigger). Step 0 fires at first trigger, etc.
    resize_test_steps: Option<Vec<(u32, u32, Duration)>>,
    resize_test_idx: usize,
    cursor: Option<PhysicalPosition<f64>>,
    mouse_buttons: MouseVirtualKeys,
    modifiers: KeyModifierFlags,
    /// Set by `--probe-snapshot` mode. The probe schedules
    /// `request_snapshot` once `started_at + delay` elapses, then
    /// drains the result in `about_to_wait` and exits.
    probe: Option<ProbeState>,
    /// Set by `--scripted`. The autonomous test driver — fires
    /// post_web_message / scroll / keyboard at scheduled offsets,
    /// asserts on the JS-echoed responses, and exits.
    scripted: Option<ScriptedState>,
    /// Set by `--profile-test`. Loads the cookie test page once and
    /// reports observed cookie state.
    profile_test: Option<ProfileTestState>,
    /// Set by `--two-tabs`. The second producer instance, navigated
    /// independently from `producer`. Both share the same NSView
    /// parent (subviews of the host window) and the same data_dir
    /// (so they're "tabs in the same browsing session").
    second_producer: Option<WkWebViewProducer>,
    /// Two-tabs mode exits at this elapsed time. `None` outside
    /// `--two-tabs`.
    two_tabs_deadline: Option<Duration>,
    /// Set by `--browser-test`. Drives a state machine across items
    /// 1, 3, 4, 9.
    browser_test: Option<BrowserTestState>,
    /// Set by `--interaction-state-test`. Drives the
    /// serialize → mutate-history → restore → assert round-trip.
    interaction_state_test: Option<InteractionStateTestState>,
    /// Set by `--pointer-input-test`. Drives pointer-event
    /// synthesis and asserts JS-side observation.
    pointer_input_test: Option<PointerInputTestState>,
    /// Set by `--incognito-test`. Drives the two-producer
    /// non_persistent-isolation assertion.
    incognito_test: Option<IncognitoTestState>,
    /// Set by `--download-test`. Drives the three-phase
    /// download-pipeline assertion.
    download_test: Option<DownloadTestState>,
    /// Set by `--capture-test`. Counts SCK frames and asserts every requested
    /// capture size. Pairing it with `--resize-test` turns the old open-ended
    /// resize demo into a bounded ScreenCaptureKit reconfiguration gate.
    capture_test: Option<CaptureTestState>,
    /// Set when `--two-tabs` is invoked. Per-tab URL log used at
    /// deadline to assert each producer's nav events stayed in
    /// its own queue (no cross-talk).
    two_tabs_test: Option<TwoTabsTestState>,
    started_at: Instant,
}

#[derive(Default)]
struct BrowserTestState {
    step: BrowserTestStep,
    step_started_at: Option<Instant>,
    settings_ok: Option<bool>,
    find_result: Option<bool>,
    pdf_bytes: Option<usize>,
    pdf_error: Option<String>,
    /// Most recent page marker observed through the page's
    /// `loaded:*` web message. Unlike `SourceChanged`, this proves
    /// the document script ran before the test issues its next
    /// navigation or browser command.
    last_loaded_page: String,
    /// Most recent URL observed through a completed navigation.
    /// Back/forward restores may come from WebKit's page cache and
    /// therefore do not rerun the page's `loaded:*` script.
    last_completed_url: String,
    failures: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum BrowserTestStep {
    #[default]
    LoadFirst,
    AwaitFirst,
    LoadSecond,
    AwaitSecond,
    GoBack,
    AwaitBack,
    GoForward,
    AwaitForward,
    ApplySettings,
    NavigateForFind,
    AwaitFindPage,
    FindInPage,
    AwaitFindResult,
    RequestPdf,
    AwaitPdfResult,
    Done,
}

#[derive(Default)]
struct InteractionStateTestState {
    step: InteractionStateStep,
    step_started_at: Option<Instant>,
    /// Bytes captured by `serialize_interaction_state`. Restored
    /// later in the run to verify round-trip correctness.
    serialized: Option<Vec<u8>>,
    /// Most recent URL observed via a `Completed` event. Stricter
    /// than `SourceChanged` — only fires once WebKit has fully
    /// committed the navigation into its back-forward list, so the
    /// test can fire the next `load_url` without racing the
    /// previous nav out of the history stack.
    last_completed_url: String,
    failures: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum InteractionStateStep {
    #[default]
    LoadA,
    AwaitA,
    LoadB,
    AwaitB,
    LoadC,
    AwaitC,
    Serialize,
    GoBackOne,
    AwaitBackOne,
    GoBackTwo,
    AwaitBackTwo,
    Restore,
    AwaitRestore,
    Verify,
    Done,
}

#[derive(Default)]
struct TwoTabsTestState {
    tab1_urls: Vec<String>,
    tab2_urls: Vec<String>,
}

/// Helper that pulls the URL out of any `NavigationEvent` variant
/// that carries one. Used by both `--two-tabs` cross-talk
/// accumulation and any other test that wants per-event URL
/// observation.
fn nav_event_url(event: &NavigationEvent) -> Option<String> {
    match event {
        NavigationEvent::Starting { url }
        | NavigationEvent::SourceChanged { url }
        | NavigationEvent::Completed { url, .. }
        | NavigationEvent::NewWindowRequested { url }
        | NavigationEvent::AuthChallenged { url, .. } => Some(url.clone()),
        _ => None,
    }
}

struct CaptureTestState {
    /// Frames received via `try_acquire_frame` so far. Each entry
    /// is the frame's reported `(width, height)` so the test can
    /// assert dims match the configured webview size.
    frame_dims: Vec<(u32, u32)>,
    /// Every capture dimension the bounded run must observe. A plain
    /// `--capture-test` has one entry; `--capture-test --resize-test` carries
    /// the initial size plus each scheduled half-window capture size.
    expected_dims: Vec<(u32, u32)>,
    /// Number of frames required at each expected size. Requiring several
    /// samples keeps a one-off stale transition frame from becoming a receipt.
    frames_per_size: usize,
    /// Set when `start_capture_async` is first attempted. The startup
    /// deadline is measured from this point rather than app construction.
    capture_started_at: Option<Instant>,
    /// Set when the SCK pipeline first goes `Live`. Frame sampling and
    /// capture-test resize schedules are measured from this point so macOS
    /// permission/startup latency cannot consume their observation windows.
    live_started_at: Option<Instant>,
    /// Final pass/fail accumulator.
    failures: Vec<String>,
}

#[derive(Default)]
struct DownloadTestState {
    step: DownloadStep,
    step_started_at: Option<Instant>,
    /// HTTP URL the loopback server serves the plain (no-auth)
    /// download from. Set in `AppState::new` after
    /// `start_download_test_server` returns.
    download_url: String,
    /// HTTP URL that requires basic auth. Phase D loads this and
    /// expects the registered auth handler to supply credentials.
    download_auth_url: String,
    /// HTTP URL that streams the body slowly with `Accept-Ranges:
    /// bytes` so phase E can cancel mid-transfer and exercise
    /// resume.
    download_slow_url: String,
    /// `DownloadStarted` events seen so far, keyed by ID.
    started: HashMap<DownloadId, StartedRecord>,
    /// Set of download IDs that have received a `DownloadProgress` event.
    progress_seen: HashSet<DownloadId>,
    /// `DownloadFinished` events keyed by ID. Records `Some(error)` on
    /// failure, `None` on clean completion.
    finished: HashMap<DownloadId, Option<String>>,
    /// `DownloadCancelled` event IDs, mapped to their resume_data
    /// (if any). Phase E uses the bytes to drive `resume_download`.
    cancelled: HashMap<DownloadId, Option<Vec<u8>>>,
    /// Counters across the three sub-phases for the final summary.
    phase_a_id: Option<DownloadId>,
    phase_b_id: Option<DownloadId>,
    /// Result of `cancel_download(unknown_id)`.
    phase_c_unknown_returned_false: bool,
    /// Set when an `AuthChallenged` event fires with a non-empty
    /// URL — the page-level auth callback was invoked.
    page_level_auth_seen: bool,
    /// Set when an `AuthChallenged` event fires with an empty
    /// URL — the download-level auth callback was invoked.
    /// Phase D's `start_download` path is the only flow that
    /// reaches the download-level callback (HTTP basic-auth
    /// challenges via `load_url` get caught at the page level
    /// before promotion).
    download_level_auth_seen: bool,
    /// Phase E (resume): the in-flight slow download we cancel
    /// to obtain resume_data.
    phase_e_id: Option<DownloadId>,
    /// Phase E: the resumed download's id (newly allocated when
    /// `resumeDownloadFromResumeData:` runs `decideDestination`).
    phase_e_resume_id: Option<DownloadId>,
    failures: Vec<String>,
}

struct StartedRecord {
    destination_path: PathBuf,
    /// Captured for diagnostic logging; not asserted on directly
    /// because the scrying-test scheme handler fills
    /// `expectedContentLength` from its NSData length so the value
    /// is trivially correct.
    _total_bytes_expected: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum DownloadStep {
    #[default]
    PhaseALoad,
    PhaseAAwaitFinish,
    PhaseAVerifyFile,
    PhaseBInstallCancelHandler,
    PhaseBLoad,
    PhaseBAwaitCancel,
    PhaseCCancelUnknown,
    PhaseDInstallAuthHandler,
    PhaseDLoad,
    PhaseDAwaitFinish,
    PhaseELoad,
    PhaseEAwaitProgress,
    PhaseECancel,
    PhaseEAwaitCancel,
    PhaseEResume,
    PhaseEAwaitResume,
    PhaseEVerify,
    Done,
}

#[derive(Default)]
struct IncognitoTestState {
    step: IncognitoStep,
    step_started_at: Option<Instant>,
    /// Cookie name we set on the incognito producer; uniquely
    /// suffixed per run so stale data in any persistent store
    /// from a prior run can't false-positive the assertion.
    cookie_name: String,
    /// `request_all_cookies` result from the incognito producer.
    /// Assertion: should contain `cookie_name`.
    incognito_cookies: Option<Vec<Cookie>>,
    /// `request_all_cookies` result from the persistent producer.
    /// Assertion: should NOT contain `cookie_name`.
    persistent_cookies: Option<Vec<Cookie>>,
    failures: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum IncognitoStep {
    #[default]
    SetCookie,
    AwaitSet,
    QueryIncognito,
    AwaitQueryIncognito,
    QueryPersistent,
    AwaitQueryPersistent,
    Verify,
    Done,
}

#[derive(Default)]
struct PointerInputTestState {
    step: PointerInputStep,
    step_started_at: Option<Instant>,
    /// Page handshake — set true when JS posts "ready".
    saw_ready: bool,
    /// Set true when JS posts a "ptr:down:*" message.
    saw_down: bool,
    /// Set true when JS posts at least one "ptr:move:*" message.
    saw_move: bool,
    /// Set true when JS posts a "ptr:up:*" message.
    saw_up: bool,
    /// Set true when JS posts a "ptr:leave:*" message.
    saw_leave: bool,
    /// `pointerType` field WebKit reported for the synthesized
    /// events (recorded so we can document the WebKit-side mapping
    /// — the producer collapses every device to mouse, so this
    /// should be "mouse").
    observed_pointer_type: Option<String>,
    failures: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum PointerInputStep {
    #[default]
    LoadPage,
    AwaitReady,
    SendDown,
    SendMove,
    SendUp,
    SendLeave,
    Verify,
    Done,
}

#[derive(Default)]
struct ProfileTestState {
    step: ProfileStep,
    step_started_at: Option<Instant>,
    /// Cookie name we set on producer #1 — uniquely suffixed so a
    /// stray entry from a prior run can't satisfy the assertion.
    cookie_name: String,
    /// Cookies observed on producer #1.
    producer1_cookies: Option<Vec<Cookie>>,
    /// Cookies observed on producer #2 (the persistence
    /// counterpart at the same `data_dir`).
    producer2_cookies: Option<Vec<Cookie>>,
    failures: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ProfileStep {
    #[default]
    SetCookie,
    AwaitSet,
    QueryProducer1,
    AwaitQuery1,
    QueryProducer2,
    AwaitQuery2,
    Verify,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScriptedStep {
    Initial,
    AwaitReady,
    PostMessage,
    AwaitEcho,
    Scroll,
    AwaitScroll,
    FocusForKeys,
    SendKeys,
    AwaitTyped,
    Done,
}

struct ScriptedState {
    step: ScriptedStep,
    step_started_at: Instant,
    /// Subset of expectations seen so far. Used to advance the state
    /// machine when a JS-side message arrives.
    saw_ready: bool,
    saw_echo: bool,
    saw_scroll: bool,
    saw_typed: String,
    /// What we typed via send_keyboard_input, expected to round-trip
    /// back as `typed:<accumulated>` from the JS input listener.
    typed_expected: String,
    /// Number of times the registered `set_cursor_handler` callback
    /// has fired. Asserted >= 1 at the end so the push-model API
    /// gets the same runtime coverage as the pull-model
    /// `poll_cursor_shape` queue.
    cursor_handler_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Pass / fail summary printed at exit.
    failures: Vec<String>,
}

impl ScriptedState {
    fn new() -> Self {
        Self {
            step: ScriptedStep::Initial,
            step_started_at: Instant::now(),
            saw_ready: false,
            saw_echo: false,
            saw_scroll: false,
            saw_typed: String::new(),
            cursor_handler_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            typed_expected: String::new(),
            failures: Vec::new(),
        }
    }
    fn enter(&mut self, step: ScriptedStep) {
        self.step = step;
        self.step_started_at = Instant::now();
    }
}

#[derive(Clone, Copy)]
struct ProbeState {
    requested: bool,
    request_at: Duration,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        match AppState::new(event_loop, self.cli) {
            Ok(state) => self.state = Some(state),
            Err(error) => {
                eprintln!("demo-mac: initialization failed: {error}");
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // Drain any messages that arrived since the last wakeup
        // before advancing test-mode state machines.
        if state.scripted.is_some()
            || state.profile_test.is_some()
            || state.browser_test.is_some()
            || state.interaction_state_test.is_some()
            || state.pointer_input_test.is_some()
            || state.download_test.is_some()
            || state.two_tabs_test.is_some()
        {
            drain_events(state);
        }
        if state.scripted.is_some() {
            advance_scripted(state, event_loop);
            drain_events(state);
        }
        if state.profile_test.is_some() {
            advance_profile_test(state, event_loop);
        }
        if state.browser_test.is_some() {
            advance_browser_test(state, event_loop);
        }
        if state.interaction_state_test.is_some() {
            advance_interaction_state_test(state, event_loop);
        }
        if state.pointer_input_test.is_some() {
            advance_pointer_input_test(state, event_loop);
        }
        if state.incognito_test.is_some() {
            advance_incognito_test(state, event_loop);
        }
        if state.download_test.is_some() {
            advance_download_test(state, event_loop);
        }
        // Drain second-producer events on every tick so a
        // visible (`--visible`) run sees tab 2's nav / context-menu
        // / JS-bridge messages even after we suppress the auto-exit
        // deadline. The headless test path also depends on this
        // drain to feed the cross-talk assertion's tab-2 url
        // accumulator.
        if let Some(second) = state.second_producer.as_mut() {
            while let Some(event) = second.poll_navigation_event() {
                println!("demo-mac: [tab2] nav event: {event:?}");
                if let Some(test) = state.two_tabs_test.as_mut() {
                    if let Some(url) = nav_event_url(&event) {
                        test.tab2_urls.push(url);
                    }
                }
            }
            while let Some(message) = second.poll_web_message() {
                println!("demo-mac: [tab2] js->host: {message}");
            }
        }
        if let Some(deadline) = state.two_tabs_deadline {
            if state.started_at.elapsed() >= deadline {
                if state.two_tabs_test.is_some() {
                    finalize_two_tabs_test(state, event_loop);
                } else {
                    println!("demo-mac: --two-tabs deadline reached, exiting");
                    event_loop.exit();
                }
            }
        }
        // Capture mode: kick off `start_capture_async` after the
        // kickoff delay elapses (so the initial navigation has time
        // to land first), then poll capture_status and drive
        // redraws once the stream is live.
        if let Some(kickoff) = state.capture_kickoff_at {
            if !state.capture_started
                && state.started_at.elapsed() >= kickoff
                && let Some(render) = state.render.as_ref()
            {
                if let Some(test) = state.capture_test.as_mut() {
                    test.capture_started_at.get_or_insert_with(Instant::now);
                }
                let host = render.host_context.clone();
                match state.producer.start_capture_async(host) {
                    Ok(()) => {
                        println!("demo-mac: start_capture_async kicked off");
                        state.capture_started = true;
                    }
                    Err(error) => {
                        eprintln!("demo-mac: start_capture_async failed: {error}");
                        state.capture_started = true;
                    }
                }
            }
            // Resize-test driver: programmatically resize the
            // window once each schedule step elapses. This exercises
            // both the producer's `resize` path and slice N's live
            // `SCStream::updateConfiguration:` callback.
            if let Some(steps) = state.resize_test_steps.as_ref() {
                let elapsed = state.capture_test.as_ref().map_or_else(
                    || Some(state.started_at.elapsed()),
                    |test| test.live_started_at.map(|started_at| started_at.elapsed()),
                );
                if let Some(elapsed) = elapsed
                    && state.resize_test_idx < steps.len()
                {
                    let (w, h, at) = steps[state.resize_test_idx];
                    if elapsed >= at {
                        let new_size = winit::dpi::PhysicalSize::new(w, h);
                        let _ = state.window.request_inner_size(new_size);
                        println!(
                            "demo-mac: resize-test step {} → request inner_size = {}x{}",
                            state.resize_test_idx, w, h
                        );
                        state.resize_test_idx += 1;
                    }
                }
            }
            if state.capture_started && state.render.is_some() {
                match state.producer.capture_status() {
                    CaptureStatus::Live => {
                        if state.capture_test.is_some() {
                            // --capture-test is the sole frame
                            // consumer when active. Skip the
                            // wgpu redraw path so the render
                            // loop's `try_acquire_frame` doesn't
                            // race the test driver for the
                            // latest-sample slot — every other
                            // frame would otherwise land at one
                            // path or the other depending on
                            // tick scheduling.
                            advance_capture_test(state, event_loop);
                        } else {
                            // Live: request a redraw to drive the wgpu loop.
                            state.window.request_redraw();
                        }
                    }
                    CaptureStatus::Failed(msg) => {
                        eprintln!("demo-mac: capture failed: {msg}");
                        if let Some(test) = state.capture_test.as_mut() {
                            test.failures.push(format!(
                                "capture_status reported Failed: {msg} (Screen Recording permission?)"
                            ));
                            finalize_capture_test(state, event_loop);
                            return;
                        }
                        event_loop.exit();
                        return;
                    }
                    CaptureStatus::Starting | CaptureStatus::Idle => {
                        // Still spinning up; keep polling.
                        if let Some(test) = state.capture_test.as_ref() {
                            // Bail on a startup deadline — Screen
                            // Recording permission failures often
                            // surface as "stuck in Starting"
                            // rather than an immediate `Failed`.
                            if test.capture_started_at.is_some_and(|started_at| {
                                started_at.elapsed() > Duration::from_secs(15)
                            }) {
                                eprintln!(
                                    "demo-mac: capture-test: capture_status stuck in Starting/Idle for 15s — Screen Recording permission likely missing"
                                );
                                if let Some(test) = state.capture_test.as_mut() {
                                    test.failures.push(
                                        "capture never reached Live (Screen Recording permission?)"
                                            .into(),
                                    );
                                }
                                finalize_capture_test(state, event_loop);
                                return;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Probe-snapshot pump: once `request_at` elapses we kick off
        // `request_snapshot`. Subsequent ticks drain the result.
        if let Some(probe) = state.probe.as_mut() {
            let elapsed = state.started_at.elapsed();
            if !probe.requested && elapsed >= probe.request_at {
                probe.requested = true;
                if let Err(error) = state.producer.request_snapshot() {
                    eprintln!("demo-mac: request_snapshot failed: {error}");
                    event_loop.exit();
                    return;
                }
                println!("demo-mac: probe-snapshot requested at {:?}", elapsed);
            }
            if probe.requested
                && let Some(result) = state.producer.poll_snapshot()
            {
                match result {
                    Ok(scrying::WebSurfaceFrame::CpuRgba { pixels, .. }) => {
                        if let Err(error) = pixels.save("demo-mac-snapshot.png") {
                            eprintln!("demo-mac: snapshot save failed: {error}");
                        } else {
                            println!("demo-mac: probe-snapshot saved to demo-mac-snapshot.png");
                        }
                    }
                    Ok(_) => {
                        eprintln!("demo-mac: poll_snapshot returned non-CpuRgba frame");
                    }
                    Err(error) => {
                        eprintln!("demo-mac: probe snapshot failed: {error}");
                    }
                }
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                // The startup code in `App::resumed` wires three
                // distinct webview layouts depending on flags:
                //   - capture mode (`--capture`): left half only
                //     (the wgpu render fills the right half)
                //   - two-tabs mode (`--two-tabs`): tab 1 in the
                //     bottom half, tab 2 in the top half (the
                //     producer's parent NSView is flipped, so
                //     y=H/2 puts the webview's top edge at the
                //     screen midpoint)
                //   - default: webview fills the window
                // The resize handler has to keep all three
                // consistent — without this branch tab 2 (which
                // is *not* `state.producer`) never grows, leaving
                // the bare NSWindow background visible in the
                // top-right when the user widens the window.
                let two_tabs_layout = state.two_tabs_test.is_some();
                let main_size = if state.capture_kickoff_at.is_some() {
                    PhysicalSize::new(new_size.width / 2, new_size.height)
                } else if two_tabs_layout {
                    PhysicalSize::new(new_size.width, new_size.height / 2)
                } else {
                    PhysicalSize::new(new_size.width, new_size.height)
                };
                if let Err(error) = state.producer.resize(main_size) {
                    eprintln!("demo-mac: producer resize failed: {error}");
                }
                if two_tabs_layout {
                    let half_h = new_size.height / 2;
                    if let Err(error) = state.producer.set_offset(0.0, half_h as f32) {
                        eprintln!("demo-mac: producer set_offset failed: {error}");
                    }
                    if let Some(second) = state.second_producer.as_mut() {
                        if let Err(error) = second.resize(PhysicalSize::new(new_size.width, half_h))
                        {
                            eprintln!("demo-mac: second producer resize failed: {error}");
                        }
                    }
                }
                if let Some(render) = state.render.as_mut() {
                    render.resize(new_size.width, new_size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(render) = state.render.as_mut() {
                    if let Err(error) = render.render(&mut state.producer) {
                        eprintln!("demo-mac: render failed: {error}");
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.cursor = Some(position);
                // Only synthesize a host→producer mouse event when
                // the WKWebView is being captured (and therefore
                // not natively visible to AppKit's responder
                // chain). In overlay mode the WKWebView is a
                // subview of the winit window — AppKit delivers
                // mouse / scroll / key events to it directly, so
                // forwarding from here too would double-dispatch
                // every event and produce visible input lag.
                if state.capture_kickoff_at.is_some() {
                    let event = MouseInput {
                        kind: MouseEventKind::Move,
                        virtual_keys: state.mouse_buttons,
                        mouse_data: 0,
                        point: (position.x as i32, position.y as i32),
                    };
                    let _ = state.producer.send_mouse_input(event);
                }
            }
            WindowEvent::MouseInput {
                state: btn_state,
                button,
                ..
            } => {
                let kind = match (button, btn_state) {
                    (MouseButton::Left, ElementState::Pressed) => MouseEventKind::LeftButtonDown,
                    (MouseButton::Left, ElementState::Released) => MouseEventKind::LeftButtonUp,
                    (MouseButton::Right, ElementState::Pressed) => MouseEventKind::RightButtonDown,
                    (MouseButton::Right, ElementState::Released) => MouseEventKind::RightButtonUp,
                    (MouseButton::Middle, ElementState::Pressed) => {
                        MouseEventKind::MiddleButtonDown
                    }
                    (MouseButton::Middle, ElementState::Released) => MouseEventKind::MiddleButtonUp,
                    _ => return,
                };
                match (button, btn_state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        state.mouse_buttons.left_button = true
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        state.mouse_buttons.left_button = false
                    }
                    (MouseButton::Right, ElementState::Pressed) => {
                        state.mouse_buttons.right_button = true
                    }
                    (MouseButton::Right, ElementState::Released) => {
                        state.mouse_buttons.right_button = false
                    }
                    (MouseButton::Middle, ElementState::Pressed) => {
                        state.mouse_buttons.middle_button = true
                    }
                    (MouseButton::Middle, ElementState::Released) => {
                        state.mouse_buttons.middle_button = false
                    }
                    _ => {}
                }
                if state.capture_kickoff_at.is_some() {
                    let point = state
                        .cursor
                        .map(|p| (p.x as i32, p.y as i32))
                        .unwrap_or((0, 0));
                    let _ = state.producer.send_mouse_input(MouseInput {
                        kind,
                        virtual_keys: state.mouse_buttons,
                        mouse_data: 0,
                        point,
                    });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if state.capture_kickoff_at.is_none() {
                    // Overlay mode: AppKit delivers the wheel
                    // event to WKWebView directly. Skip our
                    // synthetic forwarding to avoid double-scroll.
                    return;
                }
                let (dx, dy) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        // Convert lines to pixels with a fudge factor —
                        // matches what AppKit reports for line-units
                        // on most mice.
                        ((x * 16.0) as i32, (y * 16.0) as i32)
                    }
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                };
                let point = state
                    .cursor
                    .map(|p| (p.x as i32, p.y as i32))
                    .unwrap_or((0, 0));
                if dy != 0 {
                    let _ = state.producer.send_mouse_input(MouseInput {
                        kind: MouseEventKind::Wheel,
                        virtual_keys: state.mouse_buttons,
                        mouse_data: dy,
                        point,
                    });
                }
                if dx != 0 {
                    let _ = state.producer.send_mouse_input(MouseInput {
                        kind: MouseEventKind::HorizontalWheel,
                        virtual_keys: state.mouse_buttons,
                        mouse_data: dx,
                        point,
                    });
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                state.modifiers.shift = modifiers.state().shift_key();
                state.modifiers.control = modifiers.state().control_key();
                state.modifiers.alt = modifiers.state().alt_key();
                state.modifiers.meta = modifiers.state().super_key();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                handle_key(state, event);
            }
            _ => {}
        }

        drain_events(state);
    }
}

fn handle_key(state: &mut AppState, event: KeyEvent) {
    let kind = match event.state {
        ElementState::Pressed => KeyEventKind::Down,
        ElementState::Released => KeyEventKind::Up,
    };

    // Hotkey demo bindings: handle on key-press, swallow.
    if event.state == ElementState::Pressed
        && let Key::Character(s) = &event.logical_key
    {
        match s.as_str() {
            "s" | "S" => {
                if let Err(error) = state.producer.request_snapshot() {
                    eprintln!("demo-mac: request_snapshot failed: {error}");
                } else {
                    println!("demo-mac: snapshot requested — will save when ready");
                }
                return;
            }
            "m" | "M" => {
                if let Err(error) = state
                    .producer
                    .post_web_message("hello from demo-mac (key M)")
                {
                    eprintln!("demo-mac: post_web_message failed: {error}");
                } else {
                    println!("demo-mac: posted host->JS message");
                }
                return;
            }
            _ => {}
        }
    }

    // Same overlay/capture gate as mouse forwarding: in overlay
    // mode AppKit delivers keys to WKWebView via the responder
    // chain, so a synthetic forwarder here just doubles every
    // keystroke.
    if state.capture_kickoff_at.is_none() {
        return;
    }

    let characters = match &event.text {
        Some(s) => s.to_string(),
        None => String::new(),
    };
    let virtual_key_code = match event.physical_key {
        winit::keyboard::PhysicalKey::Code(code) => code as u32,
        winit::keyboard::PhysicalKey::Unidentified(_) => 0,
    };

    let input = KeyboardInput {
        kind,
        virtual_key_code,
        characters: characters.clone(),
        characters_ignoring_modifiers: characters,
        modifiers: state.modifiers,
        is_repeat: event.repeat,
    };
    let _ = state.producer.send_keyboard_input(input);
}

fn drain_events(state: &mut AppState) {
    while let Some(event) = state.producer.poll_navigation_event() {
        println!("demo-mac: nav event: {event:?}");
        if let Some(test) = state.browser_test.as_mut()
            && let NavigationEvent::Completed { url, success: true } = &event
        {
            test.last_completed_url = url.clone();
        }
        if let Some(test) = state.interaction_state_test.as_mut()
            && let NavigationEvent::Completed { url, .. } = &event
        {
            test.last_completed_url = url.clone();
        }
        if let Some(test) = state.two_tabs_test.as_mut()
            && let Some(url) = nav_event_url(&event)
        {
            test.tab1_urls.push(url);
        }
        if let Some(test) = state.download_test.as_mut() {
            match &event {
                NavigationEvent::DownloadStarted {
                    id,
                    destination_path,
                    total_bytes_expected,
                    ..
                } => {
                    test.started.insert(
                        *id,
                        StartedRecord {
                            destination_path: destination_path.clone(),
                            _total_bytes_expected: *total_bytes_expected,
                        },
                    );
                }
                NavigationEvent::DownloadProgress { id, .. } => {
                    test.progress_seen.insert(*id);
                }
                NavigationEvent::DownloadFinished { id, error, .. } => {
                    test.finished.insert(*id, error.clone());
                }
                NavigationEvent::DownloadCancelled {
                    id, resume_data, ..
                } => {
                    test.cancelled.insert(*id, resume_data.clone());
                }
                NavigationEvent::AuthChallenged { source, .. } => match source {
                    scrying::AuthSource::Download => {
                        test.download_level_auth_seen = true;
                    }
                    scrying::AuthSource::Page => {
                        test.page_level_auth_seen = true;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    while let Some(message) = state.producer.poll_web_message() {
        println!("demo-mac: js->host: {message}");
        if let Some(test) = state.browser_test.as_mut()
            && let Some(page) = message.strip_prefix("loaded:")
        {
            test.last_loaded_page = page.to_string();
        }
        if let Some(scripted) = state.scripted.as_mut() {
            if message == "ready" {
                scripted.saw_ready = true;
            } else if let Some(rest) = message.strip_prefix("echo:") {
                if rest == "ping-from-host" {
                    scripted.saw_echo = true;
                }
            } else if let Some(rest) = message.strip_prefix("scrolled:") {
                if let Ok(y) = rest.parse::<i64>()
                    && y != 0
                {
                    scripted.saw_scroll = true;
                }
            } else if let Some(rest) = message.strip_prefix("typed:") {
                scripted.saw_typed = rest.to_string();
            }
        }
        if let Some(pointer) = state.pointer_input_test.as_mut() {
            if message == "ready" {
                pointer.saw_ready = true;
            } else if let Some(rest) = message.strip_prefix("ptr:") {
                // Format: "<kind>:<x>,<y>:<pointerType>"
                let mut parts = rest.splitn(3, ':');
                let kind = parts.next().unwrap_or("");
                let _coords = parts.next().unwrap_or("");
                let ptype = parts.next().unwrap_or("");
                if pointer.observed_pointer_type.is_none() && !ptype.is_empty() {
                    pointer.observed_pointer_type = Some(ptype.to_string());
                }
                match kind {
                    "down" => pointer.saw_down = true,
                    "move" => pointer.saw_move = true,
                    "up" => pointer.saw_up = true,
                    "leave" => pointer.saw_leave = true,
                    _ => {}
                }
            }
        }
    }
    while let Some(shape) = state.producer.poll_cursor_shape() {
        println!("demo-mac: cursor change: {shape:?}");
    }
    if state.probe.is_none() {
        // Probe mode handles snapshot draining in `about_to_wait` and
        // exits afterward — don't double-drain or we'd consume the
        // snapshot the probe is waiting on.
        if let Some(result) = state.producer.poll_snapshot() {
            match result {
                Ok(scrying::WebSurfaceFrame::CpuRgba { pixels, .. }) => {
                    match pixels.save("demo-mac-snapshot.png") {
                        Ok(()) => println!("demo-mac: snapshot saved to demo-mac-snapshot.png"),
                        Err(error) => {
                            eprintln!("demo-mac: snapshot save failed: {error}")
                        }
                    }
                }
                Ok(_) => eprintln!("demo-mac: poll_snapshot returned non-CpuRgba"),
                Err(error) => eprintln!("demo-mac: snapshot failed: {error}"),
            }
        }
    }
}

/// Drive the `--scripted` test state machine. Each step posts an
/// action to the producer (or just waits) and transitions on a
/// timeout or on the JS-echoed response.
fn advance_scripted(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(scripted) = state.scripted.as_mut() else {
        return;
    };
    let elapsed = scripted.step_started_at.elapsed();
    let step = scripted.step;
    match step {
        ScriptedStep::Initial => {
            // Load the offline test page.
            if let Err(e) = state.producer.load_html(SCRIPTED_HTML) {
                eprintln!("demo-mac: load_html failed: {e}");
                scripted.failures.push(format!("load_html: {e}"));
                scripted.enter(ScriptedStep::Done);
                return;
            }
            scripted.enter(ScriptedStep::AwaitReady);
        }
        ScriptedStep::AwaitReady => {
            if scripted.saw_ready {
                println!("demo-mac: scripted: page ready");
                scripted.enter(ScriptedStep::PostMessage);
            } else if elapsed > Duration::from_secs(8) {
                scripted.failures.push("page never posted 'ready'".into());
                scripted.enter(ScriptedStep::Done);
            }
        }
        ScriptedStep::PostMessage => {
            // Slice E: host → JS message.
            if let Err(e) = state.producer.post_web_message("ping-from-host") {
                scripted.failures.push(format!("post_web_message: {e}"));
                scripted.enter(ScriptedStep::Done);
                return;
            }
            scripted.enter(ScriptedStep::AwaitEcho);
        }
        ScriptedStep::AwaitEcho => {
            if scripted.saw_echo {
                println!("demo-mac: scripted: JS echoed host message");
                scripted.enter(ScriptedStep::Scroll);
            } else if elapsed > Duration::from_secs(3) {
                scripted
                    .failures
                    .push("JS never echoed host message".into());
                scripted.enter(ScriptedStep::Scroll);
            }
        }
        ScriptedStep::Scroll => {
            // Slice G: scroll wheel forwarding. Send several wheel
            // events; we assert the producer dispatches them
            // without error. Whether WebKit actually scrolls the
            // page is downstream of scrying's contract — synthetic
            // events from an offscreen / unfocused window often
            // bypass WebKit's hit-testing. If the JS scroll
            // listener does fire we capture that as a bonus
            // confirmation.
            //
            // Also dispatch a `MouseMoved` first so the WebView
            // sees a hover-tracking event at the same point —
            // matches the real-input pattern more closely and gives
            // WebKit's input handler more context.
            let _ = state.producer.send_mouse_input(MouseInput {
                kind: MouseEventKind::Move,
                virtual_keys: MouseVirtualKeys::default(),
                mouse_data: 0,
                point: (40, 200),
            });
            let mut all_ok = true;
            for _ in 0..6 {
                if let Err(e) = state.producer.send_mouse_input(MouseInput {
                    kind: MouseEventKind::Wheel,
                    virtual_keys: MouseVirtualKeys::default(),
                    mouse_data: -120,
                    point: (40, 200),
                }) {
                    scripted
                        .failures
                        .push(format!("send_mouse_input(Wheel): {e}"));
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                println!("demo-mac: scripted: 6 ScrollWheel events dispatched without error");
            }
            scripted.enter(ScriptedStep::AwaitScroll);
        }
        ScriptedStep::AwaitScroll => {
            if scripted.saw_scroll {
                println!("demo-mac: scripted: page reported scroll (bonus end-to-end)");
                scripted.enter(ScriptedStep::FocusForKeys);
            } else if elapsed > Duration::from_secs(2) {
                // No-DOM-effect is acceptable for synthetic events
                // when the window isn't user-focused. The API-level
                // dispatch was already asserted in the previous
                // step.
                scripted.enter(ScriptedStep::FocusForKeys);
            }
        }
        ScriptedStep::FocusForKeys => {
            // Click the input box so the keyboard events have a
            // focused target. The test HTML places the input near
            // the top-left of the page (~40-100 px range).
            let pt = (60, 90);
            let _ = state.producer.send_mouse_input(MouseInput {
                kind: MouseEventKind::LeftButtonDown,
                virtual_keys: MouseVirtualKeys::default(),
                mouse_data: 0,
                point: pt,
            });
            let _ = state.producer.send_mouse_input(MouseInput {
                kind: MouseEventKind::LeftButtonUp,
                virtual_keys: MouseVirtualKeys::default(),
                mouse_data: 0,
                point: pt,
            });
            // Also ask the responder chain to focus the WKWebView so
            // keyDown: routes to it.
            if let Err(e) = state
                .producer
                .move_focus(scrying::FocusReason::Programmatic)
            {
                eprintln!("demo-mac: scripted: move_focus failed: {e}");
            }
            scripted.enter(ScriptedStep::SendKeys);
        }
        ScriptedStep::SendKeys => {
            if elapsed < Duration::from_millis(150) {
                return; // brief pause to let focus settle
            }
            // Slice I: type three characters. Same caveat as scroll
            // — synthetic events from an offscreen / unfocused
            // window may not propagate through WebKit's input
            // handler to the focused element. The assertion is that
            // the producer accepts and dispatches the events
            // without error; DOM-side observation is bonus
            // confirmation.
            let mut all_ok = true;
            for ch in ['a', 'b', 'c'] {
                let s = ch.to_string();
                if let Err(e) = state.producer.send_keyboard_input(KeyboardInput {
                    kind: KeyEventKind::Down,
                    virtual_key_code: 0,
                    characters: s.clone(),
                    characters_ignoring_modifiers: s.clone(),
                    modifiers: KeyModifierFlags::default(),
                    is_repeat: false,
                }) {
                    scripted
                        .failures
                        .push(format!("send_keyboard_input(Down '{ch}'): {e}"));
                    all_ok = false;
                    break;
                }
                if let Err(e) = state.producer.send_keyboard_input(KeyboardInput {
                    kind: KeyEventKind::Up,
                    virtual_key_code: 0,
                    characters: s.clone(),
                    characters_ignoring_modifiers: s.clone(),
                    modifiers: KeyModifierFlags::default(),
                    is_repeat: false,
                }) {
                    scripted
                        .failures
                        .push(format!("send_keyboard_input(Up '{ch}'): {e}"));
                    all_ok = false;
                    break;
                }
                scripted.typed_expected.push(ch);
            }
            if all_ok {
                println!("demo-mac: scripted: 3 KeyDown/KeyUp pairs dispatched without error");
            }
            scripted.enter(ScriptedStep::AwaitTyped);
        }
        ScriptedStep::AwaitTyped => {
            if scripted.saw_typed == scripted.typed_expected && !scripted.typed_expected.is_empty()
            {
                println!(
                    "demo-mac: scripted: page reported typed='{}' (bonus end-to-end)",
                    scripted.saw_typed
                );
                scripted.enter(ScriptedStep::Done);
            } else if elapsed > Duration::from_secs(2) {
                // Same as scroll: API dispatch already asserted; DOM
                // delivery is best-effort for synthetic events.
                scripted.enter(ScriptedStep::Done);
            }
        }
        ScriptedStep::Done => {
            // Assert the push-model `set_cursor_handler` callback
            // fired at least once over the test run. The
            // forwarded mouse / scroll / keyboard events drive
            // WebKit's cursor reporting, which `observe_cursor_change`
            // sees and dispatches to the registered handler.
            let cursor_calls = scripted
                .cursor_handler_calls
                .load(std::sync::atomic::Ordering::Relaxed);
            if cursor_calls == 0 {
                scripted.failures.push(
                    "set_cursor_handler callback never fired (cursor handler API regression?)"
                        .into(),
                );
            }
            if scripted.failures.is_empty() {
                println!(
                    "demo-mac: scripted: PASS — slices E + G + I + cursor handler verified at runtime"
                );
                println!("  - cursor handler fired {cursor_calls} time(s) over the run");
                event_loop.exit();
            } else {
                eprintln!("demo-mac: scripted: FAIL");
                for f in &scripted.failures {
                    eprintln!("  - {f}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// Drive the `--profile-test` cookie-persistence run. Loads the test
/// page on first tick, waits for the JS handshake, and exits with a
/// summary that lets a test runner verify across-process persistence.
/// Drive `--profile-test`. Two producers persistent at the SAME
/// `data_dir`. Setting a cookie on producer #1 should be visible
/// to producer #2 because `WKWebsiteDataStore::dataStoreForIdentifier:`
/// returns the same underlying store for the same identifier
/// (the producer hashes `config.data_dir` into a stable UUID).
/// This is the "persistent stores are shared" complement to
/// `--incognito-test`'s "non-persistent stores are isolated"
/// assertion.
fn advance_profile_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.profile_test.as_mut() else {
        return;
    };
    let now = Instant::now();
    if test.step_started_at.is_none() {
        test.step_started_at = Some(now);
    }
    let elapsed = now.duration_since(test.step_started_at.unwrap_or(now));
    let step = test.step;
    macro_rules! step_to {
        ($next:expr) => {{
            test.step = $next;
            test.step_started_at = Some(Instant::now());
        }};
    }
    macro_rules! await_ms {
        ($ms:expr) => {
            elapsed < Duration::from_millis($ms)
        };
    }
    match step {
        ProfileStep::SetCookie => {
            let cookie = Cookie {
                name: test.cookie_name.clone(),
                value: "shared-store".into(),
                domain: "example.com".into(),
                path: "/".into(),
                expires_at: None,
                is_secure: false,
                is_http_only: false,
                same_site: None,
                partitioned: false,
            };
            if let Err(e) = state.producer.set_cookie(&cookie) {
                test.failures
                    .push(format!("set_cookie on producer #1: {e}"));
                step_to!(ProfileStep::Done);
                return;
            }
            println!(
                "demo-mac: profile-test: set_cookie '{}' on producer #1",
                test.cookie_name
            );
            step_to!(ProfileStep::AwaitSet);
        }
        ProfileStep::AwaitSet => {
            if await_ms!(250) {
                return;
            }
            step_to!(ProfileStep::QueryProducer1);
        }
        ProfileStep::QueryProducer1 => {
            if let Err(e) = state.producer.request_all_cookies() {
                test.failures
                    .push(format!("request_all_cookies on producer #1: {e}"));
                step_to!(ProfileStep::Done);
                return;
            }
            step_to!(ProfileStep::AwaitQuery1);
        }
        ProfileStep::AwaitQuery1 => {
            if let Some(cookies) = state.producer.poll_cookies() {
                println!(
                    "demo-mac: profile-test: producer #1 store reports {} cookie(s)",
                    cookies.len()
                );
                test.producer1_cookies = Some(cookies);
                step_to!(ProfileStep::QueryProducer2);
            } else if elapsed > Duration::from_secs(3) {
                test.failures
                    .push("producer #1's poll_cookies never returned".into());
                step_to!(ProfileStep::Done);
            }
        }
        ProfileStep::QueryProducer2 => {
            let Some(second) = state.second_producer.as_mut() else {
                test.failures
                    .push("second_producer was None for profile-test".into());
                step_to!(ProfileStep::Done);
                return;
            };
            if let Err(e) = second.request_all_cookies() {
                test.failures
                    .push(format!("request_all_cookies on producer #2: {e}"));
                step_to!(ProfileStep::Done);
                return;
            }
            step_to!(ProfileStep::AwaitQuery2);
        }
        ProfileStep::AwaitQuery2 => {
            let Some(second) = state.second_producer.as_mut() else {
                test.failures
                    .push("second_producer was None mid-test".into());
                step_to!(ProfileStep::Done);
                return;
            };
            if let Some(cookies) = second.poll_cookies() {
                println!(
                    "demo-mac: profile-test: producer #2 store reports {} cookie(s)",
                    cookies.len()
                );
                test.producer2_cookies = Some(cookies);
                step_to!(ProfileStep::Verify);
            } else if elapsed > Duration::from_secs(3) {
                test.failures
                    .push("producer #2's poll_cookies never returned".into());
                step_to!(ProfileStep::Done);
            }
        }
        ProfileStep::Verify => {
            let in_p1 = test
                .producer1_cookies
                .as_ref()
                .map(|cs| cs.iter().any(|c| c.name == test.cookie_name))
                .unwrap_or(false);
            let in_p2 = test
                .producer2_cookies
                .as_ref()
                .map(|cs| cs.iter().any(|c| c.name == test.cookie_name))
                .unwrap_or(false);
            if !in_p1 {
                test.failures.push(format!(
                    "cookie '{}' missing from producer #1's own store after set_cookie",
                    test.cookie_name
                ));
            }
            if !in_p2 {
                test.failures.push(format!(
                    "cookie '{}' missing from producer #2's store — persistent stores at the same data_dir aren't sharing as expected",
                    test.cookie_name
                ));
            }
            step_to!(ProfileStep::Done);
        }
        ProfileStep::Done => {
            if test.failures.is_empty() {
                println!(
                    "demo-mac: profile-test: PASS — persistent stores at the same data_dir share cookies"
                );
                println!(
                    "  - set_cookie on producer #1 was visible to its own request_all_cookies"
                );
                println!(
                    "  - same cookie was visible to a separate persistent producer at the same data_dir"
                );
                event_loop.exit();
            } else {
                eprintln!("demo-mac: profile-test: FAIL");
                for f in &test.failures {
                    eprintln!("  - {f}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// Drive the `--browser-test` runtime verification of items 1 / 3 /
/// 4 / 9. Each step either issues a producer call and transitions
/// to an Await state, or polls for completion / observed effects.
/// Items 2, 5, 6, 8 aren't covered — they need a real network or
/// harder-to-trigger conditions.
fn advance_browser_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.browser_test.as_mut() else {
        return;
    };
    let now = Instant::now();
    if test.step_started_at.is_none() {
        test.step_started_at = Some(now);
    }
    let elapsed = now.duration_since(test.step_started_at.unwrap_or(now));
    let step = test.step;
    macro_rules! step_to {
        ($next:expr) => {{
            test.step = $next;
            test.step_started_at = Some(Instant::now());
        }};
    }
    macro_rules! await_ms {
        ($ms:expr) => {
            elapsed < Duration::from_millis($ms)
        };
    }
    match step {
        BrowserTestStep::LoadFirst => {
            if let Err(e) = state.producer.load_url("scrying-test://history-1") {
                test.failures.push(format!("load history-1: {e}"));
                step_to!(BrowserTestStep::Done);
                return;
            }
            step_to!(BrowserTestStep::AwaitFirst);
        }
        BrowserTestStep::AwaitFirst => {
            if test.last_loaded_page == "history-1" {
                println!("demo-mac: browser-test: history-1 loaded");
                step_to!(BrowserTestStep::LoadSecond);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push("history-1 never loaded".into());
                step_to!(BrowserTestStep::Done);
            }
        }
        BrowserTestStep::LoadSecond => {
            if let Err(e) = state.producer.load_url("scrying-test://history-2") {
                test.failures.push(format!("load history-2: {e}"));
                step_to!(BrowserTestStep::Done);
                return;
            }
            step_to!(BrowserTestStep::AwaitSecond);
        }
        BrowserTestStep::AwaitSecond => {
            if test.last_loaded_page == "history-2" {
                println!("demo-mac: browser-test: history-2 loaded");
                step_to!(BrowserTestStep::GoBack);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push("history-2 never loaded".into());
                step_to!(BrowserTestStep::Done);
            }
        }
        BrowserTestStep::GoBack => {
            // Settle so WKWebView's back-forward list catches up
            // with the just-completed second load. The
            // `Completed` event fires before WKWebView updates
            // `canGoBack`, so a small delay avoids a flake.
            if await_ms!(200) {
                return;
            }
            // Item 1: history controls.
            if !state.producer.can_go_back() {
                test.failures
                    .push("can_go_back returned false after two loads".into());
                step_to!(BrowserTestStep::ApplySettings);
                return;
            }
            match state.producer.go_back() {
                Ok(true) => step_to!(BrowserTestStep::AwaitBack),
                Ok(false) => {
                    test.failures.push("go_back returned Ok(false)".into());
                    step_to!(BrowserTestStep::ApplySettings);
                }
                Err(e) => {
                    test.failures.push(format!("go_back: {e}"));
                    step_to!(BrowserTestStep::ApplySettings);
                }
            }
        }
        BrowserTestStep::AwaitBack => {
            if test.last_completed_url.contains("history-1") {
                println!("demo-mac: browser-test: go_back navigated to history-1");
                step_to!(BrowserTestStep::GoForward);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push(format!(
                    "go_back didn't navigate to history-1 (saw '{}')",
                    test.last_completed_url
                ));
                step_to!(BrowserTestStep::ApplySettings);
            }
        }
        BrowserTestStep::GoForward => {
            // Brief settle so WebKit's history stack catches up
            // with the just-completed go_back navigation. Without
            // this, `canGoForward` can momentarily return false
            // even though forward navigation is logically valid.
            if await_ms!(200) {
                return;
            }
            match state.producer.go_forward() {
                Ok(true) => step_to!(BrowserTestStep::AwaitForward),
                Ok(false) => {
                    test.failures.push("go_forward returned Ok(false)".into());
                    step_to!(BrowserTestStep::ApplySettings);
                }
                Err(e) => {
                    test.failures.push(format!("go_forward: {e}"));
                    step_to!(BrowserTestStep::ApplySettings);
                }
            }
        }
        BrowserTestStep::AwaitForward => {
            if test.last_completed_url.contains("history-2") {
                println!("demo-mac: browser-test: go_forward navigated to history-2");
                step_to!(BrowserTestStep::ApplySettings);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push(format!(
                    "go_forward didn't navigate to history-2 (saw '{}')",
                    test.last_completed_url
                ));
                step_to!(BrowserTestStep::ApplySettings);
            }
        }
        BrowserTestStep::ApplySettings => {
            // Item 3: settings.
            let settings = scrying::WebSurfaceSettings {
                zoom_factor: Some(1.5),
                javascript_enabled: Some(true),
                devtools_enabled: Some(false),
                user_agent: Some("scrying-demo-test/0.1".into()),
                ..Default::default()
            };
            test.settings_ok = Some(state.producer.apply_settings(&settings).is_ok());
            if test.settings_ok == Some(true) {
                println!("demo-mac: browser-test: apply_settings ok");
            } else {
                test.failures.push("apply_settings returned Err".into());
            }
            step_to!(BrowserTestStep::NavigateForFind);
        }
        BrowserTestStep::NavigateForFind => {
            if let Err(e) = state.producer.load_url("scrying-test://find-target") {
                test.failures.push(format!("load find-target: {e}"));
                step_to!(BrowserTestStep::Done);
                return;
            }
            step_to!(BrowserTestStep::AwaitFindPage);
        }
        BrowserTestStep::AwaitFindPage => {
            if test.last_loaded_page == "find-target" {
                step_to!(BrowserTestStep::FindInPage);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push("find-target never loaded".into());
                step_to!(BrowserTestStep::RequestPdf);
            }
        }
        BrowserTestStep::FindInPage => {
            if await_ms!(150) {
                return;
            }
            // Item 9 part A: find_in_page.
            let opts = FindOptions::default();
            if let Err(e) = state.producer.find_in_page("scrying-find-marker", opts) {
                test.failures.push(format!("find_in_page: {e}"));
                step_to!(BrowserTestStep::RequestPdf);
                return;
            }
            step_to!(BrowserTestStep::AwaitFindResult);
        }
        BrowserTestStep::AwaitFindResult => {
            if let Some(matched) = state.producer.poll_find_match() {
                test.find_result = Some(matched);
                if matched {
                    println!("demo-mac: browser-test: find_in_page matched");
                } else {
                    test.failures
                        .push("find_in_page returned no match for known marker".into());
                }
                step_to!(BrowserTestStep::RequestPdf);
            } else if elapsed > Duration::from_secs(3) {
                test.failures.push("find_in_page never completed".into());
                step_to!(BrowserTestStep::RequestPdf);
            }
        }
        BrowserTestStep::RequestPdf => {
            if let Err(e) = state.producer.request_pdf() {
                test.failures.push(format!("request_pdf: {e}"));
                step_to!(BrowserTestStep::Done);
                return;
            }
            step_to!(BrowserTestStep::AwaitPdfResult);
        }
        BrowserTestStep::AwaitPdfResult => {
            if let Some(result) = state.producer.poll_pdf() {
                match result {
                    Ok(bytes) => {
                        let len = bytes.len();
                        test.pdf_bytes = Some(len);
                        if len > 100 {
                            println!("demo-mac: browser-test: PDF rendered ({len} bytes)");
                        } else {
                            test.failures
                                .push(format!("PDF rendered but suspiciously small ({len} bytes)"));
                        }
                    }
                    Err(msg) => {
                        test.pdf_error = Some(msg.clone());
                        test.failures.push(format!("PDF render failed: {msg}"));
                    }
                }
                step_to!(BrowserTestStep::Done);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push("request_pdf never completed".into());
                step_to!(BrowserTestStep::Done);
            }
        }
        BrowserTestStep::Done => {
            if test.failures.is_empty() {
                println!("demo-mac: browser-test: PASS — items 1, 3, 4, 9 verified at runtime");
                println!(
                    "  - history controls (go_back/go_forward observed via SourceChanged events)"
                );
                println!("  - settings (apply_settings returned Ok)");
                println!(
                    "  - URL schemes (scrying-test:// served {} pages successfully)",
                    if test.last_loaded_page == "find-target" {
                        3
                    } else {
                        2
                    }
                );
                println!(
                    "  - find_in_page → {:?}, request_pdf → {} bytes",
                    test.find_result,
                    test.pdf_bytes.unwrap_or(0)
                );
                event_loop.exit();
            } else {
                eprintln!("demo-mac: browser-test: FAIL");
                for f in &test.failures {
                    eprintln!("  - {f}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// Drive `--interaction-state-test`. Loads three pages so the
/// WKWebView's back-forward list contains [A, B, *C], serializes
/// the interaction state at C, navigates back to A (so the list
/// is [*A, B, C]), then restores the captured blob and asserts
/// the WebView ends up at C with `can_go_back == true` and
/// `can_go_forward == false`.
fn advance_interaction_state_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.interaction_state_test.as_mut() else {
        return;
    };
    let now = Instant::now();
    if test.step_started_at.is_none() {
        test.step_started_at = Some(now);
    }
    let elapsed = now.duration_since(test.step_started_at.unwrap_or(now));
    let step = test.step;
    macro_rules! step_to {
        ($next:expr) => {{
            test.step = $next;
            test.step_started_at = Some(Instant::now());
        }};
    }
    macro_rules! await_ms {
        ($ms:expr) => {
            elapsed < Duration::from_millis($ms)
        };
    }
    const A: &str = "scrying-test://history-1";
    const B: &str = "scrying-test://history-2";
    const C: &str = "scrying-test://find-target";
    match step {
        InteractionStateStep::LoadA => {
            if let Err(e) = state.producer.load_url(A) {
                test.failures.push(format!("load A: {e}"));
                step_to!(InteractionStateStep::Done);
                return;
            }
            step_to!(InteractionStateStep::AwaitA);
        }
        InteractionStateStep::AwaitA => {
            if test.last_completed_url == A {
                step_to!(InteractionStateStep::LoadB);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push("page A never loaded".into());
                step_to!(InteractionStateStep::Done);
            }
        }
        InteractionStateStep::LoadB => {
            if let Err(e) = state.producer.load_url(B) {
                test.failures.push(format!("load B: {e}"));
                step_to!(InteractionStateStep::Done);
                return;
            }
            step_to!(InteractionStateStep::AwaitB);
        }
        InteractionStateStep::AwaitB => {
            if test.last_completed_url == B {
                step_to!(InteractionStateStep::LoadC);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push("page B never loaded".into());
                step_to!(InteractionStateStep::Done);
            }
        }
        InteractionStateStep::LoadC => {
            if let Err(e) = state.producer.load_url(C) {
                test.failures.push(format!("load C: {e}"));
                step_to!(InteractionStateStep::Done);
                return;
            }
            step_to!(InteractionStateStep::AwaitC);
        }
        InteractionStateStep::AwaitC => {
            if test.last_completed_url == C {
                step_to!(InteractionStateStep::Serialize);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push("page C never loaded".into());
                step_to!(InteractionStateStep::Done);
            }
        }
        InteractionStateStep::Serialize => {
            // Settle so WebKit commits C into its back-forward list.
            // serialize_interaction_state pulls a snapshot of that
            // list, so a too-early call captures only [A, B].
            if await_ms!(250) {
                return;
            }
            match state.producer.serialize_interaction_state() {
                Some(bytes) if !bytes.is_empty() => {
                    println!(
                        "demo-mac: interaction-state-test: serialized {} bytes at C",
                        bytes.len()
                    );
                    test.serialized = Some(bytes);
                    step_to!(InteractionStateStep::GoBackOne);
                }
                Some(_) => {
                    test.failures
                        .push("serialize_interaction_state returned empty blob".into());
                    step_to!(InteractionStateStep::Done);
                }
                None => {
                    test.failures
                        .push("serialize_interaction_state returned None".into());
                    step_to!(InteractionStateStep::Done);
                }
            }
        }
        InteractionStateStep::GoBackOne => {
            if await_ms!(150) {
                return;
            }
            match state.producer.go_back() {
                Ok(true) => step_to!(InteractionStateStep::AwaitBackOne),
                Ok(false) => {
                    test.failures.push("go_back #1 returned Ok(false)".into());
                    step_to!(InteractionStateStep::Done);
                }
                Err(e) => {
                    test.failures.push(format!("go_back #1: {e}"));
                    step_to!(InteractionStateStep::Done);
                }
            }
        }
        InteractionStateStep::AwaitBackOne => {
            if test.last_completed_url == B {
                step_to!(InteractionStateStep::GoBackTwo);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push(format!(
                    "go_back #1 didn't land on B (saw '{}')",
                    test.last_completed_url
                ));
                step_to!(InteractionStateStep::Done);
            }
        }
        InteractionStateStep::GoBackTwo => {
            if await_ms!(150) {
                return;
            }
            match state.producer.go_back() {
                Ok(true) => step_to!(InteractionStateStep::AwaitBackTwo),
                Ok(false) => {
                    test.failures.push("go_back #2 returned Ok(false)".into());
                    step_to!(InteractionStateStep::Done);
                }
                Err(e) => {
                    test.failures.push(format!("go_back #2: {e}"));
                    step_to!(InteractionStateStep::Done);
                }
            }
        }
        InteractionStateStep::AwaitBackTwo => {
            if test.last_completed_url == A {
                step_to!(InteractionStateStep::Restore);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push(format!(
                    "go_back #2 didn't land on A (saw '{}')",
                    test.last_completed_url
                ));
                step_to!(InteractionStateStep::Done);
            }
        }
        InteractionStateStep::Restore => {
            if await_ms!(150) {
                return;
            }
            let bytes = match test.serialized.as_ref() {
                Some(b) => b.clone(),
                None => {
                    test.failures.push("no serialized blob to restore".into());
                    step_to!(InteractionStateStep::Done);
                    return;
                }
            };
            // Clear the URL tracker so we can detect the post-restore
            // SourceChanged unambiguously.
            test.last_completed_url.clear();
            match state.producer.restore_interaction_state(&bytes) {
                Ok(()) => {
                    println!("demo-mac: interaction-state-test: restore_interaction_state ok");
                    step_to!(InteractionStateStep::AwaitRestore);
                }
                Err(e) => {
                    test.failures
                        .push(format!("restore_interaction_state: {e}"));
                    step_to!(InteractionStateStep::Done);
                }
            }
        }
        InteractionStateStep::AwaitRestore => {
            if test.last_completed_url == C {
                println!("demo-mac: interaction-state-test: restored to C");
                step_to!(InteractionStateStep::Verify);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push(format!(
                    "restore didn't reload C (saw '{}')",
                    test.last_completed_url
                ));
                step_to!(InteractionStateStep::Done);
            }
        }
        InteractionStateStep::Verify => {
            // After restoring the state captured at C with history
            // [A, B, *C], we expect: at C, can_go_back true, can_go_forward false.
            if await_ms!(200) {
                return;
            }
            if !state.producer.can_go_back() {
                test.failures
                    .push("post-restore can_go_back was false (expected true)".into());
            }
            if state.producer.can_go_forward() {
                test.failures
                    .push("post-restore can_go_forward was true (expected false)".into());
            }
            step_to!(InteractionStateStep::Done);
        }
        InteractionStateStep::Done => {
            if test.failures.is_empty() {
                println!(
                    "demo-mac: interaction-state-test: PASS — serialize/restore round-trip verified"
                );
                println!(
                    "  - serialized at C ({} bytes), navigated back to A, restore landed on C",
                    test.serialized.as_ref().map(|b| b.len()).unwrap_or(0)
                );
                println!("  - post-restore can_go_back == true, can_go_forward == false");
                event_loop.exit();
            } else {
                eprintln!("demo-mac: interaction-state-test: FAIL");
                for f in &test.failures {
                    eprintln!("  - {f}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// Drive `--pointer-input-test`. Loads the pointer-observer page,
/// then synthesizes Down → Update → Up → Leave through
/// `send_pointer_input` (with PointerDevice::Touch so the
/// macOS-side virtual_keys.left_button gets set on the way down).
/// Asserts the JS pointer-event listeners observe each kind.
///
/// On macOS the producer collapses every device to a synthetic
/// mouse event (no public `NSEventTypeDirectTouch` synthesis), so
/// JS sees `pointerType == "mouse"` regardless of the host
/// `event.device`. The test records the observed type so the
/// macOS-specific behavior is documented in stdout.
fn advance_pointer_input_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.pointer_input_test.as_mut() else {
        return;
    };
    let now = Instant::now();
    if test.step_started_at.is_none() {
        test.step_started_at = Some(now);
    }
    let elapsed = now.duration_since(test.step_started_at.unwrap_or(now));
    let step = test.step;
    macro_rules! step_to {
        ($next:expr) => {{
            test.step = $next;
            test.step_started_at = Some(Instant::now());
        }};
    }
    macro_rules! await_ms {
        ($ms:expr) => {
            elapsed < Duration::from_millis($ms)
        };
    }
    match step {
        PointerInputStep::LoadPage => {
            if let Err(e) = state.producer.load_url("scrying-test://pointer") {
                test.failures.push(format!("load pointer page: {e}"));
                step_to!(PointerInputStep::Done);
                return;
            }
            step_to!(PointerInputStep::AwaitReady);
        }
        PointerInputStep::AwaitReady => {
            if test.saw_ready {
                println!("demo-mac: pointer-input-test: page ready");
                step_to!(PointerInputStep::SendDown);
            } else if elapsed > Duration::from_secs(5) {
                test.failures
                    .push("pointer page never posted 'ready'".into());
                step_to!(PointerInputStep::Done);
            }
        }
        PointerInputStep::SendDown => {
            // Brief settle so JS listeners are attached before we
            // start firing events at the document.
            if await_ms!(150) {
                return;
            }
            let event = PointerInput {
                kind: PointerEventKind::Down,
                device: PointerDevice::Touch,
                pointer_id: 1,
                point: (100, 100),
                pressure: 0.5,
                tilt: (0.0, 0.0),
            };
            if let Err(e) = state.producer.send_pointer_input(event) {
                test.failures.push(format!("send_pointer_input(Down): {e}"));
                step_to!(PointerInputStep::Done);
                return;
            }
            step_to!(PointerInputStep::SendMove);
        }
        PointerInputStep::SendMove => {
            if await_ms!(50) {
                return;
            }
            let event = PointerInput {
                kind: PointerEventKind::Update,
                device: PointerDevice::Touch,
                pointer_id: 1,
                point: (150, 150),
                pressure: 0.5,
                tilt: (0.0, 0.0),
            };
            if let Err(e) = state.producer.send_pointer_input(event) {
                test.failures
                    .push(format!("send_pointer_input(Update): {e}"));
                step_to!(PointerInputStep::Done);
                return;
            }
            step_to!(PointerInputStep::SendUp);
        }
        PointerInputStep::SendUp => {
            if await_ms!(50) {
                return;
            }
            let event = PointerInput {
                kind: PointerEventKind::Up,
                device: PointerDevice::Touch,
                pointer_id: 1,
                point: (150, 150),
                pressure: 0.0,
                tilt: (0.0, 0.0),
            };
            if let Err(e) = state.producer.send_pointer_input(event) {
                test.failures.push(format!("send_pointer_input(Up): {e}"));
                step_to!(PointerInputStep::Done);
                return;
            }
            step_to!(PointerInputStep::SendLeave);
        }
        PointerInputStep::SendLeave => {
            if await_ms!(50) {
                return;
            }
            let event = PointerInput {
                kind: PointerEventKind::Leave,
                device: PointerDevice::Touch,
                pointer_id: 1,
                point: (150, 150),
                pressure: 0.0,
                tilt: (0.0, 0.0),
            };
            if let Err(e) = state.producer.send_pointer_input(event) {
                test.failures
                    .push(format!("send_pointer_input(Leave): {e}"));
                step_to!(PointerInputStep::Done);
                return;
            }
            step_to!(PointerInputStep::Verify);
        }
        PointerInputStep::Verify => {
            // Settle so the synthesized events have time to traverse
            // WebKit's input pipeline and fire JS listeners.
            if await_ms!(400) {
                return;
            }
            if !test.saw_down {
                test.failures
                    .push("JS never observed pointerdown after send_pointer_input(Down)".into());
            }
            if !test.saw_move {
                test.failures
                    .push("JS never observed pointermove after send_pointer_input(Update)".into());
            }
            if !test.saw_up {
                test.failures
                    .push("JS never observed pointerup after send_pointer_input(Up)".into());
            }
            // pointerleave is best-effort: WebKit may or may not
            // synthesize one in response to a `mouseExited:` on a
            // single-element document. We log without failing.
            step_to!(PointerInputStep::Done);
        }
        PointerInputStep::Done => {
            if test.failures.is_empty() {
                println!(
                    "demo-mac: pointer-input-test: PASS — Down/Update/Up reached JS pointer listeners"
                );
                println!(
                    "  - JS observed: down={} move={} up={} leave={}",
                    test.saw_down, test.saw_move, test.saw_up, test.saw_leave
                );
                println!(
                    "  - WebKit reported pointerType = {:?} (macOS collapses every device to mouse)",
                    test.observed_pointer_type
                );
                event_loop.exit();
            } else {
                eprintln!("demo-mac: pointer-input-test: FAIL");
                for f in &test.failures {
                    eprintln!("  - {f}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// Drive `--incognito-test`. The main producer is incognito
/// (`non_persistent = true`); the second producer is persistent at
/// a separate `data_dir`. Sets a uniquely-named cookie on the
/// incognito producer's `WKHTTPCookieStore`, then queries both
/// producers' cookie stores and asserts the cookie is visible to
/// the incognito producer but not to the persistent one.
fn advance_incognito_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.incognito_test.as_mut() else {
        return;
    };
    let now = Instant::now();
    if test.step_started_at.is_none() {
        test.step_started_at = Some(now);
    }
    let elapsed = now.duration_since(test.step_started_at.unwrap_or(now));
    let step = test.step;
    macro_rules! step_to {
        ($next:expr) => {{
            test.step = $next;
            test.step_started_at = Some(Instant::now());
        }};
    }
    macro_rules! await_ms {
        ($ms:expr) => {
            elapsed < Duration::from_millis($ms)
        };
    }
    match step {
        IncognitoStep::SetCookie => {
            let cookie = Cookie {
                name: test.cookie_name.clone(),
                // Set HttpOnly so the read path also exercises
                // the round-trip — pre-fix `set_cookie` silently
                // dropped the flag (Apple's property-dict
                // initializer doesn't accept HttpOnly), so the
                // observed cookie's `is_http_only` came back
                // false even when we set it true. The cookies
                // module now routes HttpOnly cookies through
                // `cookiesWithResponseHeaderFields:forURL:` so
                // the flag survives.
                value: "phase-a".into(),
                domain: "example.com".into(),
                path: "/".into(),
                expires_at: None,
                is_secure: false,
                is_http_only: true,
                same_site: None,
                partitioned: false,
            };
            if let Err(e) = state.producer.set_cookie(&cookie) {
                test.failures
                    .push(format!("set_cookie on incognito producer: {e}"));
                step_to!(IncognitoStep::Done);
                return;
            }
            println!(
                "demo-mac: incognito-test: set_cookie '{}' on incognito producer",
                test.cookie_name
            );
            step_to!(IncognitoStep::AwaitSet);
        }
        IncognitoStep::AwaitSet => {
            // `setCookie:completionHandler:` is fire-and-forget on
            // our side; give Apple's cookie store a beat to commit
            // before we query.
            if await_ms!(250) {
                return;
            }
            step_to!(IncognitoStep::QueryIncognito);
        }
        IncognitoStep::QueryIncognito => {
            if let Err(e) = state.producer.request_all_cookies() {
                test.failures
                    .push(format!("request_all_cookies on incognito: {e}"));
                step_to!(IncognitoStep::Done);
                return;
            }
            step_to!(IncognitoStep::AwaitQueryIncognito);
        }
        IncognitoStep::AwaitQueryIncognito => {
            if let Some(cookies) = state.producer.poll_cookies() {
                println!(
                    "demo-mac: incognito-test: incognito store reports {} cookie(s)",
                    cookies.len()
                );
                test.incognito_cookies = Some(cookies);
                step_to!(IncognitoStep::QueryPersistent);
            } else if elapsed > Duration::from_secs(3) {
                test.failures
                    .push("incognito producer's poll_cookies never returned".into());
                step_to!(IncognitoStep::Done);
            }
        }
        IncognitoStep::QueryPersistent => {
            let Some(second) = state.second_producer.as_mut() else {
                test.failures
                    .push("second_producer was None for incognito-test".into());
                step_to!(IncognitoStep::Done);
                return;
            };
            if let Err(e) = second.request_all_cookies() {
                test.failures
                    .push(format!("request_all_cookies on persistent: {e}"));
                step_to!(IncognitoStep::Done);
                return;
            }
            step_to!(IncognitoStep::AwaitQueryPersistent);
        }
        IncognitoStep::AwaitQueryPersistent => {
            let Some(second) = state.second_producer.as_mut() else {
                test.failures
                    .push("second_producer was None mid-test".into());
                step_to!(IncognitoStep::Done);
                return;
            };
            if let Some(cookies) = second.poll_cookies() {
                println!(
                    "demo-mac: incognito-test: persistent store reports {} cookie(s)",
                    cookies.len()
                );
                test.persistent_cookies = Some(cookies);
                step_to!(IncognitoStep::Verify);
            } else if elapsed > Duration::from_secs(3) {
                test.failures
                    .push("persistent producer's poll_cookies never returned".into());
                step_to!(IncognitoStep::Done);
            }
        }
        IncognitoStep::Verify => {
            let observed = test
                .incognito_cookies
                .as_ref()
                .and_then(|cs| cs.iter().find(|c| c.name == test.cookie_name));
            let in_incognito = observed.is_some();
            let http_only_round_tripped = observed.map(|c| c.is_http_only).unwrap_or(false);
            let in_persistent = test
                .persistent_cookies
                .as_ref()
                .map(|cs| cs.iter().any(|c| c.name == test.cookie_name))
                .unwrap_or(false);
            if !in_incognito {
                test.failures.push(format!(
                    "cookie '{}' missing from incognito producer's own store (set_cookie didn't take effect)",
                    test.cookie_name
                ));
            }
            if in_incognito && !http_only_round_tripped {
                test.failures.push(format!(
                    "cookie '{}' was set with is_http_only=true but observed back as is_http_only=false — round-trip regression in cookies module",
                    test.cookie_name
                ));
            }
            if in_persistent {
                test.failures.push(format!(
                    "cookie '{}' leaked into the persistent comparison producer's store — incognito isolation is broken",
                    test.cookie_name
                ));
            }
            step_to!(IncognitoStep::Done);
        }
        IncognitoStep::Done => {
            if test.failures.is_empty() {
                println!(
                    "demo-mac: incognito-test: PASS — non_persistent stores stay isolated from persistent ones"
                );
                println!(
                    "  - set_cookie on incognito producer was visible to its own request_all_cookies"
                );
                println!("  - same cookie was absent from a separate persistent producer's store");
                event_loop.exit();
            } else {
                eprintln!("demo-mac: incognito-test: FAIL");
                for f in &test.failures {
                    eprintln!("  - {f}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// Run the PASS / FAIL summary for `--two-tabs` and exit. Asserts
/// each producer's nav-event queue saw only URLs intended for
/// its own tab — proving multiple producers in one process keep
/// independent event streams (browser-class item 7).
fn finalize_two_tabs_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.two_tabs_test.take() else {
        return;
    };
    let mut failures = Vec::<String>::new();
    let tab1_history1 = test.tab1_urls.iter().any(|u| u.contains("history-1"));
    let tab1_history2 = test.tab1_urls.iter().any(|u| u.contains("history-2"));
    let tab2_history2 = test.tab2_urls.iter().any(|u| u.contains("history-2"));
    let tab2_history1 = test.tab2_urls.iter().any(|u| u.contains("history-1"));

    if !tab1_history1 {
        failures.push("tab 1 never observed any nav event for scrying-test://history-1".into());
    }
    if !tab2_history2 {
        failures.push("tab 2 never observed any nav event for scrying-test://history-2".into());
    }
    if tab1_history2 {
        failures.push(
            "tab 1 saw a nav event for scrying-test://history-2 — cross-talk between independent producers".into(),
        );
    }
    if tab2_history1 {
        failures.push(
            "tab 2 saw a nav event for scrying-test://history-1 — cross-talk between independent producers".into(),
        );
    }

    if failures.is_empty() {
        println!(
            "demo-mac: two-tabs: PASS — multi-instance independence verified ({} tab-1 urls, {} tab-2 urls, no cross-talk)",
            test.tab1_urls.len(),
            test.tab2_urls.len(),
        );
        event_loop.exit();
    } else {
        eprintln!("demo-mac: two-tabs: FAIL");
        for f in &failures {
            eprintln!("  - {f}");
        }
        std::process::exit(1);
    }
}

/// Drain frames in `--capture-test` mode. Called only when
/// `capture_status` reports `Live`. Each call pulls one frame via
/// `try_acquire_frame`, records its `(width, height)` for later
/// assertion, and advances toward the per-size frame target. Once
/// reached (or 30 seconds after the stream became live), routes to
/// `finalize_capture_test`.
fn advance_capture_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.capture_test.as_mut() else {
        return;
    };
    let live_started_at = *test.live_started_at.get_or_insert_with(Instant::now);

    match state.producer.try_acquire_frame() {
        Ok(Some(scrying::WebSurfaceFrame::Native(scrying::NativeFrame::MetalTextureRef(
            frame,
        )))) => {
            let dims = (frame.size.width, frame.size.height);
            if let Some(test) = state.capture_test.as_mut() {
                test.frame_dims.push(dims);
            }
        }
        Ok(Some(_other)) => {
            if let Some(test) = state.capture_test.as_mut() {
                test.failures
                    .push("try_acquire_frame returned non-Metal frame variant".into());
            }
            finalize_capture_test(state, event_loop);
            return;
        }
        Ok(None) => {
            // No new sample yet; SCK delivers at display refresh
            // (~16ms). Stay in this state and wait for next tick.
        }
        Err(error) => {
            if let Some(test) = state.capture_test.as_mut() {
                test.failures
                    .push(format!("try_acquire_frame error: {error}"));
            }
            finalize_capture_test(state, event_loop);
            return;
        }
    }

    let saw_enough = state
        .capture_test
        .as_ref()
        .map(|test| {
            test.expected_dims.iter().all(|expected| {
                test.frame_dims
                    .iter()
                    .filter(|actual| *actual == expected)
                    .count()
                    >= test.frames_per_size
            })
        })
        .unwrap_or(true);
    let timed_out = live_started_at.elapsed() > Duration::from_secs(30);
    if saw_enough || timed_out {
        if !saw_enough && let Some(test) = state.capture_test.as_mut() {
            test.failures.push(format!(
                "captured {} frame(s) within 30s without observing {} frame(s) at every expected size",
                test.frame_dims.len(),
                test.frames_per_size,
            ));
        }
        finalize_capture_test(state, event_loop);
    }
}

/// Run the PASS / FAIL summary for `--capture-test` and exit.
/// Takes the test state out of the AppState so repeat ticks of
/// `about_to_wait` (which fire between `event_loop.exit()` and
/// the actual loop teardown) don't re-print the summary.
fn finalize_capture_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.capture_test.take() else {
        return;
    };
    let mut failures = test.failures;
    for expected in &test.expected_dims {
        let count = test
            .frame_dims
            .iter()
            .filter(|actual| *actual == expected)
            .count();
        if count < test.frames_per_size {
            failures.push(format!(
                "observed {count} frame(s) at {}x{}, expected at least {}",
                expected.0, expected.1, test.frames_per_size,
            ));
        }
    }
    for &(w, h) in &test.frame_dims {
        if !test.expected_dims.contains(&(w, h)) {
            failures.push(format!("unexpected capture dimensions {w}x{h}"));
        }
    }
    if failures.is_empty() {
        let sizes = test
            .expected_dims
            .iter()
            .map(|(w, h)| format!("{w}x{h}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "demo-mac: capture-test: PASS — SCK pipeline delivered {} frames across [{}]",
            test.frame_dims.len(),
            sizes,
        );
        event_loop.exit();
    } else {
        eprintln!("demo-mac: capture-test: FAIL");
        for f in &failures {
            eprintln!("  - {f}");
        }
        std::process::exit(1);
    }
}

/// Drive `--download-test`. Three sub-phases:
///
/// - **Phase A (basic flow)**: load `scrying-test://download`,
///   observe `DownloadStarted` → `DownloadProgress` →
///   `DownloadFinished` events with non-empty IDs and a real
///   `destination_path`, verify the bytes on disk match what the
///   scheme handler served.
/// - **Phase B (host Cancel decision)**: register a destination
///   handler that returns `DownloadDecision::Cancel`, load again,
///   observe `DownloadCancelled` (and *no* `DownloadStarted`,
///   since the cancel happens before we promote the download to
///   the host).
/// - **Phase C (cancel_download(unknown))**: assert
///   `cancel_download(DownloadId(99_999_999))` returns
///   `Ok(false)` for an ID that was never issued.
fn advance_download_test(state: &mut AppState, event_loop: &ActiveEventLoop) {
    let Some(test) = state.download_test.as_mut() else {
        return;
    };
    let now = Instant::now();
    if test.step_started_at.is_none() {
        test.step_started_at = Some(now);
    }
    let elapsed = now.duration_since(test.step_started_at.unwrap_or(now));
    let step = test.step;
    macro_rules! step_to {
        ($next:expr) => {{
            test.step = $next;
            test.step_started_at = Some(Instant::now());
        }};
    }
    macro_rules! await_ms {
        ($ms:expr) => {
            elapsed < Duration::from_millis($ms)
        };
    }
    match step {
        DownloadStep::PhaseALoad => {
            if let Err(e) = state.producer.load_url(&test.download_url) {
                test.failures.push(format!("phase A load: {e}"));
                step_to!(DownloadStep::Done);
                return;
            }
            step_to!(DownloadStep::PhaseAAwaitFinish);
        }
        DownloadStep::PhaseAAwaitFinish => {
            // Find the most-recent (by ID) DownloadStarted; once a
            // matching DownloadFinished is recorded we move on.
            let candidate = test
                .started
                .iter()
                .max_by_key(|(id, _)| id.0)
                .map(|(id, _)| *id);
            let Some(id) = candidate else {
                if elapsed > Duration::from_secs(5) {
                    test.failures
                        .push("phase A: no DownloadStarted observed within 5s".into());
                    step_to!(DownloadStep::Done);
                }
                return;
            };
            if test.finished.contains_key(&id) {
                test.phase_a_id = Some(id);
                step_to!(DownloadStep::PhaseAVerifyFile);
            } else if elapsed > Duration::from_secs(5) {
                test.failures.push(format!(
                    "phase A: DownloadStarted({}) seen but no DownloadFinished within 5s",
                    id.0
                ));
                step_to!(DownloadStep::Done);
            }
        }
        DownloadStep::PhaseAVerifyFile => {
            let Some(id) = test.phase_a_id else {
                step_to!(DownloadStep::Done);
                return;
            };
            let started = test.started.get(&id).cloned_path();
            let path = match started {
                Some(p) => p,
                None => {
                    test.failures
                        .push("phase A: lost DownloadStarted record".into());
                    step_to!(DownloadStep::Done);
                    return;
                }
            };
            if path.as_os_str().is_empty() {
                test.failures.push(format!(
                    "phase A: DownloadStarted carried an empty destination_path for id {}",
                    id.0
                ));
                step_to!(DownloadStep::Done);
                return;
            }
            let finished_error = test.finished.get(&id).cloned().flatten();
            if let Some(err) = finished_error {
                test.failures
                    .push(format!("phase A: DownloadFinished error: {err}"));
                step_to!(DownloadStep::Done);
                return;
            }
            if !test.progress_seen.contains(&id) {
                test.failures.push(format!(
                    "phase A: no DownloadProgress observed for id {} (final-tick should always emit)",
                    id.0
                ));
            }
            // Read the file and compare against the served body.
            match std::fs::read(&path) {
                Ok(actual) => {
                    let expected = download_test_body();
                    if actual != expected {
                        test.failures.push(format!(
                            "phase A: bytes on disk ({} bytes) don't match served payload ({} bytes)",
                            actual.len(),
                            expected.len()
                        ));
                    } else {
                        println!(
                            "demo-mac: download-test: phase A: {} bytes landed at {} (matches served payload)",
                            actual.len(),
                            path.display()
                        );
                    }
                }
                Err(e) => {
                    test.failures.push(format!(
                        "phase A: couldn't read destination file {}: {e}",
                        path.display()
                    ));
                }
            }
            step_to!(DownloadStep::PhaseBInstallCancelHandler);
        }
        DownloadStep::PhaseBInstallCancelHandler => {
            state
                .producer
                .set_download_handler(|_request| DownloadDecision::Cancel);
            step_to!(DownloadStep::PhaseBLoad);
        }
        DownloadStep::PhaseBLoad => {
            // Brief settle so the previous nav's residual events
            // don't bleed into phase B's ID accounting.
            if await_ms!(150) {
                return;
            }
            if let Err(e) = state.producer.load_url(&test.download_url) {
                test.failures.push(format!("phase B load: {e}"));
                step_to!(DownloadStep::Done);
                return;
            }
            step_to!(DownloadStep::PhaseBAwaitCancel);
        }
        DownloadStep::PhaseBAwaitCancel => {
            // Phase B's download ID is the largest one observed in
            // either `started` (if WebKit happened to emit one
            // before we returned Cancel — we don't, but be robust)
            // or `cancelled`.
            let cancelled_max = test.cancelled.keys().map(|id| id.0).max();
            let known_phase_a = test.phase_a_id.map(|id| id.0).unwrap_or(0);
            let Some(cancelled_id) = cancelled_max.filter(|m| *m > known_phase_a).map(DownloadId)
            else {
                if elapsed > Duration::from_secs(5) {
                    test.failures
                        .push("phase B: no DownloadCancelled observed within 5s".into());
                    step_to!(DownloadStep::Done);
                }
                return;
            };
            test.phase_b_id = Some(cancelled_id);
            // Defensive: confirm we did NOT see a DownloadStarted
            // for the cancelled ID — host Cancel suppresses Started.
            if test.started.contains_key(&cancelled_id) {
                test.failures.push(format!(
                    "phase B: host Cancel decision still emitted DownloadStarted for id {}",
                    cancelled_id.0
                ));
            }
            // Defensive: confirm the same ID never produced a
            // DownloadFinished — Cancel routes solely to Cancelled.
            if test.finished.contains_key(&cancelled_id) {
                test.failures.push(format!(
                    "phase B: host Cancel decision still emitted DownloadFinished for id {}",
                    cancelled_id.0
                ));
            }
            println!(
                "demo-mac: download-test: phase B: DownloadCancelled fired for id {} after host Cancel decision",
                cancelled_id.0
            );
            // Drop the host handler so it doesn't bleed into any
            // future test runs that share the same producer.
            state.producer.clear_download_handler();
            step_to!(DownloadStep::PhaseCCancelUnknown);
        }
        DownloadStep::PhaseCCancelUnknown => {
            match state.producer.cancel_download(DownloadId(99_999_999)) {
                Ok(false) => {
                    test.phase_c_unknown_returned_false = true;
                    println!(
                        "demo-mac: download-test: phase C: cancel_download(unknown) returned Ok(false) as expected"
                    );
                }
                Ok(true) => test
                    .failures
                    .push("phase C: cancel_download(unknown) returned Ok(true)".into()),
                Err(e) => test
                    .failures
                    .push(format!("phase C: cancel_download(unknown): {e}")),
            }
            step_to!(DownloadStep::PhaseDInstallAuthHandler);
        }
        DownloadStep::PhaseDInstallAuthHandler => {
            // The shared auth handler covers both page-level and
            // download-level auth challenges; for this test we
            // only care about the download path firing the
            // callback and supplying credentials that satisfy the
            // server's basic-auth challenge.
            state.producer.set_auth_handler(|challenge| {
                eprintln!(
                    "demo-mac: download-test: auth challenge for host '{}' method '{}'",
                    challenge.host, challenge.auth_method
                );
                AuthDisposition::UseCredential {
                    username: DOWNLOAD_AUTH_USER.to_string(),
                    password: DOWNLOAD_AUTH_PASS.to_string(),
                }
            });
            step_to!(DownloadStep::PhaseDLoad);
        }
        DownloadStep::PhaseDLoad => {
            // Brief settle so phase C's residual events don't bleed
            // into phase D's ID accounting.
            if await_ms!(150) {
                return;
            }
            // Use `start_download` (programmatic, bypasses
            // navigation) rather than `load_url` so the auth
            // challenge fires at the WKDownloadDelegate level
            // (download-level auth callback) instead of the
            // NavDelegate level (page-level auth callback). Same
            // shared `AuthHandlerFn` covers both paths; this run
            // exercises the download-specific code path. The
            // discriminator is the AuthChallenged event's `url`:
            // page-level emits the URL, download-level emits "".
            if let Err(e) = state.producer.start_download(&test.download_auth_url) {
                test.failures.push(format!("phase D start_download: {e}"));
                step_to!(DownloadStep::Done);
                return;
            }
            step_to!(DownloadStep::PhaseDAwaitFinish);
        }
        DownloadStep::PhaseDAwaitFinish => {
            // Phase D's download is the largest ID greater than the
            // ones we've already attributed to A and B.
            let phase_a = test.phase_a_id.map(|i| i.0).unwrap_or(0);
            let phase_b = test.phase_b_id.map(|i| i.0).unwrap_or(0);
            let baseline = phase_a.max(phase_b);
            let candidate = test
                .finished
                .keys()
                .copied()
                .filter(|id| id.0 > baseline)
                .max();
            let Some(id) = candidate else {
                if elapsed > Duration::from_secs(8) {
                    test.failures
                        .push("phase D: no DownloadFinished after auth challenge within 8s".into());
                    step_to!(DownloadStep::Done);
                }
                return;
            };
            // The finished entry must succeed (no error). If it
            // failed, the auth handler probably didn't supply a
            // credential WebKit could use.
            if let Some(Some(error)) = test.finished.get(&id) {
                test.failures.push(format!(
                    "phase D: DownloadFinished error: {error} (auth handler didn't satisfy the challenge)"
                ));
                step_to!(DownloadStep::Done);
                return;
            }
            let started = test.started.get(&id);
            let path = match started {
                Some(s) => s.destination_path.clone(),
                None => {
                    test.failures
                        .push("phase D: DownloadFinished without matching DownloadStarted".into());
                    step_to!(DownloadStep::Done);
                    return;
                }
            };
            match std::fs::read(&path) {
                Ok(actual) => {
                    let expected = download_test_body();
                    if actual != expected {
                        test.failures.push(format!(
                            "phase D: bytes on disk ({} bytes) don't match served payload ({} bytes)",
                            actual.len(),
                            expected.len()
                        ));
                    } else {
                        println!(
                            "demo-mac: download-test: phase D: auth-protected download succeeded with {} bytes after credentials supplied",
                            actual.len()
                        );
                    }
                    if !test.download_level_auth_seen {
                        test.failures.push(
                            "phase D: expected download-level auth callback (AuthChallenged with AuthSource::Download) but never observed one"
                                .into(),
                        );
                    }
                }
                Err(e) => {
                    test.failures.push(format!(
                        "phase D: couldn't read auth-protected destination file {}: {e}",
                        path.display()
                    ));
                }
            }
            state.producer.clear_auth_handler();
            step_to!(DownloadStep::PhaseELoad);
        }
        DownloadStep::PhaseELoad => {
            // Brief settle so phase D residue clears.
            if await_ms!(150) {
                return;
            }
            // The slow-resumable URL streams in 8 KiB chunks at
            // 50 ms apart so the cancel below lands mid-transfer.
            if let Err(e) = state.producer.load_url(&test.download_slow_url) {
                test.failures.push(format!("phase E load: {e}"));
                step_to!(DownloadStep::Done);
                return;
            }
            step_to!(DownloadStep::PhaseEAwaitProgress);
        }
        DownloadStep::PhaseEAwaitProgress => {
            // Find a Started for this phase (id greater than every
            // earlier phase's id) and wait for at least one
            // Progress on it, so the cancel arrives mid-stream.
            let baseline = [
                test.phase_a_id.map(|i| i.0).unwrap_or(0),
                test.phase_b_id.map(|i| i.0).unwrap_or(0),
            ]
            .into_iter()
            .max()
            .unwrap_or(0);
            // Exclude any ID that's already in `finished` or
            // `cancelled` — phase E specifically wants the
            // *in-flight* slow download, not phase D's
            // already-completed transfer.
            let candidate = test
                .started
                .keys()
                .copied()
                .filter(|id| {
                    id.0 > baseline
                        && !test.cancelled.contains_key(id)
                        && !test.finished.contains_key(id)
                })
                .max();
            let Some(id) = candidate else {
                if elapsed > Duration::from_secs(5) {
                    test.failures.push(
                        "phase E: no DownloadStarted for slow-resumable download within 5s".into(),
                    );
                    step_to!(DownloadStep::Done);
                }
                return;
            };
            // No need to wait for Progress — Started + !Finished is
            // enough to know the download is in-flight. The
            // cancel arrives 200 ms+ after Started (because the
            // server's first chunk takes 200 ms to flush before
            // the next is written), which is plenty of time for
            // WebKit to capture resume_data.
            test.phase_e_id = Some(id);
            step_to!(DownloadStep::PhaseECancel);
        }
        DownloadStep::PhaseECancel => {
            let Some(id) = test.phase_e_id else {
                step_to!(DownloadStep::Done);
                return;
            };
            // Settle so WebKit's URL session has time to actually
            // process body bytes (not just receive headers). On
            // a 200 ms-per-8 KiB server, 1500 ms means ~7 chunks
            // (~56 KiB) have been delivered — enough mass that
            // WebKit's internal buffering should have flushed at
            // least once and capture viable resume_data on
            // cancel. Resume data depends on opaque WebKit
            // internals; we caveat the soft-skip path below if
            // it still comes back None.
            if await_ms!(1500) {
                return;
            }
            match state.producer.cancel_download(id) {
                Ok(true) => {
                    println!(
                        "demo-mac: download-test: phase E: cancel_download({}) -> Ok(true), awaiting resume_data",
                        id.0
                    );
                    step_to!(DownloadStep::PhaseEAwaitCancel);
                }
                Ok(false) => {
                    test.failures.push(format!(
                        "phase E: cancel_download({}) returned Ok(false) — entry was pruned before cancel",
                        id.0
                    ));
                    step_to!(DownloadStep::Done);
                }
                Err(e) => {
                    test.failures.push(format!("phase E: cancel_download: {e}"));
                    step_to!(DownloadStep::Done);
                }
            }
        }
        DownloadStep::PhaseEAwaitCancel => {
            let Some(id) = test.phase_e_id else {
                step_to!(DownloadStep::Done);
                return;
            };
            match test.cancelled.get(&id) {
                Some(Some(resume_data)) => {
                    println!(
                        "demo-mac: download-test: phase E: DownloadCancelled with {} bytes of resume_data",
                        resume_data.len()
                    );
                    step_to!(DownloadStep::PhaseEResume);
                }
                Some(None) => {
                    // Server claims to support Range but WebKit
                    // didn't capture resume bytes — treat as a
                    // soft skip: the cancel itself worked, but
                    // resume can't be exercised. Still a partial
                    // pass.
                    println!(
                        "demo-mac: download-test: phase E: DownloadCancelled but resume_data was None (WebKit declined to capture); skipping resume sub-phase"
                    );
                    step_to!(DownloadStep::Done);
                }
                None => {
                    if elapsed > Duration::from_secs(10) {
                        test.failures.push(format!(
                            "phase E: no DownloadCancelled for id {} within 10s",
                            id.0
                        ));
                        step_to!(DownloadStep::Done);
                    }
                }
            }
        }
        DownloadStep::PhaseEResume => {
            let Some(id) = test.phase_e_id else {
                step_to!(DownloadStep::Done);
                return;
            };
            let bytes = match test.cancelled.get(&id).and_then(|d| d.as_ref()) {
                Some(b) => b.clone(),
                None => {
                    test.failures
                        .push("phase E: resume_data unavailable at PhaseEResume".into());
                    step_to!(DownloadStep::Done);
                    return;
                }
            };
            // Find the destination_path from the original
            // DownloadStarted record so we can hand it to
            // resume_download (the resumed transfer continues to
            // the same file path WebKit picked originally).
            let destination_path = match test.started.get(&id) {
                Some(s) => s.destination_path.clone(),
                None => {
                    test.failures.push(format!(
                        "phase E: no Started record for id {} at resume time",
                        id.0
                    ));
                    step_to!(DownloadStep::Done);
                    return;
                }
            };
            match state.producer.resume_download(&bytes, destination_path) {
                Ok(resumed_id) => {
                    println!(
                        "demo-mac: download-test: phase E: resume_download issued, resumed id = {}",
                        resumed_id.0
                    );
                    test.phase_e_resume_id = Some(resumed_id);
                }
                Err(e) => {
                    test.failures.push(format!("phase E: resume_download: {e}"));
                    step_to!(DownloadStep::Done);
                    return;
                }
            }
            step_to!(DownloadStep::PhaseEAwaitResume);
        }
        DownloadStep::PhaseEAwaitResume => {
            // The producer's `resume_download` returned the
            // resumed ID synchronously, so we know exactly which
            // entry to wait for here.
            let Some(id) = test.phase_e_resume_id else {
                step_to!(DownloadStep::Done);
                return;
            };
            if test.finished.contains_key(&id) {
                step_to!(DownloadStep::PhaseEVerify);
            } else if elapsed > Duration::from_secs(20) {
                test.failures.push(format!(
                    "phase E: resumed download id {} never finished within 20s",
                    id.0
                ));
                step_to!(DownloadStep::Done);
            }
        }
        DownloadStep::PhaseEVerify => {
            let Some(id) = test.phase_e_resume_id else {
                step_to!(DownloadStep::Done);
                return;
            };
            if let Some(Some(error)) = test.finished.get(&id) {
                test.failures.push(format!(
                    "phase E: resumed download finished with error: {error}"
                ));
                step_to!(DownloadStep::Done);
                return;
            }
            let path = match test.started.get(&id) {
                Some(s) => s.destination_path.clone(),
                None => {
                    test.failures.push(format!(
                        "phase E: resumed download id {} had no Started record",
                        id.0
                    ));
                    step_to!(DownloadStep::Done);
                    return;
                }
            };
            match std::fs::read(&path) {
                Ok(actual) => {
                    let expected = download_test_body();
                    if actual != expected {
                        test.failures.push(format!(
                            "phase E: resumed bytes ({} bytes) don't match expected ({} bytes)",
                            actual.len(),
                            expected.len()
                        ));
                    } else {
                        println!(
                            "demo-mac: download-test: phase E: resumed download produced {} bytes matching the expected payload",
                            actual.len()
                        );
                    }
                }
                Err(e) => {
                    test.failures.push(format!(
                        "phase E: couldn't read resumed file {}: {e}",
                        path.display()
                    ));
                }
            }
            step_to!(DownloadStep::Done);
        }
        DownloadStep::Done => {
            if test.failures.is_empty() {
                println!("demo-mac: download-test: PASS — item 8 download pipeline verified");
                println!(
                    "  - basic: DownloadStarted/Progress/Finished events landed with id {} and bytes match served payload",
                    test.phase_a_id.map(|i| i.0).unwrap_or(0)
                );
                println!(
                    "  - host Cancel decision: DownloadCancelled fired (no Started, no Finished) for id {}",
                    test.phase_b_id.map(|i| i.0).unwrap_or(0)
                );
                println!("  - cancel_download(unknown_id): returned Ok(false) as expected");
                println!(
                    "  - basic-auth download via start_download: shared auth handler supplied credentials, download succeeded (download-level auth callback fired = {})",
                    test.download_level_auth_seen
                );
                match (test.phase_e_id, test.phase_e_resume_id) {
                    (Some(cancel_id), Some(resume_id)) => println!(
                        "  - resume: cancelled mid-stream (id {}), resume_download produced complete bytes (resumed id {})",
                        cancel_id.0, resume_id.0
                    ),
                    (Some(cancel_id), None) => println!(
                        "  - resume: cancelled (id {}), but server / WebKit didn't capture resume_data — soft skip",
                        cancel_id.0
                    ),
                    _ => println!("  - resume: not exercised this run"),
                }
                event_loop.exit();
            } else {
                eprintln!("demo-mac: download-test: FAIL");
                for f in &test.failures {
                    eprintln!("  - {f}");
                }
                std::process::exit(1);
            }
        }
    }
}

/// Trait helper to extract a clone of the destination path from a
/// `&StartedRecord` without the borrow checker tripping on an
/// in-place clone.
trait StartedRecordExt {
    fn cloned_path(self) -> Option<PathBuf>;
}

impl StartedRecordExt for Option<&StartedRecord> {
    fn cloned_path(self) -> Option<PathBuf> {
        self.map(|r| r.destination_path.clone())
    }
}

impl AppState {
    fn new(event_loop: &ActiveEventLoop, cli: Cli) -> Result<Self, Box<dyn std::error::Error>> {
        // Headless test runs hide the window. WKWebView still
        // composes / runs JS / processes synthesized input events
        // when its host window is hidden, so the four `--*-test`
        // assertion modes don't lose any coverage. SCK-capture mode
        // (`--capture`) needs a visible window for the SCWindow
        // lookup to find a real CGWindowID, so it stays visible.
        let window = event_loop.create_window(
            WindowAttributes::default()
                .with_title("scrying demo-mac")
                .with_inner_size(winit::dpi::LogicalSize::new(1024.0, 768.0))
                .with_visible(!cli.is_headless()),
        )?;
        let window = Arc::new(window);

        let ns_view_ptr = match window.window_handle()?.as_raw() {
            RawWindowHandle::AppKit(handle) => handle.ns_view.as_ptr(),
            other => return Err(format!("unexpected RawWindowHandle on macOS: {other:?}").into()),
        };

        let inner = window.inner_size();
        // In capture mode, size the WKWebView to the window's left
        // half so the wgpu-rendered imported texture has the right
        // half of the surface free to draw into. Otherwise the
        // WebView fills the entire window (overlay mode).
        let webview_size = if cli.capture {
            PhysicalSize::new(inner.width / 2, inner.height)
        } else {
            PhysicalSize::new(inner.width, inner.height)
        };

        // Per-profile data store. Path is purely informational on
        // macOS — the producer hashes it into a UUID and resolves a
        // per-profile WKWebsiteDataStore. Using the cargo target dir
        // segregates demo cookies from any other scrying instance the
        // host might run. The profile-test mode uses its own subdir
        // so multiple back-to-back runs share a stable persistent
        // store without bleeding into the regular demo profile.
        let data_root = demo_data_root()?;
        let data_dir = if cli.profile_test {
            // PID-suffixed so each test run gets a fresh shared
            // store — prior-run cookies can't false-positive the
            // "cookie X is in producer #2's store" assertion.
            let pid = std::process::id();
            data_root.join(format!("demo-mac-profile-test-{pid}"))
        } else if cli.incognito_test {
            // Hint path is ignored when `non_persistent` wins, but we
            // pass a unique one anyway so the bookkeeping in the
            // producer is unambiguous if we ever flip the flag off.
            data_root.join("demo-mac-incognito")
        } else if cli.download_test {
            // PID-suffixed so each run gets a fresh
            // WKWebsiteDataStore (test asserts cleanly even if a
            // prior run crashed mid-download).
            let pid = std::process::id();
            data_root.join(format!("demo-mac-download-test-{pid}"))
        } else {
            data_root.join("demo-mac-profile")
        };
        let mut producer_config = WkWebViewProducerConfig::new(webview_size, &data_dir);
        producer_config.color_pipeline = cli.color_pipeline;
        if cli.incognito_test {
            producer_config = producer_config.non_persistent();
        }
        if cli.download_test {
            // Override the default `<data_dir>/downloads` so we can
            // verify the file landed at a known per-run path.
            producer_config.download_dir = data_dir.join("download-output");
            // Pre-clean so prior-run file detritus can't mask a real
            // failure on the "file landed" assertion.
            let _ = std::fs::remove_dir_all(&producer_config.download_dir);
        }

        // Browser-test and interaction-state-test modes register a
        // custom URL scheme so the canned test pages don't depend on
        // the network and have stable origins suitable for
        // `serialize_interaction_state` round-trips.
        let url_schemes: Vec<(String, UrlSchemeHandlerFn)> = if cli.browser_test
            || cli.interaction_state_test
            || cli.pointer_input_test
            || cli.download_test
            || cli.two_tabs
        {
            vec![("scrying-test".to_string(), browser_test_scheme_handler())]
        } else {
            Vec::new()
        };

        // SAFETY: ns_view_ptr is the live NSView from winit's window,
        // which outlives the producer (window is owned by AppState
        // via Arc, dropped only when the event loop exits).
        let mut producer = unsafe {
            WkWebViewProducer::new_with_url_schemes(ns_view_ptr, producer_config, url_schemes)?
        };

        // Scripted mode loads its own offline test page once the
        // event loop is running (in `advance_scripted`); for
        // everything else, kick off the initial network navigation
        // here. We use `load_url` (non-blocking) rather than
        // `navigate_to_url` (blocking) because we're inside winit's
        // `resumed` callback and the blocking variant would pump the
        // main `NSRunLoop` and re-enter winit's event handler (which
        // panics under winit's "no nested event handling" guard).
        // Completion arrives asynchronously via the navigation event
        // FIFO.
        if !cli.scripted
            && !cli.profile_test
            && !cli.two_tabs
            && !cli.browser_test
            && !cli.interaction_state_test
            && !cli.pointer_input_test
            && !cli.incognito_test
            && !cli.download_test
        {
            if let Err(error) = producer.load_url(INITIAL_URL) {
                eprintln!("demo-mac: initial load_url failed: {error}");
            } else {
                println!("demo-mac: started loading {INITIAL_URL}");
            }
        }

        // `--incognito-test`: spin up a *persistent* second producer
        // alongside the (incognito) main producer. The test asserts
        // a cookie set on the main producer never reaches the
        // persistent producer's cookie store. The PID-suffixed
        // data_dir guarantees a fresh per-run WKWebsiteDataStore so
        // stale cookies from prior runs can't false-positive.
        let second_producer = if cli.incognito_test {
            let pid = std::process::id();
            let persistent_data_dir =
                data_root.join(format!("demo-mac-incognito-persistent-{pid}"));
            let second_config = WkWebViewProducerConfig::new(
                PhysicalSize::new(inner.width, inner.height),
                &persistent_data_dir,
            );
            // SAFETY: ns_view_ptr is the same valid NSView the first
            // producer was constructed against; both producers will
            // be dropped before the window vanishes.
            let second = unsafe { WkWebViewProducer::new(ns_view_ptr, second_config)? };
            println!("demo-mac: --incognito-test: spun up persistent comparison producer");
            Some(second)
        } else if cli.profile_test {
            // Persistent counterpart at the SAME data_dir as the
            // main producer. Setting a cookie on the main producer
            // should be visible to this one — that's the
            // "shared persistent store" property profile-test
            // asserts.
            let second_config = WkWebViewProducerConfig::new(webview_size, &data_dir);
            // SAFETY: same NSView the first producer was built
            // against; both drop before the window vanishes.
            let second = unsafe { WkWebViewProducer::new(ns_view_ptr, second_config)? };
            println!("demo-mac: --profile-test: spun up persistent counterpart at same data_dir");
            Some(second)
        } else if cli.two_tabs {
            // Position tab 1 at top-half and tab 2 at bottom-half so
            // both are visually distinct in any captured frame.
            let half_height = inner.height / 2;
            let _ = producer.resize(PhysicalSize::new(inner.width, half_height));
            let _ = producer.set_offset(0.0, half_height as f32);
            // Use the scrying-test:// custom scheme rather than
            // network URLs so the cross-talk assertion runs
            // hermetically: tab 1 loads /history-1, tab 2 loads
            // /history-2, and the test verifies each producer's
            // nav-event queue saw only its own URL.
            let _ = producer.load_url("scrying-test://history-1");

            let second_data_dir = data_root.join("demo-mac-multi-tab");
            let second_config = WkWebViewProducerConfig::new(
                PhysicalSize::new(inner.width, half_height),
                &second_data_dir,
            );
            let second_url_schemes: Vec<(String, UrlSchemeHandlerFn)> =
                vec![("scrying-test".to_string(), browser_test_scheme_handler())];
            // SAFETY: ns_view_ptr is the same valid NSView the first
            // producer was constructed against; both producers will
            // be dropped before the window vanishes.
            let second = unsafe {
                WkWebViewProducer::new_with_url_schemes(
                    ns_view_ptr,
                    second_config,
                    second_url_schemes,
                )?
            };
            let _ = second.load_url("scrying-test://history-2");
            println!("demo-mac: --two-tabs: spun up second producer");
            Some(second)
        } else {
            None
        };

        let render = if cli.capture {
            let mut r = pollster::block_on(WgpuRender::new(window.clone()))?;
            if cli.dump_every > 0 {
                r.dump_every = Some(cli.dump_every);
                println!(
                    "demo-mac: dumping every {} imported frame(s) to demo-mac-frame-NNNN.png",
                    cli.dump_every
                );
            }
            Some(r)
        } else {
            None
        };

        let resize_test_steps = if cli.resize_test {
            Some(vec![
                (1280, 800, Duration::from_secs(6)),
                (768, 600, Duration::from_secs(10)),
                (1024, 768, Duration::from_secs(14)),
            ])
        } else {
            None
        };

        let capture_test = cli.capture_test.then(|| {
            let mut expected_dims = vec![(webview_size.width, webview_size.height)];
            if let Some(steps) = resize_test_steps.as_ref() {
                expected_dims.extend(steps.iter().map(|(width, height, _)| (*width / 2, *height)));
                expected_dims.sort_unstable();
                expected_dims.dedup();
            }
            CaptureTestState {
                frame_dims: Vec::new(),
                expected_dims,
                frames_per_size: if cli.resize_test { 3 } else { 5 },
                capture_started_at: None,
                live_started_at: None,
                failures: Vec::new(),
            }
        });

        // Download-test mode needs a `Content-Disposition: attachment`
        // HTTP response — WebKit doesn't promote custom URL-scheme
        // responses to downloads even when they're octet-stream, so
        // we serve the test payload via a loopback HTTP listener
        // and navigate to that URL instead.
        // Scripted mode also exercises the push-model cursor
        // handler (parity with the existing pull-model
        // `poll_cursor_shape` queue). Build the state first so we
        // can register the handler against its Arc counter, then
        // move the state into AppState below.
        let scripted_state = if cli.scripted {
            let state = ScriptedState::new();
            let counter = Arc::clone(&state.cursor_handler_calls);
            producer.set_cursor_handler(move |_shape| {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            Some(state)
        } else {
            None
        };

        let download_test_state = if cli.download_test {
            let urls = start_download_test_server(download_test_body()).map_err(
                |e| -> Box<dyn std::error::Error> {
                    format!("download-test: failed to start loopback server: {e}").into()
                },
            )?;
            println!("demo-mac: download-test: serving payload at {}", urls.plain);
            println!(
                "demo-mac: download-test: auth-required payload at {}",
                urls.auth_required
            );
            Some(DownloadTestState {
                download_url: urls.plain,
                download_auth_url: urls.auth_required,
                download_slow_url: urls.slow_resumable,
                ..DownloadTestState::default()
            })
        } else {
            None
        };

        Ok(Self {
            window,
            producer,
            render,
            capture_kickoff_at: cli.capture.then_some(Duration::from_secs(3)),
            capture_started: false,
            resize_test_steps,
            resize_test_idx: 0,
            cursor: None,
            mouse_buttons: MouseVirtualKeys::default(),
            modifiers: KeyModifierFlags::default(),
            probe: cli.probe_snapshot.then(|| ProbeState {
                requested: false,
                request_at: Duration::from_secs(3),
            }),
            scripted: scripted_state,
            profile_test: cli.profile_test.then(|| ProfileTestState {
                cookie_name: format!("scrying-profile-{}", std::process::id()),
                ..ProfileTestState::default()
            }),
            second_producer,
            // Skip the auto-exit timer in --visible mode so the
            // developer can resize / interact / screenshot without
            // racing an 8-second clock. Tests still run; they just
            // don't close the window.
            two_tabs_deadline: (cli.two_tabs && !cli.visible).then_some(Duration::from_secs(8)),
            two_tabs_test: cli.two_tabs.then(TwoTabsTestState::default),
            browser_test: cli.browser_test.then(BrowserTestState::default),
            interaction_state_test: cli
                .interaction_state_test
                .then(InteractionStateTestState::default),
            pointer_input_test: cli.pointer_input_test.then(PointerInputTestState::default),
            incognito_test: cli.incognito_test.then(|| IncognitoTestState {
                cookie_name: format!("scrying-incognito-{}", std::process::id()),
                ..IncognitoTestState::default()
            }),
            download_test: download_test_state,
            capture_test,
            started_at: Instant::now(),
        })
    }
}

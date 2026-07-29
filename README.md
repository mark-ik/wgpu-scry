# wgpu-scry

Capability-driven system-webview frame adapter. Scry into a platform system
webview (WebView2 / WKWebView / WebKitGTK / WPE), produce frames a wgpu host
renderer can consume, and forward input back to the page by API.

The library crate is [`scrying`](scrying/). The name comes from *scrying* —
gazing into a reflective surface for visions. The webview is the surface, the
captured frame is the vision, and this crate is the lens.

This repo was extracted from [`wgpu-graft`](https://github.com/merely-made/wgpu-graft)
on 2026-05-05 so that system-webview frame production has its own place to
evolve. `scrying` owns its native-frame import path in-tree as the
[`scrying::native_frame`](scrying/src/native_frame/) module, structurally
derived from Slint's
[Servo embedding example](https://github.com/slint-ui/slint/tree/master/examples/servo)
(see [NOTICE](NOTICE)).

**Made with AI**

## What it does

`scrying` splits responsibilities deliberately:

- **scrying owns backend selection.** Platform modules, concrete producer type
  aliases, and engine dependencies are `cfg(target_os = ...)` gated. A Windows
  build selects WebView2, a macOS build selects WKWebView, and a Linux build
  selects one of three WebKit-family backends by cargo feature, without
  compiling the other engine paths.
- **the host owns embedding.** The host creates the window and event loop,
  supplies the native parent handle, chooses size and data-dir policy, and
  forwards native input and lifecycle events.
- **runtime capability probing layers on top.** `WebSurfaceCapabilities::probe`
  reports which surface modes are viable for the current GPU / OS / runtime
  after the backend has been selected at compile time.

Frames are surfaced as one of four `WebSurfaceMode` outcomes: an imported native
GPU texture (`ImportedTexture`), a native child overlay (`NativeChildOverlay`),
CPU pixels or an encoded PNG (`CpuSnapshot`), or `Unsupported`. The
`WebSurfaceProducer` trait covers the full embeddable surface, not only frame
production: navigation, history, mouse / pointer / keyboard / drag input,
lifecycle events, cursor reporting, JS message bridge, settings, profiles, and
snapshots. Methods a given platform has not implemented return
`WebSurfaceError::Unsupported` rather than panicking, so consumers can probe the
surface incrementally.

## Workspace

Root `Cargo.toml` declares a 7-member workspace (`resolver = "3"`, edition 2024,
MPL-2.0).

| Crate | Version | Role |
| --- | --- | --- |
| [`scrying`](scrying/) | 0.4.0 | The library. Capability probe (`WebSurfaceMode` / `WebSurfaceCapabilities`), the `WebSurfaceProducer` trait, per-platform producer implementations, and the in-tree `native_frame` GPU-import path. |
| [`demo-scrying-winit`](demo-scrying-winit/) | — | Cross-platform backend-selection smoke. Brings up a winit + wgpu host and reports the detected backend, the platform producer / config aliases, capability status, and supported native frame kinds for the current target. |
| [`demo-win`](demo-win/) | — | Windows WebView2 composition runtime probe. Drives the CompositionController path into a wgpu texture (WGC capture, shared D3D11 texture import, resize, input, navigation / message / cursor drains, optional readback / fence diagnostics). |
| [`demo-mac`](demo-mac/) | — | macOS WKWebView host probe. Hosts a `WkWebViewProducer` against a winit window's `NSView`; flagged modes exercise navigation, input, JS messaging, ScreenCaptureKit capture, and per-profile data stores. |
| [`demo-linux`](demo-linux/) | — | WebKitGTK 4.1 (GTK 3) runtime probe. Built with the `webkitgtk-fallback` feature. Hosts a `WebKitGtkProducer` in a `GtkOffscreenWindow`, navigates, and writes a CPU RGBA PNG snapshot. |
| [`demo-linux6`](demo-linux6/) | — | WebKitGTK 6.0 (GTK 4) runtime probe. Built with the `webkit6` feature. Same shape as `demo-linux` against `gtk4` + `webkit6` and the `NetworkSession` data-dir model. |
| [`demo-wpe`](demo-wpe/) | — | WPE runtime probe. Built with the `wpe` feature. Constructs a `WpeProducer` against a self-owned `WPEDisplayHeadless` + `WebKitWebView`, navigates, and pulls one DMABUF frame (plane fds + DRM format / modifier + optional `VkSemaphore` fd). |

The demo crates are path-deps on `scrying`; they are not published.

## Platform support

| Platform | Backend | Frame transport | Status |
| --- | --- | --- | --- |
| Windows | WebView2 (`webview2_composition_producer`) | WebView2 CompositionController → WinComp visual → `Windows.Graphics.Capture` → shared D3D11 NT-handle texture → wgpu D3D12 import (`ImportedTexture`) | Reference implementation. |
| macOS | WKWebView (`wkwebview_producer`) | ScreenCaptureKit → `IOSurfaceRef` → `MTLTexture` → wgpu Metal import (`ImportedTexture`), plus a CPU `takeSnapshot:` path (`CpuSnapshot`) | Implemented. Minimum macOS 14.0 (Sonoma). |
| Linux | WebKitGTK 4.1 (`webkitgtk-fallback`) | CPU snapshot via `webkit_web_view_get_snapshot` (`CpuRgba`) | Implemented. |
| Linux | WebKitGTK 6.0 (`webkit6`) | CPU snapshot via `GtkWidgetPaintable` → `GskRenderer::render_texture` → `GdkTextureDownloader`, with legacy snapshot fallback (`CpuRgba`) | Implemented. GPU import blocked upstream (see parity matrix). |
| Linux | WPE (`wpe`) | DMABUF fds + optional `VkSemaphore` opaque fd → wgpu Vulkan import (`DmaBufImage`) | Implemented. |

Linux backend selection is by cargo feature, and the three are mutually
exclusive in practice. When neither `webkit6` nor `webkitgtk-fallback` is
enabled, the WPE producer is selected. When both `webkit6` and
`webkitgtk-fallback` are enabled, `webkit6` wins, but expect a larger
dependency graph and noisier build output (the two pull incompatible glib / gtk
crate-version trees).

macOS notes: the producer hard-depends on `WKWebsiteDataStore::dataStoreForIdentifier:`
(macOS 14+) and `WKWebView::setInspectable:` (macOS 13.3+), called without
runtime-availability guards, so building or running against an older SDK / OS is
unsupported. CI targets `macos-latest`.

## Install

```toml
[dependencies]
scrying = { git = "https://github.com/merely-made/wgpu-scry" }
```

`scrying` is not yet published to crates.io. The crate's default feature set is
empty; on Linux pick exactly one of `webkitgtk-fallback`, `webkit6`, or `wpe`
(omitting all three selects the WPE producer). It depends on `wgpu` 29 with the
`metal` feature, `wgpu-hal` 29, `image` 0.25, `palette` 0.7, `thiserror` 2, and
`dpi` 0.1.2, plus the platform-specific engine bindings below.

## Quick start

```bash
cargo check -p scrying
# Cross-platform backend-selection smoke
cargo run -p demo-scrying-winit
```

### Windows (WebView2)

```bash
cargo run -p demo-win                            # interactive runtime probe
cargo run -p demo-win -- --scripted              # JS messaging + input forwarding smoke
cargo run -p demo-win -- --browser-test          # history / settings / visibility
cargo run -p demo-win -- --cookie-test           # cookie read / write / delete
cargo run -p demo-win -- --profile-test          # persistent user_data_dir survives recreation
cargo run -p demo-win -- --incognito-test        # InPrivate profile isolation
cargo run -p demo-win -- --popup-test            # target=_blank / window.open routing
cargo run -p demo-win -- --routing-test          # WebResourceRequested virtual-host content
cargo run -p demo-win -- --process-test          # ProcessFailed + fresh-navigation recovery
cargo run -p demo-win -- --download-test         # DownloadStarting + host destination
cargo run -p demo-win -- --auth-test             # BasicAuthenticationRequested + host credentials
cargo run -p demo-win -- --permission-test       # PermissionRequested + host denial
cargo run -p demo-win -- --visibility-test       # SetIsVisible -> Page Visibility state
cargo run -p demo-win -- --find-test             # native find + match count
cargo run -p demo-win -- --pdf-test              # native PrintToPdfStream bytes
cargo run -p demo-win -- --context-test          # ContextMenuRequested bridge
cargo run -p demo-win -- --media-test            # media-capture lifecycle bridge
cargo run -p demo-win -- --multi-view-test       # simultaneous producers on separate HWNDs
```

### macOS (WKWebView)

```bash
cargo run -p demo-mac                                 # overlay mode (default)
cargo run -p demo-mac -- --scripted                   # JS messaging + input forwarding
cargo run -p demo-mac -- --browser-test               # history / settings / URL schemes / find / PDF
cargo run -p demo-mac -- --interaction-state-test     # interactionState round-trip
cargo run -p demo-mac -- --pointer-input-test         # send_pointer_input -> JS pointer events
cargo run -p demo-mac -- --incognito-test             # nonPersistentDataStore isolation
cargo run -p demo-mac -- --download-test              # downloads pipeline (HTTP loopback)
cargo run -p demo-mac -- --probe-snapshot             # CPU snapshot via takeSnapshot:
cargo run -p demo-mac -- --capture --dump-every 30    # SCK pipeline + per-N-frame readback
cargo run -p demo-mac -- --capture-test               # SCK assertion smoke (needs Screen Recording perm)
cargo run -p demo-mac -- --profile-test               # persistent store shared across producers
cargo run -p demo-mac -- --two-tabs                   # multi-instance independence

# All assertion-style runs at once (headless, exit 1 on any FAIL)
bash scripts/test-mac.sh
```

`--*-test` modes default to a hidden window and
`NSApplicationActivationPolicyProhibited` so they run silently in the
background; pass `--visible` to watch the WKWebView in real time.
`--capture-test` is the exception — it forces visibility because
ScreenCaptureKit cannot capture hidden windows, and it is held out of
`scripts/test-mac.sh` because Screen Recording permission cannot be
self-granted.

### Linux — WebKitGTK 4.1 (`webkitgtk-fallback`)

```bash
cargo run -p demo-linux                                          # default HTML -> snapshot.png
cargo run -p demo-linux -- --probe-only                         # capability probe + exit
cargo run -p demo-linux -- --snapshot-test --out /tmp/snap.png  # exit 1 on empty / zero-pixel snapshot
cargo run -p demo-linux -- --scripted                           # bidirectional JS-messaging round-trip
cargo run -p demo-linux -- --input-test                         # synthesized mouse + keyboard reaches handlers
cargo run -p demo-linux -- --cookie-test                        # cookie store set / get / delete
cargo run -p demo-linux -- --scheme-test                        # custom scry:// scheme round-trip
cargo run -p demo-linux -- --popup-test                         # target=_blank -> NewWindowRequested intercept
cargo run -p demo-linux -- --download-test                      # file:// download lifecycle events
cargo run -p demo-linux -- --cursor-test                        # hover-a-link -> CursorShape::Pointer
cargo run -p demo-linux -- --ime-test                           # autofocus input -> TextInputFocused
cargo run -p demo-linux -- --drag-test                          # send_drag_input Enter -> Drop reaches handler
cargo run -p demo-linux -- --text-test                          # send_text round-trips native key dispatch

bash scripts/test-linux.sh                                      # all assertion modes (headless via offscreen WebView)
```

### Linux — WebKitGTK 6.0 / GTK 4 (`webkit6`)

```bash
cargo run -p demo-linux6                                                 # default HTML -> snapshot.png
cargo run -p demo-linux6 -- --probe-only                                # capability probe + exit
cargo run -p demo-linux6 -- --snapshot-test                            # exit 1 on empty / zero-pixel snapshot
cargo run -p demo-linux6 -- --url https://example.com --out example.png # real-page snapshot
cargo run -p demo-linux6 -- --scripted                                 # bidirectional JS-messaging round-trip
cargo run -p demo-linux6 -- --cookie-test                             # cookie store set / get / delete
cargo run -p demo-linux6 -- --scheme-test                            # custom myscheme:// round-trip
cargo run -p demo-linux6 -- --input-test                            # JS-synthesized input (isTrusted=false)
cargo run -p demo-linux6 -- --download-test                        # file:// download lifecycle events
```

### Linux — WPE (`wpe`)

```bash
cargo run -p demo-wpe                                  # default HTML -> one DMABUF frame
cargo run -p demo-wpe -- --probe-only                 # capability probe + exit
cargo run -p demo-wpe -- --snapshot-test              # exit 1 if no DMABUF frame within ~10s
cargo run -p demo-wpe -- --url https://example.com    # real-page -> one DMABUF frame
```

WPE requires WPEWebKit 2.52.3 + Wayland + Vulkan. See
[`docs/wpe-deployment.md`](docs/wpe-deployment.md) for install and runtime
requirements.

## Linux system-package prerequisites

Fedora 44 package names below; translate for Debian / Ubuntu / Arch. The Ubuntu
equivalents for the `webkitgtk-fallback` line are mirrored in
`.github/workflows/test-linux.yml`.

```bash
# WebKitGTK 4.1 (GTK 3 line) — webkitgtk-fallback feature
sudo dnf install -y gcc gcc-c++ \
  webkit2gtk4.1-devel \
  vulkan-loader-devel vulkan-headers mesa-vulkan-drivers \
  libxkbcommon-devel libxkbcommon-x11-devel wayland-devel \
  libX11-devel libXcursor-devel libXrandr-devel libXi-devel libxcb-devel

# WebKitGTK 6.0 (GTK 4 line) — webkit6 feature
sudo dnf install -y gtk4-devel webkitgtk6.0-devel

# WPE — wpe feature
sudo dnf install -y wpewebkit-devel libwpe-devel wpebackend-fdo-devel
```

The `scrying` crate's `cfg(target_os = "linux")` dev-dependencies (`gbm`,
`pollster`) also need `mesa-libgbm-devel` and `libdrm-devel` for the DMABUF
round-trip test.

## GPU synchronization

Two cross-API sync paths are wired:

- **Windows.** The shared D3D11 destination texture is allocated with
  `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`
  and reused across frames. The producer copies the WGC capture frame in under a
  keyed-mutex acquire, waits on a `D3D11_QUERY_EVENT` for GPU completion, then
  releases. The consumer cannot reach the keyed mutex through the D3D12 handle,
  so it issues a throwaway 1x1 `copy_texture_to_buffer` before each render pass;
  wgpu's automatic state tracking inserts a transition barrier that flushes
  shader caches holding a stale view. This is empirically correct over long runs
  but relies on driver behavior rather than a contract. An explicit
  `D3D12_FENCE_FLAG_SHARED` fence path is the planned upgrade (see
  [`scrying/README.md`](scrying/README.md)).
- **macOS.** IOSurface has implicit cross-API cache coherence on Apple silicon
  (and via IOSurface locks on Intel), so today's path needs no explicit fence. A
  `MetalSharedEventSynchronizer` is scaffolded but inert because ScreenCaptureKit
  does not expose a render-queue hook to drive a signal from.

Critical caveat for event-loop hosts on macOS: blocking entry points
(`navigate_to_url`, `navigate_to_string`, `start_capture`,
`capture_cpu_snapshot`) pump the main `NSRunLoop` and must not be called from
inside a host event-loop callback (winit's `resumed` / `window_event`). Use the
non-blocking equivalents (`load_url` / `load_html`, `start_capture_async`,
`request_snapshot` + `poll_snapshot`) from those contexts.

## Status

Each backend delivers a working frame transport and a broad slice of the
browser-class surface (navigation, history, input, cookies, custom schemes,
script-message bridge, downloads, profile isolation). Per-capability state,
including documented caveats, lives in
[`docs/parity-matrix.md`](docs/parity-matrix.md). Notable open items:

- WebKitGTK 6.0 GPU texture import is blocked upstream: GTK 4 (through 4.22) does
  not export a public C accessor for `GdkDmabufTexture` plane fds / fourcc /
  modifier, so the producer downloads pixels into the `CpuRgba` tier.
- WebKitGTK 6.0 input is JS-event-synthesis only (synthesized DOM events arrive
  with `isTrusted === false`), because GTK 4 removed `gtk_main_do_event`.
- WPE headless `WPEToplevelHeadless::resize` is a no-op in WPEWebKit 2.52.3; pick
  the final size at `WpeProducer::new`. Touch dispatch hangs on headless WPE.
- WPE pixel-correctness through the wgpu Vulkan importer is degraded on RADV with
  DCC-compressed RGBA until wgpu lands a `texture_from_raw` initial-state API.

CI runs the macOS suite (`.github/workflows/test-mac.yml`, `macos-latest`) and
the WebKitGTK 4.1 Linux suite (`.github/workflows/test-linux.yml`,
`ubuntu-latest`) on every push and pull request to `master`.

## Documentation

- [`docs/parity-matrix.md`](docs/parity-matrix.md) — capability parity matrix
  across all five backends.
- [`docs/wpe-deployment.md`](docs/wpe-deployment.md) — WPE install and runtime
  requirements, headless limitations, troubleshooting.
- [`scrying/README.md`](scrying/README.md) — producer / consumer contract,
  Windows WGC + shared D3D11 path, explicit-fence-sync future work.
- [`design_docs/`](design_docs/) — phase plans and retrospectives (Phase 4 WPE
  arc and Phase A webkit6 arc are the most recent).

## Relationship to wgpu-graft and wgpu-weld

`wgpu-scry` is part of a family of sibling projects that split web / rendering-
engine embedding by engine target:

- **`wgpu-scry`** (this repo) — system webviews (WebKit family): WebView2 on
  Windows, WKWebView on macOS, WebKitGTK 4.1 / 6.0 / WPE on Linux.
- **[`wgpu-graft`](https://github.com/merely-made/wgpu-graft)** — Servo embedding: a
  single Servo producer over GL-FBO interop, demoed across host frameworks
  (winit, egui, iced, Blitz, Slint, Bevy, xilem, gpui).
- **`wgpu-weld`** — CEF / Chromium embedding.

The three projects share no code. Each engine's threading model, sync story, and
API surface differs enough that a shared crate is not worth it. `wgpu-graft` is
the origin (derived from Slint's Servo embedding example); `wgpu-scry` was
extracted from it and keeps that Slint-derived `native_frame` structure, while
`wgpu-weld` follows the same import pattern with no Slint-derived code. `scrying`
owns its native-frame import in-tree because it takes platform-native texture
handles directly (D3D12 NT-handle, IOSurface, DMABUF) rather than bridging from a
GL framebuffer.

## License

[MPL-2.0](LICENSE). The `scrying::native_frame` module is structurally derived
from the Slint Servo embedding example (MIT); see [NOTICE](NOTICE).
</content>
</invoke>

# scrying

Capability-driven system-webview adapter — scry into WebView2/WKWebView/WPE/WebKitGTK and surface frames the host renderer can consume.

The name comes from *scrying* — gazing into a reflective surface for visions. The webview is the surface; the captured frame is the vision; this crate is the lens.

This crate is the home for system-webview-backed frame production. It is deliberately separate from [`grafting`](https://github.com/merely-made/wgpu-graft) (sibling repo): Graft imports native GPU resources, while this adapter owns system-webview probing, fallback selection, synchronization policy, and platform-specific frame-source integration.

The crate defaults to wgpu 30 and also carries `wgpu-28` and `wgpu-29`
features. Pick the row matching the host, with default features disabled for
28 or 29. `scrying::wgpu` re-exports the selected version so public device and
texture types cannot silently come from a different major.

## Rust support and CI

The published library supports Rust 1.92 and has an exact 1.92.0 compile gate
on Windows, macOS, and Linux for each wgpu 28/29/30 public API row. Linux's
native-engine families are checked independently: WebKitGTK 4.1 fallback,
WPE, and WebKitGTK 6.0 each build in their own system-library environment.
They are deliberately not combined through `--all-features`, since WebKitGTK
6.0 selects an incompatible GTK/glib dependency family. This is a
library-only compatibility promise; the headed hardware workflows remain on
their Rust 1.97.1 lane.

## Current slice

The shared contract:

- `WebSurfaceMode` — imported texture, native child overlay, CPU snapshot, or unsupported.
- `WebSurfaceCapabilities` — platform/backend capability reporting.
- `WebSurfaceFrame` — imported native frame, CPU RGBA frame, PNG snapshot, or overlay-only state.
- `WebSurfaceProducer` — producer trait that platform implementations satisfy.
- `PlatformWebSurfaceProducer` / `PlatformWebSurfaceConfig` — cfg-selected aliases for the current target platform's primary concrete producer and config. Linux selects the WPE producer type; the `wpe` feature enables its runtime FFI. Its shared-fd DCC DMABUF path is pixel-verified on AMD Renoir/RADV.
- `OverlayOnlyProducer` — conservative fallback when no capture backend is available.

Platform selection is intentionally split:

- **scrying owns backend selection.** Platform modules, concrete producer aliases, and engine dependencies are `cfg(target_os = ...)` gated, so a Windows build selects WebView2, a macOS build selects WKWebView, and a Linux build selects the WPE type without compiling the other engine paths. Add `wpe` for the live WPEPlatform producer; without it the alias is a compile-only shell. WebKitGTK 4.1 and 6.0 remain opt-in Linux alternatives.
- **the host owns embedding.** The host still creates the window/event loop, supplies the native parent handle, chooses size/data-dir policy, and forwards native input/lifecycle events. Those responsibilities are application-specific and cannot be guessed reliably inside the library.
- **runtime capability probing stays layered on top.** `WebSurfaceCapabilities::probe` answers which surface modes are viable for the current GPU/OS/runtime after the target backend has been selected at compile time.

`WebSurfaceProducer` covers the full embeddable-webview surface, not just frame production:

- **Frame acquisition** — `acquire_frame`, plus producer-specific fast paths.
- **Layout** — `resize`, `set_offset`.
- **Navigation** — `navigate_to_string`, `navigate_to_url`. Both block until `NavigationCompleted`.
- **History** — `reload`, `stop`, `go_back`, `go_forward`, `can_go_back`, `can_go_forward`.
- **Input** — `send_mouse_input` (mouse + scroll + leave), `send_pointer_input` (touch / pen with pressure + tilt), `move_focus` (Programmatic / Next / Previous tab order). Drag-and-drop is implemented on the Windows producer's concrete type as `drag_enter` / `drag_over` / `drag_leave` / `drop_data` — the host supplies an `IDataObject` from its OLE drop-target callbacks. The trait-level `send_drag_input` stays platform-abstract; full cross-platform DnD waits for a unified data-carrier abstraction.
- **Lifecycle events** — `poll_navigation_event` drains a FIFO queue of `Starting` / `SourceChanged` / `Completed` / `TitleChanged` events.
- **Cursor reporting** — `poll_cursor_shape` returns the next [`CursorShape`] the engine wants the host to display (Pointer over a link, Text in an input, etc.).
- **JS messaging** — `post_web_message` (Rust → JS via `window.chrome.webview` listeners), `poll_web_message` (JS → Rust via `window.chrome.webview.postMessage`).
- **DevTools** — `open_devtools_window` opens the engine's developer-tools UI.
- **Settings** — `apply_settings(&WebSurfaceSettings)` accepts a partial update of zoom factor, user-agent string, JS-enabled, devtools-enabled, default-context-menus, and built-in accelerator keys. `None` fields are left at the producer's current value.
- **Profiles** — platform configs take a persistent data directory. `non_persistent()` switches supported producers into incognito/private mode so browser-shaped hosts can create temporary tiles without touching the persistent profile.
- **Snapshots** — `capture_snapshot_png` returns encoded PNG bytes via the underlying engine's preview API.

Methods that aren't yet implemented on a given platform return [`WebSurfaceError::Unsupported`] rather than panicking, so consumers can probe the surface incrementally.

Per-platform producer modules:

| Platform | Module | Status | Capture path |
| --- | --- | --- | --- |
| Windows | [`webview2_composition_producer`] | **Implemented.** Reference implementation; runtime-driven by [`demo-scrying-winit`]. | WebView2 CompositionController → `Windows.UI.Composition.Visual` → `Windows.Graphics.Capture` → shared D3D11 NT-handle texture → `wgpu` D3D12 import. |
| macOS | [`wkwebview_producer`] | **Implemented.** Runtime-driven by [`demo-mac`]. Slices A–N + the `MetalTextureRef` import path all exercised end-to-end. See [`design_docs/2026-05-07_platform_ceilings.md`](../design_docs/2026-05-07_platform_ceilings.md). | `WKWebView` hosted in NSView → `ScreenCaptureKit` stream bound to the host window → `CMSampleBuffer` → `IOSurfaceRef` → `MTLTexture` (via `MTLDevice::newTextureWithDescriptor:iosurface:plane:`) → `wgpu` Metal import (via `wgpu::hal::metal::Device::texture_from_raw`). |
| Linux | [`wpe_producer`], [`webkitgtk_producer`], [`webkit6_producer`] | **Implemented behind backend features.** `wpe` enables the headless WPEPlatform producer; its wgpu 30 DMABUF path is pixel-verified on AMD Renoir/RADV. WebKitGTK 4.1 and 6.0 are CPU-snapshot alternatives. Their `try_acquire_frame` path is deliberately non-blocking and returns `None`; call `acquire_frame` when a blocking CPU snapshot is acceptable. | `WPEWebView` + `WPEViewBackendDMABuf` → `DmaBufImage` → Graft's host-side Vulkan import. |

## Capability matrix

`WebSurfaceCapabilities::features` is the host-facing contract for browser
operations. Every field is an explicit `Supported`, `Partial` (with a caveat),
or `Unsupported` status. Hosts should inspect it before selecting a fallback.
The matrix covers cookie read/write/delete/change events and attributes,
script execution/results/exceptions and timeout behavior, page capture,
developer tools, downloads, popups, drag/drop, pointer input, IME, and
accessibility. `degradation_reasons` carries stable explanations for backend
limits such as host API mismatches, reduced pointer metadata, and GTK's
blocking CPU snapshot path.

The current honest limits are important: WebKitGTK and WPE do not expose
script results through the portable producer trait; WKWebView cannot open
Safari Web Inspector or synthesize capture-mode drag payloads; WebView2's
portable drag method cannot carry its required `IDataObject`; and none of the
four producers exports an accessibility tree. Cookie `SameSite` and
`Partitioned` fields are explicitly reported as unsupported on every current
backend, while secure, HttpOnly, and expiry attributes are reported
individually.

The Windows and macOS producers cover the producer/consumer split, lazy capture standup, lifecycle teardown, and platform-appropriate cross-API sync (D3D11 keyed-mutex on Windows; implicit IOSurface coherence + `MTLSharedEvent` scaffolding on macOS). The WPE producer exposes owned DMABUF frame metadata and duplicates its fds before releasing the WPE buffer back to the producer pool.

## Windows producer details

The Windows producer ([`webview2_composition_producer::WebView2CompositionProducer`]) owns the full WebView2 composition + WGC capture lifecycle:

- WebView2 environment + `ICoreWebView2CompositionController` + `ICoreWebView2Controller`
- `Windows.UI.Composition` compositor + desktop-window-target + root + WebView visuals
- `Windows.Graphics.Capture` item, frame pool, session
- Persistent shared D3D11 destination texture (`D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`) reused across frames; one allocation + one wgpu import per size change
- Lazy `start_capture` + bounded first-frame block + post-resize tear-down/rebuild + stall-detection escape hatch (`force_restart_capture`)
- Optional `WebView2CompositionConfig::non_persistent()` InPrivate mode for producers whose cookie, local-storage, and IndexedDB state should die with the controller instead of persisting into `user_data_dir`
- `NewWindowRequested` event routing for `target="_blank"` / `window.open(...)`, with the default WebView2 popup suppressed so the host owns tab creation
- `ProcessFailed` routing to `NavigationEvent::ContentProcessTerminated`, plus DevTools-protocol diagnostic calls for bounded crash/recovery smokes
- `register_virtual_host_handler(host, handler)` for app-owned `https://{host}/...` content via WebView2 `WebResourceRequested`, using the same `UrlSchemeResponse` body/header shape as macOS custom schemes
- `DownloadStarting` routing to `NavigationEvent::DownloadStarted` / `DownloadProgress` / `DownloadFinished` / `DownloadCancelled`, with `set_download_handler`, `cancel_download`, `pause_download`, `resume_download`, and `can_resume_download` for host-owned destinations and live WebView2 operation control. WebView2 does not expose a portable offline resume-data blob through this path, so cancelled/interrupted events still carry `resume_data: None`.
- `BasicAuthenticationRequested` routing to `NavigationEvent::AuthChallenged` plus `set_auth_handler` for host-supplied HTTP Basic credentials. Challenges matching an active download URL are reported as `AuthSource::Download`; WebView2 otherwise surfaces Basic auth at the WebView level.
- `PermissionRequested` routing through `set_permission_handler` for camera, microphone, and sensor-like prompts
- Browser-convenience APIs: native `find_in_page` / `poll_find_match`, native `request_pdf` / `poll_pdf` using `PrintToPdfStream`, and `print()` via WebView2's print UI.
- `ContextMenuRequested` routing through both WebView2's native `ContextMenuRequested` event and a document-start context-menu bridge, with default-menu suppression tied to `WebSurfaceSettings::default_context_menus_enabled`.
- `DropDetected` routing through a document-start `DataTransfer` bridge; real page delivery still uses the concrete OLE `IDataObject` drag/drop helpers.
- `MediaCaptureStateChanged` routing through a document-start `getUserMedia` observer that tracks active audio/video tracks.
- Cookie-change callbacks for host cookie writes/deletes, page-side `document.cookie` writes, and native `Set-Cookie` response headers observed through `WebResourceResponseReceived`.

`WebView2 TextureStream` is not treated as the primary path because it is a page/media texture stream API, not a whole-webview compositor-output API.

The lower-level building blocks live in [`windows_capture`]:

- `D3D11SharedTextureFactory::create_shared_texture_frame(...)` allocates an NT-handle-shareable D3D11 texture.
- `D3D11SharedTextureFactory::copy_capture_into_existing_target(...)` is the explicit-fence-only internal copy used by the composition producer. One-shot diagnostics use `copy_capture_into_shared_frame(...)`, which waits for the D3D11 copy before returning a fresh resource.
- `capture_graphics_item_frame_once(...)` and `capture_visual_frame_once(...)` are one-shot capture helpers used by the demo's startup probes.
- `DxgiSharedHandleBridge` wraps the `WebView2DxgiSharedHandleFrame` → `WebView2Dx12SharedFrame` → `WebSurfaceFrame::Native(NativeFrame::Dx12SharedTexture)` handoff.

## Fallbacks

`NativeChildOverlay` remains the normal native-overlay fallback on every platform. macOS supports `CpuSnapshot` end-to-end via `WKWebView.takeSnapshot` (synchronous via `capture_cpu_snapshot`, non-blocking via `request_snapshot` / `poll_snapshot`). WPE does not provide that tier; the WebKitGTK 4.1 and 6.0 alternatives do.

`CpuSnapshot` is useful for diagnostics, thumbnails, and low-frequency preview paths, but it is not the target for interactive composited web surfaces.

## macOS producer details

**Minimum macOS: 14.0 (Sonoma).** The producer hard-depends on `WKWebsiteDataStore::dataStoreForIdentifier:` (per-profile storage, macOS 14+) and `WKWebView::setInspectable:` (macOS 13.3+). It also uses `ScreenCaptureKit` (macOS 12.3+), `WKDownloadDelegate` (macOS 11.3+), and `WKWebView::interactionState` (macOS 12+). All of these are called unconditionally — there are no runtime-availability guards — so building or running against an older SDK / OS is unsupported. CI targets `macos-latest` (Apple Silicon, currently 14+) which matches.

The macOS producer ([`wkwebview_producer::WkWebViewProducer`]) was developed in slice-by-slice fashion. Slices A–N cover the core surface (lifecycle, SCK pipeline, navigation, mouse / scroll / keyboard, JS messaging, snapshots, KVO, cursor reporting, profile data store, MTLSharedEvent scaffolding, resize-applies-to-stream). Items 1–9 of the browser-class roadmap (history controls, new-window intercept, settings, custom URL schemes, process-failure recovery, auth pass-through, multi-instance, downloads, find + PDF) build on top to make scrying usable for browser-shape consumers. Both rosters are tracked in [`design_docs/2026-05-07_platform_ceilings.md`](../design_docs/2026-05-07_platform_ceilings.md) with API hooks and known limitations.

Browser-class additions on top of `WebSurfaceProducer`:

- **History.** `reload`, `stop`, `go_back`, `go_forward`, `can_go_back`, `can_go_forward` — straight `WKWebView` mappings.
- **New-window intercept.** `NavigationEvent::NewWindowRequested { url }` fires when a page tries to open a popup; the producer suppresses the engine-level popup so browser-shape consumers can route the URL into a new tab.
- **Settings.** `apply_settings(&WebSurfaceSettings)` applies zoom factor, custom user-agent, JS-enabled, and devtools (via `setInspectable`, macOS 13.3+).
- **Custom URL schemes.** `WkWebViewProducer::new_with_url_schemes(parent, config, schemes)` registers `WKURLSchemeHandler`s on the configuration. Each scheme handler is a closure `Fn(&str) -> UrlSchemeResponse + Send + Sync`.
- **Process-failure recovery.** `NavigationEvent::ContentProcessTerminated` fires when the WebKit content process crashes; the WKWebView is reusable via `producer.reload()` or another `load_url`.
- **Auth.** `NavigationEvent::AuthChallenged { url, host, auth_method }` fires when the engine receives an auth challenge. With no handler the producer responds with `PerformDefaultHandling` (system keychain / interactive prompts); register a `Fn(AuthChallenge) -> AuthDisposition` via `set_auth_handler` to drive the disposition yourself (HTTP basic via `AuthDisposition::UseCredential { username, password }`, server-trust override, etc.). The same handler also covers `WKDownloadDelegate::download:didReceiveAuthenticationChallenge:` for both promotion-driven and `start_download`-initiated transfers.
- **Permissions.** `set_permission_handler` registers a `Fn(PermissionRequest) -> PermissionDecision` for camera / microphone / device-orientation requests; default with no handler is `Prompt` (system UI).
- **Cookies.** `request_all_cookies` + `poll_cookies` (async fetch), `set_cookie(&Cookie)` / `delete_cookie(name, domain, path)` (fire-and-forget). Wraps the producer's `WKHTTPCookieStore`.
- **Incognito.** `WkWebViewProducerConfig::non_persistent` (or `.non_persistent()` builder) wires `WKWebsiteDataStore::nonPersistentDataStore` — cookies / local storage / IndexedDB live only for the producer's lifetime.
- **Tab restoration.** macOS `serialize_interaction_state() -> Option<Vec<u8>>` + `restore_interaction_state(&[u8])` round-trip WebKit's `interactionState` blob (back-forward list, scroll position, form data). Windows exposes same-named methods for cross-platform call sites, but WebView2 has no opaque blob equivalent: serialize returns `None`, restore returns `Unsupported`.
- **Downloads.** `NavigationEvent::DownloadStarted` / `DownloadProgress` / `DownloadFinished` / `DownloadCancelled` carry a `DownloadId` so concurrent downloads correlate cleanly. Progress is throttled (100ms / 1MiB per download); a final emit on completion always lands. `set_download_handler` lets the host pick destinations or cancel via `DownloadDecision`; `cancel_download(id)` cancels in-flight transfers. macOS surfaces WebKit `resume_data` on resumable cancellations and `resume_download(&[u8], PathBuf)` restarts from `resumeDownloadFromResumeData:`. Windows WebView2 exposes live `pause_download(id)` / `resume_download(id)` / `can_resume_download(id)` while the operation exists, but cancelled downloads report `resume_data: None` because WebView2 exposes no portable offline resume-data blob. Defaults: `<config.download_dir>/<suggested_filename>` with `-N` collision suffixing.
- **Find / PDF.** `find_in_page(query, FindOptions)` + `poll_find_match() -> Option<bool>` and `request_pdf()` + `poll_pdf() -> Option<Result<Vec<u8>, String>>` are async, mirroring the snapshot pattern.
- **DPI awareness.** An `NSWindowDidChangeBackingPropertiesNotification` observer re-applies `config.size` on the next `try_acquire_frame` / `resize` so points/pixels stay coherent across monitor moves. No host-side wiring needed.
- **Cursors.** `set_cursor_handler` registers a `Fn(CursorShape) + Send + Sync` callback invoked synchronously on every system-cursor change observed after a forwarded input event. Coexists with the pull-model `poll_cursor_shape` queue — both fire on the same change so hosts can mix push and pull.
- **Pointer input.** `WebSurfaceProducer::send_pointer_input` synthesizes touch / pen events through the same path as `send_mouse_input`; WebKit's pointer-events JS API observes them as `pointerType: "mouse"` because macOS has no public direct-touch synthesis API.

Key cross-API GPU-sync notes:

- The `MetalTextureRef` import path is the analog of the Windows D3D12 shared-handle path. Scry creates the producer texture and delegates the raw `MTLTexture *` → `wgpu::Texture` boundary to Graft.
- IOSurface has implicit cross-API cache coherence on Apple silicon and via IOSurface locks on Intel, so today's correctness model doesn't require an explicit fence. A `MetalSharedEventSynchronizer` (parallel to `Dx12FenceSynchronizer`) is scaffolded but inert; ScreenCaptureKit doesn't expose its render queue, so there's no producer-side hook to drive a signal from. The infrastructure is ready for when SCK extends or a downstream consumer wires manual signal points.

Critical caveat for event-loop hosts: blocking entry points (`navigate_to_url`, `navigate_to_string`, `start_capture`, `capture_cpu_snapshot`) pump the main `NSRunLoop` and **must not be called from inside a host event-loop callback** (winit's `resumed` / `window_event` etc.) — the pump re-enters the host's dispatch and panics. Each blocking method's docstring carries a `⚠️` warning and a pointer to the non-blocking equivalent (`load_url` / `load_html`, `start_capture_async` + `capture_status`, `request_snapshot` + `poll_snapshot`).

## Cross-API GPU sync (Windows)

The composition producer allocates a new `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED` destination for every emitted frame. Its host must create a `Dx12FenceSynchronizer`, configure the producer with `with_dx12_fence_synchronizer`, and import using that same synchronizer.

1. The producer copies the WGC capture output and signals the host-created `D3D12_FENCE_FLAG_SHARED` fence with a monotonic value.
2. Before importing, the host synchronizer queues `ID3D12CommandQueue::Wait(fence, value)`.
3. The producer never writes that destination again, so an in-flight host render cannot race the next capture copy.

The keyed-mutex path remains only for one-shot diagnostics that complete their D3D11 copy before returning their fresh resource. It is not a live composition-texture fallback.

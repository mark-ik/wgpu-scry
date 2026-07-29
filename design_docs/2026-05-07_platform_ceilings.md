# Platform ceilings and parity roadmap

**Status:** living document. Last refreshed 2026-05-14 against post-0.4.x source. The Linux section is superseded by [`2026-05-14_linux_webkitgtk_phase_2a.md`](2026-05-14_linux_webkitgtk_phase_2a.md), which records the three-co-equal-backends decision and Phase 2a outcomes.

This records, for each of the three target platforms, the **upper bound of
what the platform's native webview can deliver to a host wgpu pipeline**,
the **current state of scrying's implementation**, and the **gap to close**
to hit cross-platform parity.

The intent is not "implement everything everywhere", it's:

1. Know the ceiling so we don't accidentally accept a lower one.
2. Pick a parity baseline that's achievable on all three without
   architecture-bending workarounds.
3. Surface platform-specific upgrades (fences, IME, DMABUF) as
   distinct slices that can ship independently of the parity baseline.

Platform-specific implementation targets live in focused companion docs when
the detail gets too deep for this overview. Windows WebView2's target and
remaining lane are split into
[`2026-05-11_windows_webview2_target.md`](2026-05-11_windows_webview2_target.md).

---

## Per-platform ceilings

### Windows — WebView2 + WGC + D3D11/D3D12

**Capture path (what we ship):** ICoreWebView2CompositionController →
Windows.UI.Composition.Visual → Windows.Graphics.Capture →
shared D3D11 NT-handle texture → wgpu D3D12 `OpenSharedHandle`
import, with optional shared D3D12 fence sync.

**Ceiling:**

- **Frame rate / latency**: capped by the system compositor. Sample
  rate = display refresh rate (typically 60 / 120 / 144 Hz). Latency =
  ~1–2 compositor frames (16–33ms at 60 Hz) between WebView paint and
  importable wgpu texture. Cannot go below 1 frame without Microsoft
  shipping a non-WGC export path (long-standing open WV2Feedback ask;
  no commitment).
- **Pixel quality**: post-DComp composited output. Pixel-exact under
  the WebView's own rasterization. Cannot get pre-composition raw
  layer textures.
- **GPU sync** (today): explicit `D3D12_FENCE_FLAG_SHARED` sync is
  wired when the host passes a fence handle through
  `WebView2CompositionConfig::with_fence_shared_handle`. The older
  keyed-mutex / transition-barrier path remains the fallback when no
  fence is supplied.
- **Input**: full mouse + scroll (`SendMouseInput`), full touch + pen
  (`SendPointerInput`), focus entry (`MoveFocus`) with focus-event
  hooks available at the WebView2 ceiling, drag-and-drop forwarding
  (`DragEnter`/`DragOver`/`DragLeave`/`Drop`), and cursor-change reporting
  (`add_CursorChanged`) on pure visual hosting. Keyboard/IME is a pure
  CompositionController ceiling. A limited no-overlay fallback can set the
  focused DOM input value, post through the WebView message bridge, and drive a
  hash navigation-style state change via CDP/`ExecuteScript` DOM automation.
  The CDP diagnostic explicitly probes `Input.dispatchKeyEvent`,
  `Input.insertText`, and `Input.imeSetComposition`; the 2026-05-13 smoke
  reports DOM input observed for those routes plus `Runtime.evaluate` and
  `ExecuteScript`. Those are synthetic input routes, not OS IME/candidate UI
  ownership. Host accelerators are separate: WebView2 `AcceleratorKeyPressed`
  is now surfaced through `NavigationEvent::AcceleratorKeyPressed`, and
  `demo-win --accelerator-test` observes F3 in pure CompositionController mode.
  The first host-owned IME bridge is also proven: WebView2 emits focused
  editable/caret state, the winit host sets native IME enablement and candidate
  area from that state, and CDP `imeSetComposition` / `insertText` apply the
  renderer-side composition/commit. The hardened bridge suppresses
  password-like editables before emitting selection/caret metadata, validates
  textarea scroll/multiline caret placement, maps through the current
  composition offset and host scale factor, and cancels composition on
  blur/navigation boundaries. This is a baseline bridge, not full TSF
  `ITextStoreACP2` parity.
  Window-to-Visual is proven for ASCII DOM input and parent-HWND WGC/wgpu
  import, including three simultaneous animated pages,
  but the live controllers report visible `Chrome_WidgetWin_0` child HWNDs, so
  it is not accepted as the no-overlay composition target.
- **Navigation / lifecycle**: complete. URL + HTML, back/forward/
  stop/reload, NavigationStarting/SourceChanged/NavigationCompleted/
  DocumentTitleChanged events, ProcessFailed for crash recovery.
- **JS interop**: full (`PostWebMessageAsString`, `WebMessageReceived`,
  and `AddScriptToExecuteOnDocumentCreated`).
- **Settings / environment ceiling**: zoom, user agent, IsVisible
  (Page Visibility / throttling control), profile + cookie store
  (`WKWebsiteDataStore` analog), custom URL schemes via
  `WebResourceRequested`, downloads, auth / permissions, new-window
  interception, DevTools, print + print-to-PDF.
- **Snapshots**: `CapturePreview` (PNG/JPEG) — already wired in 0.2.0.
- **Out of reach without MSFT API additions**: pre-composition
  extraction, sub-iframe capture, capture while visual is hidden
  from the composition tree, sub-frame latency.

**Current scrying state (post-0.4.x):** frame production complete.
Embeddable surface shipped for imported GPU frames, resize / offset,
URL + HTML navigation, mouse + scroll, pointer / touch / pen, focus,
navigation + title events, JS messaging, PNG snapshots, history controls,
settings, DevTools, per-profile user-data directory, OLE drag-in helpers,
cursor-change reporting, CDP-backed keyboard/text dispatch and
accelerator-key events for pure CompositionController hosting, baseline
host-owned IME bridge events/candidate placement, and optional explicit D3D12
fence sync.

The remaining Windows work is no longer "make the WebView2 producer
real"; it is a browser-shell completion and capture-polish lane. The
target is a production-quality WebView2 host surface plus robust
post-composition WGC texture import, not pre-composition renderer access.
See
[`2026-05-11_windows_webview2_target.md`](2026-05-11_windows_webview2_target.md)
for the audited implementation state, runtime proof, hard ceilings, and
Windows-specific W1-W5 lane.

---

### macOS — WKWebView + ScreenCaptureKit + IOSurface + Metal

**Capture path (planned):** `WKWebView` hosted in `NSView` →
`ScreenCaptureKit` content-filter bound to that view (or to an offscreen
`NSWindow`) → `CMSampleBuffer` carrying an `IOSurface` →
`MTLTexture` via `CVMetalTextureCacheCreateTextureFromImage` → wgpu
Metal import.

**Ceiling:**

- **Frame rate / latency**: similar to Windows. SCK samples per
  compositor frame; latency is 1–2 frames depending on display
  configuration. ProMotion / 120 Hz handled natively.
- **Pixel quality**: post-composition, pixel-exact. Same constraint
  as Windows: cannot get pre-composition raw layer output.
- **GPU sync**: IOSurface has implicit cross-API coherence on Apple
  silicon (unified memory) and via IOSurface locks on Intel. Cleaner
  than the Windows path — there's no equivalent of the keyed-mutex /
  transition-barrier dance. **Upgrade option: explicit
  `MTLSharedEvent`** between producer and consumer command queues if
  empirical coherence ever breaks (analog of the Windows fence work).
- **Input**: WKWebView is a normal `NSView`. In *captured-and-rendered-
  elsewhere* mode, the host forwards events to the WKWebView via the
  NSResponder API: `mouseDown:`, `mouseUp:`, `mouseMoved:`,
  `scrollWheel:`, `keyDown:`, `keyUp:`, `flagsChanged:`. Touch via
  NSTouch (limited), gestures via NSEvent. **IME**: NSTextInputClient
  protocol — Apple's IME story is more uniform than Windows' but still
  non-trivial.
- **Cursor**: `NSCursor.set(_:)` plus the WKWebView's
  `mouseMoved`/cursor-update protocol; clean.
- **Navigation / lifecycle**: complete. `WKNavigationDelegate` covers
  every nav event; `WKUIDelegate` for popups/permissions; KVO on
  `title` / `url` / `estimatedProgress` for state.
- **JS interop**: full bidirectional via `WKUserContentController` +
  `WKScriptMessageHandler` (JS → host) and `evaluateJavaScript` /
  `callAsyncJavaScript` (host → JS).
- **Settings / environment**: per-`WKWebsiteDataStore` cookies +
  storage isolation, custom URL schemes (`WKURLSchemeHandler`),
  downloads (`WKDownload` 11.3+), `WKPreferences` for JS / dev tools /
  fraudulent-content-warning, user agent, content rules
  (`WKContentRuleList`).
- **Snapshots**: `takeSnapshot(with:completionHandler:)` returns an
  `NSImage`. Encode to PNG via `NSBitmapImageRep`. Already documented
  as a fallback advertise; needs wiring.
- **Out of reach without Apple API additions**: pre-composition layer
  textures, capture-while-hidden (SCK requires the view to be in a
  window), sub-iframe capture.

**Current scrying state (0.4.0+):**
The macOS producer is a real working WebView, not a skeleton.
Implementation landed in five cohesive slices on top of the
[`native_frame::metal`](../scrying/src/native_frame/metal.rs)
`MTLTexture` → `wgpu::Texture` import (initial M2 work, lifted
structurally from wgpu-graft and adapted to the wgpu-hal 29 API drift
where `texture_from_raw` now takes
`Retained<ProtocolObject<dyn MTLTexture>>` directly):

- **Slice A — WKWebView lifecycle.** `WkWebViewProducer::new` retains
  the parent `NSView`, builds a default `WKWebViewConfiguration`,
  creates the `WKWebView` with an NSRect derived from
  `config.offset` and `config.size` (both physical pixels → both
  divided by the parent window's `backingScaleFactor` to get
  AppKit points), wires a navigation delegate, and adds the
  WebView as a subview.
  `navigate_to_string` waits on `WKNavigationDelegate.didFinishNavigation:`
  while pumping the main run loop in 16 ms slices; `resize` /
  `set_offset` reshape the live view; `Drop` removes from superview
  and clears the delegate.
- **Slice B — ScreenCaptureKit pipeline.**
  [`WkWebViewProducer::start_capture(host, timeout)`](../scrying/src/wkwebview_producer.rs)
  pulls the host wgpu's `MTLDevice` via
  `as_hal::<Metal>().raw_device().clone()`, walks
  `SCShareableContent.windows` for the WKWebView's host
  `NSWindow.windowNumber`, builds an
  `SCContentFilter::initWithDesktopIndependentWindow:`, configures an
  `SCStream` for `kCVPixelFormatType_32BGRA` /
  `setShowsCursor(false)` / `setQueueDepth(3)`, attaches custom
  `SCStreamDelegate` + `SCStreamOutput` delegates on a dedicated
  `DispatchQueue`, and blocks on `startCaptureWithCompletionHandler:`.
  `try_acquire_frame` then takes the latest `CMSampleBuffer`,
  extracts `IOSurfaceRef` via `CVPixelBufferGetIOSurface`, wraps it
  as `MTLTexture` on the host device, and emits
  `WebSurfaceFrame::Native(NativeFrame::MetalTextureRef(...))`.
  A small `SendCFRetained<T>` newtype wraps the dispatch-queue →
  main-thread sample handoff. `acquire_frame` is blocking — pumps
  the run loop until a sample arrives or `frame_timeout` elapses.
- **Slice C — navigation parity.** `navigate_to_url` loads a URL via
  `loadRequest:`. The navigation delegate now fires `Starting` /
  `SourceChanged` / `Completed` events into a FIFO drained by
  `poll_navigation_event`. `move_focus` sends the WKWebView to
  first-responder via the host `NSWindow`.
- **Slice D — mouse forwarding.** `send_mouse_input` synthesizes an
  `NSEvent` (window-coordinates, points, bottom-left origin via
  `convertPoint_toView(None)`), and dispatches directly through the
  WKWebView's NSResponder slots — `mouseDown:` / `mouseUp:` /
  `mouseDragged:` / `mouseMoved:` / `rightMouse*` / `otherMouse*` /
  `mouseExited:`. `Move` differentiates dragged-with-button from
  plain `MouseMoved` based on `MouseVirtualKeys` button state;
  `DoubleClick` rides on `clickCount = 2`. Scroll wheel and
  X-button `buttonNumber` distinction are deferred (need the
  `CGEvent` path).
- **Slice E — bidirectional JS messaging.**
  `WKUserContentController` is pre-loaded on the configuration with
  a `WKScriptMessageHandler` named `scryingHostBridge` and a
  document-start user script that builds a `window.chrome.webview`
  shim. JS-side `window.chrome.webview.postMessage(s)` lands in a
  FIFO drained by `poll_web_message`; host-side `post_web_message(s)`
  runs an `evaluateJavaScript:` with a JSON-encoded literal that
  dispatches to listeners registered via
  `window.chrome.webview.addEventListener('message', ...)`. The shim
  is idempotent and the JS API matches WebView2's surface so
  consumers can write portable bridge code.
- **Slice F — CPU snapshots.** `capture_cpu_snapshot` runs
  `takeSnapshotWithConfiguration:completionHandler:` (main-thread
  callback, no Screen Recording permission needed), pumps the run
  loop until the NSImage arrives or `config.frame_timeout` elapses,
  and decodes the `NSImage::TIFFRepresentation` through the `image`
  crate's TIFF decoder into an `RgbaImage` returned as
  `WebSurfaceFrame::CpuRgba`. Independent of `start_capture` —
  works as a fallback diagnostic path, useful for thumbnails or for
  verifying the WebView is rendering before standing up the SCK
  pipeline.
- **Slice G — scroll wheel via CGEvent.** `MouseEventKind::Wheel` /
  `HorizontalWheel` build a `CGEventCreateScrollWheelEvent2` (pixel
  units, AppKit sign convention) and convert through
  `NSEvent::eventWithCGEvent:` before dispatching to
  `webview.scrollWheel:`. Removes the only "deferred" caveat from
  slice D's mouse forwarding.
- **Slice H — title-changed events via KVO.** A `TitleObserver`
  NSObject subclass is registered as a `title` key-path observer on
  the WKWebView at construction time (`addObserver:forKeyPath:options:context:`
  with `NSKeyValueObservingOptions::New`). When the page mutates
  `document.title` after the initial load, the KVO callback pushes
  `NavigationEvent::TitleChanged { title }` into the same FIFO the
  navigation delegate writes to. `Drop` calls `removeObserver:`
  before any retained references cascade so the observed object
  outlives its observer registration.
- **Slice I — keyboard forwarding (with IME baseline).**
  `send_keyboard_input` synthesizes an `NSEvent` via
  `keyEventWithType:...:characters:charactersIgnoringModifiers:isARepeat:keyCode:`
  and dispatches through the WKWebView's `keyDown:` / `keyUp:` /
  `flagsChanged:` slots. `characters` flows through to WebKit's
  `NSTextInputClient` implementation, so IME composition (CJK, dead
  keys, marked text) works without explicit composition-state
  threading on the host side — the host just forwards whatever the
  windowing system reports.
- **Slice J — cursor-change reporting.** After each forwarded
  pointer event, `observe_cursor_change` reads
  `NSCursor.currentSystemCursor` and compares against the canonical
  cursor singletons (`arrowCursor`, `IBeamCursor`,
  `pointingHandCursor`, `crosshairCursor`, `openHandCursor`,
  `closedHandCursor`, `operationNotAllowedCursor`, etc.) to translate
  the WebKit-set cursor into a [`CursorShape`]. Only changes are
  queued, so `poll_cursor_shape` reflects "the engine wants the host
  to display X" without spamming `Default` events.
- **Slice K — drag-and-drop forwarding (documented constraint).**
  `WKWebView` receives drag/drop via the `NSDraggingDestination`
  protocol, whose callbacks require an `NSDraggingInfo` parameter.
  `NSDraggingInfo` instances are constructed only by AppKit's drag
  manager — there is no public API to synthesize one. So
  `send_drag_input` for **capture mode** is genuinely not feasible
  without SPI. **Overlay mode** is automatic — AppKit's drag manager
  delivers drags to the WKWebView through the responder chain
  without producer involvement, so the host doesn't need to forward.
  Producer returns `WebSurfaceError::Unsupported` with a message
  that explains both branches.
- **Slice L — per-profile `WKWebsiteDataStore`.** When
  `config.data_dir` is non-empty, the producer derives a stable
  version-8 UUID from the path's bytes via FNV-1a 128 and resolves
  a per-profile persistent store through
  `WKWebsiteDataStore::dataStoreForIdentifier:` (macOS 14+). Empty
  `data_dir` keeps the shared default store. macOS doesn't take an
  arbitrary path for data stores (storage lives in the app
  container by UUID); the deterministic-UUID-from-path scheme is the
  native analog of the per-directory profile model.
- **Slice M — `MTLSharedEvent` synchronizer scaffolding.** New
  `SyncMechanism::ExplicitMetalEvent` variant and a
  `MetalSharedEventSynchronizer` skeleton in
  [`scrying::native_frame`](../scrying/src/native_frame/sync_metal.rs)
  parallel to the Windows `Dx12FenceSynchronizer`. Currently a no-op
  (accepts both `None` and `ExplicitMetalEvent` without
  waiting/signalling) because ScreenCaptureKit doesn't expose its
  render queue for explicit fencing. Infrastructure is in place for
  when Apple extends SCK or a downstream consumer wires manual CPU
  signal points.
- **Slice N — `SCStreamConfiguration` auto-update on resize.**
  `resize` now pushes the new pixel dimensions through to the live
  stream via `stream.updateConfiguration:completionHandler:` (with
  the same non-size params as the original `start_capture` —
  encapsulated in a single `make_stream_configuration` helper so the
  two paths stay consistent). SCK samples post-resize arrive at the
  requested resolution without restarting the stream.

The producer struct accumulates over the slices: parent NSView,
`WKWebView`, navigation delegate (with shared `NavState` carrying both
the navigation completion signal and the events FIFO), script-message
handler, web-message FIFO, and an `Option<CaptureState>` that's `Some`
once `start_capture` has resolved. Capabilities flip from
`NativeChildOverlay` (default) to `ImportedTexture` when capture
starts, and back on `stop_capture` / `Drop`.

Threading: SCK output callbacks fire on a background dispatch queue
and write the latest sample into a `Mutex<Option<SendCFRetained<CMSampleBuffer>>>`
that `try_acquire_frame` reads on the main thread; the
`SCShareableContent` async resolution carries a similar
`unsafe impl Send` wrapper around the matched `Retained<SCWindow>`.

**Windows-0.2.0 parity status:** ✅ achieved by slices A–H. Slices
I–N pushed the macOS producer well past the 0.2.0 baseline into
0.3.0 / 0.4.0 territory: keyboard + IME, cursor reporting,
per-profile data stores, MTLSharedEvent infrastructure, and resize-
applies-to-stream are all live; drag-and-drop is documented as
SPI-blocked.

**Remaining limitations:**

- Drag-and-drop forwarding in capture mode (SPI-required).
- X-button `buttonNumber` distinction (X1/X2 arrive as Other-mouse
  with default index; CGEvent would fix this in a small follow-up).
- `MTLSharedEvent` is scaffolded but inert — needs a producer-side
  signal hook that ScreenCaptureKit's public API doesn't expose
  today. Implicit IOSurface coherence remains the contract.

---

### Linux — three co-equal WebKit-family backends

Three backends, three ceilings, all selected via mutually-exclusive
cargo features. WebKitGTK 4.1 (GTK 3) has the widest installed base
today — Tauri 1/2 still targets it, every Tauri-shipped Linux app
pulls it in, and legacy GNOME/GTK 3 embedders use it. WPE is the
strategically strongest GPU-handoff path (DMABUF + VkSemaphore on
Vulkan). WebKitGTK 6.0 (GTK 4 + libadwaita) is the modern GNOME line
gaining adoption. Build order is the installed-base order — WebKitGTK
4.1 first, then WPE, then WebKitGTK 6.0 — see
[`2026-05-14_linux_webkitgtk_phase_2a.md`](2026-05-14_linux_webkitgtk_phase_2a.md)
for the rationale for treating these as co-equal rather than
primary/fallback.

#### WPE + DMABUF + Vulkan

**Capture path (planned):** `WPEWebView` with `WPEViewBackendDMABuf` →
per-frame DMABUF fd + format/modifier + `VkSemaphore` for ordering →
wgpu Vulkan `VK_KHR_external_memory_fd` import.

**Ceiling:**

- **Frame rate / latency**: potentially the *lowest* of the three
  platforms. WPE renders directly into DMABUFs without going through
  a compositor capture step — the DMABUF fd is the WebView's render
  output. Latency = WebView paint completion + Vulkan import. No
  extra compositor-frame buffering.
- **Pixel quality**: pre-composition (this is the only platform where
  that's true). The WebView paints directly into the DMABUF; if
  scrying's host wants additional compositing, it does it with the
  imported texture.
- **GPU sync**: VkSemaphore is the cross-API contract. Producer
  signals on render completion, consumer waits before sampling.
  Standards-correct out of the box — no driver-empirical hacks.
- **Input**: WPE has a clean `wpe_view_backend_dispatch_*_event` API
  for keyboard, pointer, axis, touch. Host serializes platform events
  (winit / wayland / xkbcommon) into WPE event structs and dispatches.
  No window subclass. **IME**: Wayland text-input-v3 / XIM via
  xkbcommon — solvable but compositor-dependent.
- **Cursor**: WPE reports cursor changes via the view backend; host
  translates to the windowing system's cursor.
- **Navigation / lifecycle**: complete. Same WebKit GTK-family API
  surface (WebKitWebView, signals for load-changed,
  load-failed, document-loaded, title-changed; WebKitNavigationPolicyDecision).
- **JS interop**: `webkit_web_view_run_javascript_in_world` (host →
  JS) + `WebKitUserContentManager` script messages (JS → host).
- **Settings / environment**: `WebKitSettings` (zoom, JS, dev tools,
  user agent), `WebKitWebsiteDataManager` (cookies / storage),
  `webkit_security_manager_register_uri_scheme_as_*` for custom
  schemes, downloads via WebKitDownload.
- **Snapshots**: `webkit_web_view_get_snapshot` → cairo surface →
  encode.
- **Out of reach**: less than the other platforms because WPE is
  designed for embedding; the main "out of reach" is platform-level
  things like printing (less well-developed than WebView2) and
  Wayland/X11 abstraction edges.

#### WebKitGTK 4.1 (GTK 3) — offscreen CPU snapshot (today) + future GPU upgrades

**Capture path (today, Phase 2a):** `WebKitWebView` as a `GtkWidget`
in a producer-owned `GtkOffscreenWindow` → `webkit_web_view_get_snapshot`
async → `cairo::ImageSurface` (ARGB32) → un-premultiplied RGBA →
[`WebSurfaceFrame::CpuRgba`](../scrying/src/lib.rs). Latency is
engine-bound (≥50 ms per snapshot); the tier is honestly `CpuSnapshot`,
not `ImportedTexture`.

**Future GPU capture upgrades (deferred):** `zwlr_screencopy_manager_v1`
on wlroots-class compositors → wl_buffer (DMABUF or shm) → wgpu import;
or WebKitGTK's accelerated-compositing DMABUF renderer (2.46+) →
DMABUF fd → Vulkan import. Same Vulkan import plumbing as the WPE
producer — see Phase 4. Compositor-dependent.

**Ceiling (lower than WPE):**

- **Frame rate / latency**: depends on capture path. Screencopy adds
  a compositor-roundtrip frame. PaintCallback / CPU readback is the
  worst.
- **Pixel quality**: same as WPE for the rendered output, but with
  GTK widget chrome possibly leaking in if not isolated.
- **GPU sync**: screencopy gives a wl_buffer; the protocol implies
  ready-on-receive but lacks an explicit semaphore. Cleaner than
  Windows pre-fence but less than WPE's VkSemaphore.
- **Input**: GTK widget event forwarding — automatic if the widget
  has focus, but in capture-and-render-elsewhere mode requires
  forwarding gdk events to the offscreen widget, which is awkward.
- **Compositor compatibility**: zwlr_screencopy_manager_v1 is wlroots
  (Sway, river, Hyprland, KDE on Wayland with extension, GNOME via
  XDG portals). X11 / non-wlroots compositors fall back to the cairo
  snapshot path.

**Strategic position:** the three backends serve overlapping but
distinct downstream shapes. WebKitGTK 4.1 meets Tauri-shaped
consumers where they are today and is the load-bearing default on
Fedora / Ubuntu LTS / similar packaged distributions. WPE is the
load-bearing choice when GPU-handoff frames matter and the
deployment can ship WPE + `wpebackend-fdo`. WebKitGTK 6.0 follows
GNOME's GTK 4 trajectory as that ecosystem catches up. Pick the
feature that matches your packaging.

**Current scrying state (post-Phase 2a):**
[`webkitgtk_producer`](../scrying/src/webkitgtk_producer/) is **shipping**
on the `webkitgtk-fallback` feature — offscreen WebView,
`webkit_web_view_get_snapshot` → `CpuRgba`, navigate + resize +
load-event signal handlers wired. The
[`demo-linux`](../demo-linux/) workspace member exercises the path
end-to-end on a Wayland session.
[`wpe_producer`](../scrying/src/wpe_producer.rs) is still the
DMABUF scaffold pending Phase 4 (FFI bridge + Vulkan import).
WebKitGTK 6.0 (`gtk4` + `webkit6` siblings) is deferred to Phase 5+.

---

## Cross-platform parity matrix

What every platform's producer should implement to hit the **parity
baseline**. Every row that's `?` on a platform is a gap to close;
every row marked `—` is structurally not on that platform.

Linux ships three co-equal WebKit-family backends; each gets its own
column. WebKitGTK 4.1 reflects Phase 2a (post-2026-05-14). WebKitGTK
6.0 and WPE remain `?` pending Phase 5+ and Phase 4 respectively.

| Capability | Windows WV2 | macOS WKWebView | Linux WebKitGTK 4.1 | Linux WebKitGTK 6.0 | Linux WPE |
| --- | --- | --- | --- | --- | --- |
| Imported GPU texture per frame | ✅ 0.1.0 | ✅ 0.4.0 | — (CPU-snapshot tier today) | — (CPU-snapshot tier today) | ? |
| Resize / offset | ✅ | ✅ 0.4.0 | ✅ Phase 2a | ✅ Phase 5 first slice | ? |
| Navigate (URL + HTML) | ✅ 0.2.0 | ✅ 0.4.0 | ✅ Phase 2a | ✅ Phase 5 first slice | ? |
| Reload / Stop / Back / Forward | ✅ | ✅ | ✅ Phase 2b | ? | ? |
| Mouse forwarding (buttons + move + leave) | ✅ 0.2.0 | ✅ 0.4.0 | ✅ Phase 2c (native `GdkEvent` via `gtk_main_do_event`, page handlers see `isTrusted = true`) | ? | ? |
| Scroll wheel forwarding | ✅ 0.2.0 | ✅ 0.4.0 | ✅ Phase 2c (native `GdkEventScroll`, smooth-scroll path) | ? | ? |
| Touch + pen forwarding | ✅ | ✅ 0.4.x (mouse-shaped JS pointer events) | 🟡 Phase 2c (mouse-shaped native `GdkEvent`; true `EventTouch` per-sequence tracking deferred) | ? | ? |
| Keyboard forwarding (basic) | 🟡 (CDP-backed `send_keyboard_input` works in pure visual hosting; Window-to-Visual input passes but exposes visible child HWNDs) | ✅ 0.4.0 | ✅ Phase 2c (native `GdkEventKey`, keyval via `gdk_unicode_to_keyval`, `isTrusted = true`) | ? | ? |
| IME (CJK / non-Latin) | 🟡 (baseline host-owned bridge proven: focused editable/caret events -> winit IME area -> CDP composition/commit; full TSF text-store parity remains future work) | ✅ 0.4.0 (via NSTextInputClient) | 🟡 Phase 2e (focused-editable state observability via JS user-script: TextInputFocused / TextInputChanged / TextInputBlurred with full TextInputState; `send_text(str)` inherent commit-forwards per-char as isTrusted-true native key events; full `GtkIMContext` preedit bridge still pending) | ? | ? |
| Drag-and-drop into webview | ✅ (concrete OLE `IDataObject` helpers; trait reports data-object requirement) | — capture (SPI-blocked) / ✅ overlay (auto) | 🟡 Phase 2e (JS `DragEvent` synthesis; `event.dataTransfer.files` is empty because there's no real drag source. Native `GdkEventDND` deferred — requires a `GdkDragContext` we can't fabricate without a drag origin widget) | ? | ? |
| Focus management | ✅ 0.2.0 | ✅ 0.4.0 | ✅ Phase 2b (`Widget::grab_focus` + JS `document.body.focus()`) | ? | ? |
| Cursor-change reporting | ✅ | ✅ 0.4.0 | ✅ Phase 2e (semantic `CursorShape` from WebKit's `mouse-target-changed` hit-test context; covers Pointer / Text / Default) | ? | ? |
| Navigation events (start/source/complete) | ✅ 0.2.0 | ✅ 0.4.0 | ✅ Phase 2b (`poll_navigation_event` draining FIFO from `load-changed` / `load-failed`) | ? | ? |
| Title-changed event | ✅ 0.2.0 | ✅ 0.4.0 (KVO) | ✅ Phase 2b (`notify::title` signal → `NavigationEvent::TitleChanged`) | ? | ? |
| JS messaging (bidirectional) | ✅ 0.2.0 | ✅ 0.4.0 | ✅ Phase 2b (script-message handler + injected `window.chrome.webview` shim; round-trip verified by `demo-linux --scripted`) | ? | ? |
| PNG / CPU snapshot | ✅ 0.2.0 | ✅ 0.4.0 (CPU RGBA) | ✅ Phase 2a (CpuRgba) + ✅ Phase 2b (`capture_snapshot_png`) | ✅ Phase 5 first slice (CpuRgba via `webkit_web_view_get_snapshot` → `gdk::Texture::download` → un-premultiplied RGBA; opacity=0 window kept mapped so WebKit's renderer engages) | ? (`get_snapshot`) |
| Settings (zoom, UA, JS, devtools) | ✅ | ✅ | 🟡 Phase 2b (zoom / JS-enabled / devtools / UA via `WebKitSettings`; default context menus + accelerator keys + inactive scheduling policy don't map onto WebKitGTK 4.1 settings cleanly) | ? | ? |
| Profile / cookie API / storage | ✅ | ✅ 0.4.0 (per-profile UUID + cookie API) | ✅ Phase 2d (data_dir-rooted `WebsiteDataManager`; per-URI `request_cookies_for_url` / `set_cookie` / `delete_cookie` on `CookieManager`) | ? | ? |
| Custom URL schemes | ✅ (virtual hosts) | ✅ | ✅ Phase 2d (`new_with_url_schemes` constructor; `WebContext::register_uri_scheme` + `URISchemeResponse` with content-type, status, and extra `MessageHeaders`) | ? | ? |
| Downloads | 🟡 (live pause/resume; no portable resume-data blob) | ✅ | 🟡 Phase 2d (DownloadStarted / DownloadProgress / DownloadFinished events; producer-picked destination under `data_dir/downloads/`; per-download cancel + host destination handler are future-add) | ? | ? |
| New-window / popup intercept | ✅ | ✅ | ✅ Phase 2d (`connect_create` returns `None` to suppress; surfaces `NavigationEvent::NewWindowRequested`; verified via `target="_blank"` anchor + isTrusted-true native click) | ? | ? |
| Process-failure recovery | ✅ | ✅ | 🟡 Phase 2d (`connect_web_process_terminated` wired → `NavigationEvent::ContentProcessTerminated`; runtime verification deferred — needs a deliberate web-process crash harness) | ? | ? |
| **Cross-API GPU sync** | explicit D3D12 fence when supplied; barrier fallback | MTLSharedEvent signal + producer wait | — (CpuRgba; no native frame) | DMABUF + future sync (Phase 5+) | VkSemaphore (explicit, Phase 4) |
| Pre-composition extraction | — | — | — | — | ✅ (only platform) |
| Sub-iframe / sub-frame capture | — | — | — | — | — |

The bottom three rows are *structural ceilings* — `—` means "not
possible without upstream API additions". Everything else is just work.

---

## GPU synchronization upgrades

The cross-API sync story is the only place where the platforms
differ in how *contractual* the producer→consumer ordering is today.

### Windows — explicit D3D12 fence (shipped, fallback still present)

The original path used keyed-mutex on the producer side and a
throwaway `copy_texture_to_buffer` on the consumer side to force a
`SHADER_RESOURCE → COPY_SRC → SHADER_RESOURCE` transition barrier,
which on D3D12 happened to flush shader caches that would otherwise
hold a stale view of the externally-written shared texture. That path
remains available as a fallback.

The contractual path is now a `D3D12_FENCE_FLAG_SHARED` fence:

1. Create the fence on the wgpu D3D12 device, export an NT handle
   via `ID3D12Device::CreateSharedHandle`.
2. Open it on the producer's D3D11 device via
   `ID3D11Device5::OpenSharedFence`.
3. Producer signals `value = n+1` on its D3D11 immediate context after
   `CopyResource`, releases the keyed mutex.
4. Consumer queues `ID3D12CommandQueue::Wait(fence, n+1)` before the
   render submit.
5. Bump `n` per frame.

The host creates the synchronizer, passes the shared handle into
`WebView2CompositionConfig::with_fence_shared_handle`, and imports
frames carrying `SyncMechanism::ExplicitFence` plus a monotonic
`fence_value`. If no handle is supplied, the producer uses the older
fallback sync path and the consumer can still force a fresh resource
with `invalidate_persistent_dest` if a driver ever shows stale pixels.

### macOS — MTLSharedEvent (real producer signal + producer-side wait)

IOSurface coherence is implicit on Apple silicon for the *source*
texture (the IOSurface SCK delivers, wrapped via
`newTextureWithDescriptor_iosurface_plane`). **The destination
texture is not IOSurface-backed** — it's a plain
`newTextureWithDescriptor` allocation in
`MTLStorageMode::Shared`, used as the blit target for the
chrome-stripped webview-pixel-rect crop. Metal does not implicitly
synchronize across `MTLCommandQueue` boundaries even when both
queues live on the same `MTLDevice`, so the consumer's wgpu queue
can race against the producer's blit-write to the destination
without an explicit fence.

The producer now closes that race two ways at once:

1. **Real `MTLSharedEvent` signal.** Every per-frame blit's
   command buffer encodes
   `MTLCommandBuffer::encodeSignalEvent:value:` after the blit's
   `endEncoding` and before `commit`, advancing a monotonic
   per-`CaptureState` counter. Each emitted `MetalTextureRef`
   carries the signalled value in `signal_value`; consumers pull
   the event handle via
   [`crate::WkWebViewProducer::metal_shared_event`]. `producer_sync`
   reports `SyncMechanism::ExplicitMetalEvent`.
2. **Producer-side `waitUntilCompleted`.** After commit, the
   producer CPU-blocks on the same command buffer until the GPU
   finishes. ~1 ms stall per acquire on Apple silicon — the
   *correct* default. Without it, consumers using the default
   `WgpuTextureImporter` (which doesn't insert a Metal wait, see
   below) would race.

The default [`WgpuTextureImporter`] on macOS uses
[`MetalSharedEventSynchronizer`], which accepts the
`ExplicitMetalEvent` mechanism but does not currently encode a
consumer-side wait. The producer-side `waitUntilCompleted`
guarantees correctness for that default path. The architectural
follow-up — encoding `encodeWaitForEvent:value:` on the consumer's
wgpu Metal queue via the wgpu-hal escape — would let us drop the
producer-side CPU stall entirely (~1ms per acquire reclaimed)
without changing public API. The producer-side signal is already
in place, so this upgrade is consumer-side-only.

Consumers that need explicit ordering today (interleaving the
blit with non-wgpu Metal queues, or wiring a custom
`InteropSynchronizer`) can already encode their own
`encodeWaitForEvent:value:` against
`metal_shared_event()` + each frame's `signal_value`.

### Linux WPE — VkSemaphore (already contractual)

The WPE DMABUF protocol returns a `VkSemaphore` per frame. Wgpu's
Vulkan import accepts external semaphores via
`VK_KHR_external_semaphore_fd`. **No extra fence work is required —
this is the standards-correct path out of the box.** This is the
deepest reason WPE on Linux is the strongest of the three GPU-sync
stories; the others are catching up to it.

### Linux WebKitGTK — wl_buffer release (implicit)

wlroots screencopy hands back a wl_buffer; the protocol's
`zwlr_screencopy_frame_v1::ready` event implies the buffer's contents
are valid at receive time. No explicit semaphore. Acceptable for the
fallback path; consumers who need stricter ordering should use the
WPE producer.

---

## Historical roadmap to parity

The version-named roadmap below is historical context for the original
cross-platform push. macOS has advanced past most of it, Windows now has
the current W1–W4 lane above, and Linux still needs the WPE producer
scaffold. Treat the release labels as old planning names, not the
current schedule.

### 0.3.0 — input completeness

Per-platform: keyboard forwarding (with IME baseline), touch + pen,
drag-and-drop, cursor-change events. Rounds out the embedding-input
surface so a consumer can build a productivity-grade UI on scrying
without going around it for input.

### 0.4.0 — environment + control

Per-platform: settings (zoom, UA, JS, devtools), reload/back/forward/
stop, profile + cookies, custom URL schemes, downloads, new-window
interception, process-failure recovery. Turns scrying into a complete
standalone system-webview surface deliverable.

### 0.5.0 — robustness + parity QA

Per-platform: explicit GPU sync (D3D12 fence on Windows;
MTLSharedEvent if needed on macOS; verify VkSemaphore wiring on
Linux WPE), throttling control for hidden composition WebViews on
Windows, DPI awareness across monitor moves, runtime distribution
strategies (WebView2 fixed-version on Windows, WPE packaging on
Linux). Cross-platform QA pass: every parity-baseline row green on
all three.

### Out-of-band slices

These don't fit on the version-by-version curve and can ship
independently when the work is ready:

- **macOS producer scaffold**: ✅ landed in 0.4.0. Slices A–N
  (lifecycle / SCK pipeline / nav parity / mouse / JS messaging /
  CPU snapshots / scroll wheel / title-changed / keyboard + IME /
  cursor changes / drag-doc / per-profile data stores /
  `MTLSharedEvent` scaffold / resize-applies-to-stream) bring the
  macOS WKWebView producer past Windows-0.2.0 parity. Browser-class
  items 1–9 (history controls / new-window intercept / settings /
  custom URL schemes / process-failure recovery / auth /
  multi-instance verification / downloads / find + PDF) bring it to
  a usable shape for browser-shape consumers like mere. Runtime
  verification via [`demo-mac`](../demo-mac/) covers slices A–N
  (12 of 13; drag is structurally SPI-blocked) plus six dedicated
  `--*-test` modes that exercise items 1, 3, 4, 7, 8, 9 +
  follow-ups (incognito, interactionState round-trip, pointer
  input, downloads). The suite runs headless via
  `bash scripts/test-mac.sh` and on every push via
  `.github/workflows/test-mac.yml`.

---

## Browser-class consumer roadmap

Beyond the parity baseline, an embeddable WebView library has to
support a browser-shape consumer (e.g. [`mark-ik/mere`](https://github.com/mark-ik/mere)):
multiple tabs per process, full navigation control, customizable
chrome, robust lifecycle hooks. Items 1–9 below landed in 0.4.x;
each has a brief notes column describing the API shape and known
limitations.

| # | Slice | Status | Public surface | macOS impl notes |
| --- | --- | --- | --- | --- |
| 1 | History controls | ✅ | `reload` / `stop` / `go_back` / `go_forward` / `can_go_back` / `can_go_forward` (trait) | direct `WKWebView::reload` / `stopLoading` / `goBack` / `goForward`; `go_back/forward` return `Ok(false)` if `canGoBack/Forward` is false |
| 2 | New-window intercept | ✅ | `NavigationEvent::NewWindowRequested { url }` | `WKUIDelegate::webView:createWebViewWithConfiguration:...` returns null to suppress the engine popup; host opens its own tab in response |
| 3 | Settings application | ✅ | `apply_settings(&WebSurfaceSettings)` (trait) | `pageZoom`, `customUserAgent`, `setInspectable` (macOS 13.3+), `WKPreferences::setJavaScriptEnabled`. Context-menu / accelerator-key fields silently ignored |
| 4 | Custom URL schemes | ✅ | `UrlSchemeHandlerFn`, `UrlSchemeResponse`, `WkWebViewProducer::new_with_url_schemes` | `WKURLSchemeHandler` delegate per registered scheme; serves bytes synchronously inside `webView:startURLSchemeTask:` |
| 5 | Process-failure recovery | ✅ | `NavigationEvent::ContentProcessTerminated` | `WKNavigationDelegate::webViewWebContentProcessDidTerminate:` |
| 6 | Auth challenges | ✅ (option A) | `NavigationEvent::AuthChallenged { url, host, auth_method }` | `webView:didReceiveAuthenticationChallenge:` defaults to `PerformDefaultHandling`; option B (host-driven disposition) deferred until mere has auth UI |
| 7 | Multi-instance verification | ✅ | n/a — architectural | `demo-mac --two-tabs` validates two producers in one process / one window cleanly drain independent event streams |
| 8 | Downloads | ✅ | `DownloadId`, `DownloadDestinationRequest`, `DownloadDecision`, `set_download_handler` / `clear_download_handler`, `cancel_download(id)`, `NavigationEvent::DownloadStarted` / `DownloadProgress` / `DownloadFinished` / `DownloadCancelled`, `WkWebViewProducerConfig::download_dir` | Per-download `DownloadId` correlates lifecycle events; `decidePolicyForNavigationResponse:` promotes non-displayable HTTP responses to downloads; per-download throttle (100ms / 1MiB) on progress; `WKDownload::cancel(_:)` for host-driven cancel; host destination handler runs synchronously inside `decideDestination` |
| 9 | Find / PDF | ✅ | `find_in_page` + `poll_find_match`, `request_pdf` + `poll_pdf`, `FindOptions` | both async-only via completion blocks; mirrors the snapshot pattern |

**Recently shipped (post-9):**

- ✅ Auth option B — `WkWebViewProducer::set_auth_handler` /
  `clear_auth_handler` registers a `Fn(AuthChallenge) -> AuthDisposition`
  closure invoked synchronously inside the navigation delegate.
  Translates to `NSURLSessionAuthChallengeDisposition` with
  optional `NSURLCredential` (HTTP basic via
  `AuthDisposition::UseCredential { username, password }`).
- ✅ Cookie store API — `request_all_cookies` / `poll_cookies`
  (async fetch), `set_cookie(&Cookie)` / `delete_cookie(name,
  domain, path)` (fire-and-forget). Wraps the
  `WKHTTPCookieStore` on the producer's `WKWebsiteDataStore`.
  Public `Cookie` struct mirrors `NSHTTPCookie`'s essential
  fields.
- ✅ Permission handlers — `set_permission_handler` /
  `clear_permission_handler` registers a
  `Fn(PermissionRequest) -> PermissionDecision` closure invoked
  for camera / microphone / device-orientation requests.
  No-handler default is `Prompt` (system UI).
- ✅ Incognito / non-persistent profile —
  `WkWebViewProducerConfig::non_persistent` (or the
  `.non_persistent()` builder) wires
  `WKWebsiteDataStore::nonPersistentDataStore`. Cookie / local
  storage / IndexedDB live only for the producer's lifetime.
  Verified by `demo-mac --incognito-test`.
- ✅ Tab-state serialization —
  `WkWebViewProducer::serialize_interaction_state` /
  `restore_interaction_state` round-trip the WKWebView's
  `interactionState` (back-forward list, scroll position, form
  data) as opaque bytes. Verified by
  `demo-mac --interaction-state-test`.
- ✅ Touch / pen / pointer input —
  `WebSurfaceProducer::send_pointer_input` synthesizes
  `NSEvent`s through the same path as `send_mouse_input`. WebKit's
  pointer-events JS API observes them as
  `pointerType: "mouse"` (macOS has no public direct-touch
  synthesis API). Verified by
  `demo-mac --pointer-input-test`.
- ✅ DPI awareness across monitor moves — `NSWindowDidChangeBackingPropertiesNotification`
  observer registered in `WkWebViewProducer::new_with_url_schemes`,
  re-applies `config.size` on the next `try_acquire_frame` /
  `resize` so points/pixels stay coherent. Cleaned up explicitly
  in `Drop`.
- ✅ Downloads (item 8 expansion) — see the row in the table
  above. `DownloadId` correlates events; throttled progress;
  host-driven destination and cancellation. Verified by
  `demo-mac --download-test`.
- ✅ Producer module split — the previous single-file
  ~4000-LOC `wkwebview_producer.rs` is now 18 submodules each
  under a 600-LOC ceiling (`producer.rs`, `capture/{mod,
  blocking, async_start}.rs`, `api.rs`, `trait_impl.rs`,
  per-delegate `*_handler.rs`, `helpers.rs`, etc.).
- ✅ Auth-challenge `protectionSpace` panic fix — bypass
  objc2's debug-build `class_getInstanceMethod` check
  (which doesn't see through `WKNSURLAuthenticationChallenge`'s
  forwarding-proxy class) by reading the property via
  `objc_msgSend` directly, gated on `respondsToSelector:` so
  defensive failure modes still surface as empty fields rather
  than a panic.
- ✅ Headless test runs — `demo-mac --*-test` modes default to
  hidden window + `NSApplicationActivationPolicyProhibited`, so
  `bash scripts/test-mac.sh` runs silently in the background.
  `--visible` overrides for debugging.
- ✅ CI — `.github/workflows/test-mac.yml` runs the suite on
  `macos-latest` (currently Apple Silicon).
- ✅ Cursor handler callback parity — `set_cursor_handler` /
  `clear_cursor_handler` registers a
  `Fn(CursorShape) + Send + Sync` closure invoked synchronously
  on every `NSCursor.currentSystemCursor` change observed after a
  forwarded input event. Coexists with the pull-model
  `poll_cursor_shape` queue. Verified by `demo-mac --scripted`.
- ✅ Download auth handler — the existing `AuthHandlerFn`
  registered via `set_auth_handler` now also routes through
  `WKDownloadDelegate::download:didReceiveAuthenticationChallenge:`.
  One handler covers both page-level and download-level auth
  challenges; hosts that need download-specific behavior can
  branch on the URL inside the handler. Verified by phase D of
  `demo-mac --download-test`.
- ✅ Programmatic download initiation — `start_download(url)`
  wraps `WKWebView::startDownloadUsingRequest:` so hosts can
  begin downloads without navigating the WKWebView. Auth
  challenges flow through the *download-level* delegate
  callback (rather than the page-level one), exercising that
  code path that promotion-driven downloads can't.
- ✅ Download cancel / resume — `cancel_download(id)` (which
  passes a non-nil completion block to Apple's `cancel:` to
  un-suppress `didFailWithError`), captures the resume_data
  WebKit emits on a `DownloadCancelled` event;
  `resume_download(&[u8], PathBuf)` calls
  `resumeDownloadFromResumeData:` with a fresh delegate
  registration so resumed transfers fire the normal lifecycle
  events. Verified by phase E of `demo-mac --download-test`.
- ✅ SCK pipeline assertion test —
  `demo-mac --capture-test` is an opt-in smoke test for the
  ScreenCaptureKit path. Not in `scripts/test-mac.sh` because
  Screen Recording permission can't be self-granted; CI runners
  need a `tccutil` pre-grant. Surfaced a producer-side fix
  along the way: `try_acquire_frame` now treats status-only
  `CMSampleBuffer`s (every `SCFrameStatus` except `Complete`)
  as "no frame ready" rather than a fatal error.
- ✅ Capability-probe parity for macOS —
  `WebSurfaceCapabilities::probe` now mirrors the Windows
  shape: a Metal-backed host wgpu device gets
  `imported_texture: Supported`,
  `preferred_mode: ImportedTexture`, and
  `supported_frames: [MetalTextureRef]`. Hosts that drive
  fallback selection from `probe` (rather than constructing a
  producer to read its runtime capabilities) now discover the
  SCK / Metal path on macOS.
- ✅ Offset units standardized — `WkWebViewProducerConfig::offset`
  is now physical pixels (matches the trait's `set_offset`
  contract and `config.size`'s units). Pre-fix `with_offset(200, …)`
  and `set_offset(200, …)` landed at different positions on
  Retina; both now resolve to the same point.
- ✅ Final `DownloadProgress` carries the response's announced
  `Content-Length` — `DownloadEntry` captures `total_bytes_expected`
  at `decideDestination` time rather than reusing
  `last_progress_bytes`, so throttled downloads no longer emit
  `bytes_written > total_bytes_expected` on the final tick.
- ✅ Scroll-wheel events carry location + modifier flags —
  `synthesize_scroll_wheel_event` now sets the CGEvent's
  location (webview rect → screen-global, top-left origin) and
  `CGEventFlags` (Shift / Control) before the round-trip
  through `NSEvent::eventWithCGEvent:`. WebKit attributes the
  scroll to the right WKWebView and honors cmd-scroll /
  shift-scroll modifier behavior.
- ✅ `is_http_only` round-trip through `set_cookie` —
  `NSHTTPCookie::cookieWithProperties:` doesn't expose HttpOnly
  as a settable property (Apple's documented set of property
  keys excludes it). `set_cookie` now routes HttpOnly cookies
  through `cookiesWithResponseHeaderFields:forURL:` with a
  synthesized `Set-Cookie` header; non-HttpOnly cookies keep the
  faster property-dict path. Verified by the `is_http_only=true`
  assertion in `--incognito-test`.
- ✅ Multi-instance + persistent-store coverage —
  `--profile-test` is now a one-process self-assertion (two
  persistent producers at the same `data_dir`, set on #1 → see
  on #2; complement to `--incognito-test`'s isolation
  assertion); `--two-tabs` asserts each producer's nav events
  stay in its own queue (no cross-talk). Both run in the
  headless suite — `scripts/test-mac.sh` is now 8 modes / 8 PASS.
- ✅ SCK source-rect crop via per-frame Metal blit —
  `initWithDesktopIndependentWindow:` captures the entire host
  window (Apple ignores `sourceRect` for single-window filters);
  the producer now captures at full window resolution and
  blit-crops to the WKWebView's pixel rect inside
  `try_acquire_frame` before handing the texture out.
  `CaptureState` carries an `MTLCommandQueue` allocated on the
  host's wgpu Metal device; per-frame cost is ~1 ms on Apple
  silicon. The imported texture's dimensions match the
  webview's pixel rect — no host chrome, no recursive capture
  even when the consumer composites the texture back into the
  same window. The crop's Y origin lives in window-frame
  top-left coords (chrome height is added on top of the
  content-view-relative Y flip) so the title-bar region of the
  captured texture is excluded; `try_acquire_frame` rejects
  in-flight pre-resize samples whose IOSurface dimensions
  differ from the current host-window pixel size, so SCK's
  push-model "deliver one or two more samples after
  `updateConfiguration:` then go quiet" doesn't leave a
  stale-stretched frame on screen across a resize.
- ✅ Capture-pipeline cadence probe —
  `WkWebViewProducer::capture_metrics()` returns a
  [`CaptureMetrics`](../scrying/src/wkwebview_producer/capture/mod.rs)
  snapshot with `samples_received` (incremented from the SCK
  background dispatch queue on every `Screen`-typed sample) and
  `samples_consumed` (incremented in `try_acquire_frame` on every
  `Ok(Some(...))`). The demo-mac `--capture` mode prints rates
  once per second; the deltas confirm SCK keeps up with display
  refresh on Apple Silicon (~58 push/s, ~58 consume/s) so the
  perceived "a bit of lag" is pipeline depth (WebKit render →
  AppKit composite → SCK encode → demo blit → wgpu present, ~3
  vsyncs ≈ 50 ms at 60 Hz), not a backlog. Going lower than that
  needs an architecture change like consumer-side crop (skip the
  per-frame Metal blit pass).

- ✅ Context-menu interception via JS user-script —
  `WKUIDelegate` exposes
  `webView:contextMenuConfigurationForElement:completionHandler:`
  on iOS only; macOS has no public-API context-menu hook.
  Rather than reach for `_WK*` SPI, the producer now injects a
  capture-phase `contextmenu` user-script via
  `WKUserContentController` that walks the click target's
  ancestor chain to recover the closest enclosing `<a href>` /
  `<img src>`, calls `event.preventDefault()` to suppress
  WebKit's default `NSMenu`, and posts a NUL-delimited 5-field
  payload to a dedicated `WKScriptMessageHandler`
  (`scryingContextMenu`). The handler parses the payload and
  pushes a [`crate::NavigationEvent::ContextMenuRequested`]
  event with `page_url` / `x` / `y` (CSS pixels relative to
  the WebView viewport) / `link_url` / `image_url`. Verified in
  `--two-tabs --visible` that each producer's right-clicks
  route to its own nav-event queue (no cross-talk).

**Outstanding for follow-up slices.** This list is mirrored as a
flat cross-platform checklist in
[`2026-05-09_browser_parity_checklist.md`](2026-05-09_browser_parity_checklist.md);
the entries below keep the macOS-specific impl notes the
checklist deliberately omits.

- ✅ Authentication during downloads — both
  nav-promoted and programmatic `start_download` paths set the
  shared `DownloadHandler` as the `WKDownloadDelegate`, and the
  delegate's
  `download:didReceiveAuthenticationChallenge:completionHandler:`
  consults the same `auth_handler` registered for page-level
  auth. Download-channel events now carry the resource URL from
  `WKDownload::originalRequest` instead of the previous
  empty-string sentinel, so hosts can correlate the auth
  challenge with the matching `DownloadStarted` event.
  
  New
  [`crate::AuthSource`] enum (`Page` /
  `Download`) added to both `NavigationEvent::AuthChallenged`
  and the `AuthChallenge` handler-arg struct so hosts can route
  the two channels differently — page auth is a tab-level UI
  moment, download auth is a per-transfer credential prompt.
  The download-test (`--download-test`) phase D verifies the
  `AuthSource::Download` event fires for an HTTP-Basic-protected
  programmatic download.
- Throttling control — suspending / resuming page activity for
  hidden tabs needs SPI (`_setSuspended:`) and is risky. The
  lighter alternative — Page Visibility sync, see below — is
  the public-API path and probably sufficient for most
  consumers.
- ✅ Page Visibility / occlusion sync —
  `WebSurfaceProducer::set_visible(bool)` cascades through
  `NSView::setHidden:`. WebKit observes the
  `viewDidHide` / `viewDidUnhide` chain and pushes
  `visibilitychange` events page-side so `document.hidden` /
  `document.visibilityState` flip. RAF callbacks throttle to
  ~1 Hz, background-tab autoplay / video-decoding throttles per
  the engine's policy, `setInterval` callbacks may coalesce. No
  `_WK*` SPI involved. Distinct from the heavier
  `_setSuspended:` SPI-only path (which fully pauses execution
  and stays out of scope).
- ✅ Drag-and-drop in (observability) — capture-phase
  `drop` user-script filters out intra-page drags (heuristic:
  drop must carry at least one of `dataTransfer.files`,
  `text/uri-list`, or an `image/*` MIME) and posts a
  NUL-delimited 4-field payload to a dedicated
  `scryingDrop` `WKScriptMessageHandler`. The handler emits
  [`crate::NavigationEvent::DropDetected { x, y, file_count,
  primary_url }`] onto the producer's nav-event queue. Pure
  observability: the user-script does *not* call
  `event.preventDefault()`, so the page's own JS `drop` event
  fires alongside, and WebKit's default behavior (file →
  navigate, drop on `<input type=file>`, etc.) runs as usual.
  Browser-class consumers use the event for analytics, status
  indicators, or "I want to route this URL drop to the active
  tab" decisions made *in addition to* whatever the page does.
  (Drag-and-drop *out* — initiating a drag *from* page content
  — remains `_WK*` SPI on macOS and stays punted.)
- ✅ Print / `Cmd+P` —
  `WkWebViewProducer::print()` fetches the standard
  `NSPrintInfo::sharedPrintInfo`, asks the WebView for an
  `NSPrintOperation` via `printOperationWithPrintInfo:`, and runs
  it modally with `runOperation()`. Returns `true` on actual
  print, `false` on cancel. Distinct from the headless
  `request_pdf` path (which is non-interactive). Hosts wanting a
  customized `NSPrintInfo` (page range, paper size) can route
  around this method by calling `printOperationWithPrintInfo:`
  through the objc2-web-kit binding directly.
- ✅ Content blocking —
  `WkWebViewProducer::compile_and_apply_content_rule_list(identifier,
  encoded_json)` compiles AdBlock-shape JSON rule lists via
  `WKContentRuleListStore::defaultStore` and, on the main-thread
  completion, attaches the resulting `WKContentRuleList` to the
  configuration's `WKUserContentController`. Fire-and-forget —
  compile failures (invalid JSON, unsupported actions) log to
  stderr rather than surfacing through the API since rules are
  best-effort and a silent skip is the right semantic. Apple's
  store caches the compiled blob on disk under the identifier,
  so re-compilation with the same id-and-json pair is fast.
  `clear_all_content_rule_lists` detaches every applied list
  (per-identifier removal is YAGNI for now). No SPI; works on
  macOS 10.13+.
- ✅ Cookie / storage observation —
  `WkWebViewProducer::set_cookie_change_handler` /
  `clear_cookie_change_handler` register a `Box<dyn Fn() + Send +
  Sync>` invoked on every `WKHTTPCookieStoreObserver::cookiesDidChangeInCookieStore:`
  callback (page-side `document.cookie` writes, `Set-Cookie`
  response headers, host calls to `set_cookie` /
  `delete_cookie`). Apple's protocol delivers no delta — the
  callback is a "go re-fetch" pulse — so consumers pair with
  `request_all_cookies` / `poll_cookies` to observe the new
  state. The observer is registered on the producer's
  `WKHTTPCookieStore` for the producer's lifetime; the closure
  slot is what gates whether anything fires, which keeps the
  set / clear path lock-only and avoids re-registration churn.
- ✅ Color management — Display P3 SDR opt-in. New
  [`crate::ColorPipeline`] enum with `Srgb` (default) and
  `DisplayP3` variants;
  [`WkWebViewProducerConfig::color_pipeline`] picks at
  construction time and
  [`WkWebViewProducer::set_color_pipeline`] flips live. The SCK
  configuration's `colorSpaceName` is set to
  `kCGColorSpaceSRGB` or `kCGColorSpaceDisplayP3`
  accordingly; pixel format stays at `BGRA8Unorm` (P3 colors
  fit in 8-bit, only the gamut tag differs). On a P3-capable
  display with the consumer's wgpu surface configured for P3
  output, page-side `color(display-p3 …)` and P3-tagged images
  arrive at the consumer in their wider gamut. On an sRGB-only
  display the visible result is identical to `Srgb` — the macOS
  composer remaps P3→sRGB at present time.

  **Configuration-revision gate** generalizes the existing
  dim-match guard: `CaptureState::config_revision` ticks every
  time we hand SCK a new `SCStreamConfiguration` (resize, DPI
  flip, *and* color-pipeline change);
  `applied_config_revision` catches up when SCK's completion
  handler fires. Until they're equal, `try_acquire_frame`
  returns `Ok(None)` to drop ambiguous in-flight samples that
  might be encoded under either the old or the new config —
  Apple doesn't tag CMSampleBuffers with their generating
  config, so the only safe move during a transition is to
  wait. Subsumes the dim-match check for any future
  reconfiguration axis (HDR pixel format, etc.) without us
  having to read CFType color-space attachments off each
  buffer.

- ✅ HDR / 16-float — `ColorPipeline::Hdr16f` is now wired
  end-to-end on the producer side. SCK's `pixelFormat` flips to
  `kCVPixelFormatType_64RGBAHalf` and `colorSpaceName` to
  `kCGColorSpaceExtendedLinearDisplayP3`; the Metal source and
  destination textures (allocated per frame in
  `try_acquire_frame`) become `MTLPixelFormat::RGBA16Float`;
  `MetalTextureRef::format` reports
  `wgpu::TextureFormat::Rgba16Float`. The per-frame revision
  gate already accommodates pixel-format / color-space changes
  uniformly, so flipping `set_color_pipeline` to `Hdr16f` on a
  live capture rides the same drop-during-transition path as
  resize.

  Consumer-side rendering of HDR content requires an HDR-capable
  wgpu surface (`Rgba16Float` + `CompositeAlphaMode::PreMultiplied`
  on macOS-EDR, or PQ-on-Rec.2020 surface configurations). On
  an SDR-only surface the demo's existing
  `Bgra8Unorm` swap chain clamps over-bright values to
  ~SDR-white at present time — the producer side is
  correct, the SDR-display fallback is just visually less
  exciting. Per-frame GPU bandwidth ~doubles (8 bytes/pixel vs
  4) because the dest texture's stride doubles; per-frame Metal
  blit cost on Apple silicon stays in the ~1ms ballpark for
  webview-sized rects. `palette` is in tree against future
  programmatic verification of color round-trips.

  Channel ordering note: `kCVPixelFormatType_32BGRA` is BGRA
  but `kCVPixelFormatType_64RGBAHalf` is RGBA — the SDR
  pipeline's swizzle from BGRA to RGB happens at sample time in
  the consumer's shader, while the HDR pipeline's source matches
  `wgpu::TextureFormat::Rgba16Float` directly. No swizzle pass
  in the producer either way.
- DevTools / Web Inspector remote attach — `setInspectable(true)`
  is wired for macOS 13.3+, but the Safari → Develop menu →
  attach flow isn't documented anywhere; downstream consumers
  hit it cold. Doc-only slice.
- ✅ Autofill / Keychain integration — system-driven on macOS,
  no producer-level code required. Apple's Keychain plus
  AppKit's `NSSecureTextField` handle credential save /
  suggest transparently for `<input type="password">` and
  `autocomplete`-tagged fields whenever the WKWebView is
  hosted in a focused, frontmost window. Per-profile
  credential isolation falls out of the existing
  `WKWebsiteDataStore` selection driven by
  [`WkWebViewProducerConfig::data_dir`] /
  `non_persistent`: a non-persistent (incognito) producer
  doesn't touch the persistent Keychain entries; persistent
  producers at distinct `data_dir`s get their own per-profile
  storage namespaces.
- ✅ Spellcheck / autocorrect controls — best-effort knob via
  [`WkWebViewProducerConfig::spellcheck_override`]
  (`Option<bool>`). When `Some(b)`, the producer injects a
  document-start user-script that walks
  `<input>` / `<textarea>` / `[contenteditable]` elements
  and sets `spellcheck="true|false"` accordingly, plus a
  `MutationObserver` on `document.documentElement` to catch
  later-added nodes. WKWebView has no public-API engine-level
  spellcheck toggle, so a JS-attribute override is the best
  scrying can do without falling back to `_WK*` SPI; pages
  that respect the standard `spellcheck` attribute (the
  vast majority) honor it.
- ✅ WebRTC capture lifecycle observability — JS user-script
  injected at `AtDocumentStart` monkey-patches
  `navigator.mediaDevices.getUserMedia`, increments per-kind
  track counters on each successful capture, and decrements them
  via `track.ended` listeners. Posts `audio:N,video:M` strings to
  a dedicated `scryingMediaCapture` `WKScriptMessageHandler`
  which parses and emits
  [`crate::NavigationEvent::MediaCaptureStateChanged { audio_active_tracks, video_active_tracks }`]
  onto the producer's nav-event queue. Counts (not booleans) so
  hosts can distinguish "1 mic" from "2 mics" if they want; a
  red-dot indicator is just `>0`. Caveats: pages that replace
  `navigator.mediaDevices` or `getUserMedia` *before* the
  user-script runs escape the wrap (rare in practice; document-
  start injection wins the race against most page code), and
  counters reset to zero per top-level navigation.
- Pre-composition extraction — capture the WebView's
  `CALayer.contents` directly via `CARenderer` / `IOSurface`,
  bypassing the WindowServer composite. Would also kill the
  "SCK goes quiet on a static page" cadence dependency that
  motivated the dim-match guard. Unclear whether reachable
  without `_WK*` SPI; spike needed.
- Sub-iframe / sub-frame capture — per-iframe textures rather
  than a single composited window grab. WebKit's
  `WKWebView` is a single composition root; per-frame access
  is `_WK*` SPI today. Likely a long-term Linux-WPE-first
  story (WPE exposes per-view buffers natively).

**Reference implementations.** Established Cocoa/WebKit hosts remain useful
references for objc2 lifetime, delegate retain, policy-decision, and
responder-chain questions when adding follow-up macOS slices.

---

- **Linux WPE producer completion (Phase 4)**: WPE FFI callback bridge,
  DMABUF + VkSemaphore acquire path, Vulkan importer, plus
  input/event model. Blocked on WPE + `wpebackend-fdo` runtime
  packaging (not in Fedora 44's repos; needs COPR / source build).
- **Linux WebKitGTK 4.1 Phase 2b**: reload/stop/back/forward,
  `poll_navigation_event`, title-changed, `apply_settings`, JS
  messaging via `WebKitUserContentManager`. Input forwarding follows
  as the Phase 2b *heavy* sub-slice — synthesized `GdkEvent` dispatch
  against the offscreen WebView.
- **Linux WebKitGTK 6.0 sibling (Phase 5+)**: `gtk4 = 0.11` +
  `webkit6 = 0.6` sibling producer behind a `webkit6` feature flag
  mutually exclusive with `webkitgtk-fallback`. Open offscreen-rendering
  question (`GtkOffscreenWindow` removed in GTK 4) needs empirical
  verification.

---

## Native-frame import: in-tree as `scrying::native_frame`

Scrying owns its native-frame import path in-tree as the
[`scrying::native_frame`](../scrying/src/native_frame/) module. It is
**not** a dependency on a sibling crate. This was a deliberate split
from the original plan to share `wgpu-native-texture-interop` with
[`wgpu-graft`](https://github.com/merely-made/wgpu-graft).

**Why in-tree**: the two projects' producers have different shapes.
wgpu-graft consumes Servo via surfman GL framebuffer surfaces and
bridges them to wgpu. scrying consumes platform-native texture
handles directly (D3D12 NT-handle today; IOSurface and DMABUF when
the macOS / Linux producers land). Sharing one crate would force two
genuinely different problems through the same abstraction.

The module is structurally derived from the per-platform shape in
Slint's [Servo embedding example](https://github.com/slint-ui/slint/tree/master/examples/servo),
adapted to take native handles directly (no GL bridge) and folded
together with scrying's explicit-fence wiring. See [NOTICE](../NOTICE)
for the upstream attribution.

What lives in `native_frame/` today:

- `mod.rs` — `HostWgpuContext`, `NativeFrame`, `Dx12SharedTexture`,
  `WgpuTextureImporter` + the import dispatch. Drops the GL FBO
  source variants from wgpu-graft's interop crate; only emits
  variants for which scrying has a working producer.
- `error.rs` — `InteropError`, `UnsupportedReason`.
- `sync.rs` — `SyncMechanism` (`None` / `ExplicitExternalSemaphore` /
  `ExplicitFence`), `InteropSynchronizer` trait,
  `NoopSynchronizer`, `ImplicitOnlySynchronizer`.
- `sync_dx12.rs` — `Dx12FenceSynchronizer` (Windows).

What lands here as the macOS / Linux producers come online:

- `IoSurfaceTexture` variant on `NativeFrame` + `import_io_surface_texture`
  (macOS). Optional `MetalSharedEventSynchronizer` if implicit
  IOSurface coherence ever fails.
- Complete `DmaBufImage` import in `import_dmabuf_image` (Linux WPE)
  and replace the placeholder external-semaphore synchronizer with a
  real Vulkan wait for the per-frame `VkSemaphore` the WPE DMABUF
  protocol carries.

These are scrying-internal additions. No cross-crate coordination
required.

---

## Open questions

- **Linux producer packaging**: all three Linux producers stay in-tree
  behind mutually-exclusive cargo features (`webkitgtk-fallback`,
  `webkit6`, `wpe`). Decided 2026-05-14 — see
  [`2026-05-14_linux_webkitgtk_phase_2a.md`](2026-05-14_linux_webkitgtk_phase_2a.md).
  Open: whether `webkitgtk-fallback` should be renamed (`webkitgtk` /
  `webkitgtk4_1`) now that "fallback" framing is dropped.
- **Windows runtime distribution**: do we document the WebView2
  Evergreen runtime requirement, or also support fixed-version
  bundling? Affects producer construction (different
  `CoreWebView2EnvironmentOptions`).
- **Macro-level demo split:** `demo-scrying-winit` is the small
  cross-platform backend-selection smoke. `demo-win` and `demo-mac`
  are the platform-specific runtime benches where heavier behavioral
  assertions live. Add `demo-linux-wpe` once the WPE/DMABUF path can
  be exercised on Linux hardware.

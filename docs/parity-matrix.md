# Capability parity matrix

Cross-backend capability comparison for `scrying`'s `WebSurfaceProducer`
impls. Each column is a backend; each row is a capability the producer
trait (or producer-inherent API) can surface to the host.

Legend:

- ✔ — implemented and runtime-exercised on this backend.
- ⚠ — implemented but with a documented caveat (see footnotes).
- ✘ — not implemented / structurally unavailable on this backend.
- ? — source doesn't clearly indicate support; treat as unverified.

Authoritative sources for this matrix:
[`scrying/src/webview2_composition_producer.rs`](../scrying/src/webview2_composition_producer.rs),
[`scrying/src/wkwebview_producer/mod.rs`](../scrying/src/wkwebview_producer/mod.rs),
[`scrying/src/webkitgtk_producer/mod.rs`](../scrying/src/webkitgtk_producer/mod.rs),
[`scrying/src/webkit6_producer/mod.rs`](../scrying/src/webkit6_producer/mod.rs),
[`scrying/src/wpe_producer/mod.rs`](../scrying/src/wpe_producer/mod.rs),
[`docs/wpe-deployment.md`](wpe-deployment.md), and the
[Phase 4 strategy doc](../design_docs/2026-05-15_phase4_strategy.md).
Demo flag coverage in [`README.md`](../README.md) is what makes a row
"runtime-exercised" rather than "compiles."

| Capability | WebView2 (Windows) | WKWebView (macOS) | WebKitGTK 4.1 (`webkitgtk-fallback`) | WebKitGTK 6.0 (`webkit6`) | WPE (`wpe`) |
| --- | --- | --- | --- | --- | --- |
| Frame transport | Shared D3D11 texture via WGC (`ImportedTexture`) [^win-frame] | CPU snapshot + ScreenCaptureKit → `IOSurface`/`MTLTexture` (`ImportedTexture`) [^mac-frame] | CPU snapshot (`CpuRgba`) via `webkit_web_view_get_snapshot` | CPU snapshot (`CpuRgba`) via `GtkWidgetPaintable` → `GskRenderer::render_texture` → `GdkTextureDownloader`, with legacy `webkit_web_view_get_snapshot` fallback; GPU import ⚠ [^wk6-dmabuf] | DMABUF fds + optional `VkSemaphore` opaque fd (`DmaBufImage`) [^wpe-dcc] |
| Navigation (load HTML, load URL, wait_for_load) | ✔ | ✔ | ✔ | ✔ | ✔ |
| Resize at runtime | ✔ | ✔ | ✔ | ✔ | ⚠ [^wpe-resize] |
| Keyboard input dispatch | ✔ | ✔ | ✔ | ⚠ [^wk6-input-js-only] | ✔ |
| Mouse input dispatch | ✔ | ✔ | ✔ | ⚠ [^wk6-input-js-only] | ✔ |
| Pointer input dispatch (pen/stylus) | ✔ | ✔ | ✔ | ⚠ [^wk6-input-js-only] | ✔ |
| Touch input dispatch | ? [^win-touch] | ? [^mac-touch] | ? [^gtk-touch] | ? [^wk6-touch] | ⚠ [^wpe-touch] |
| Scroll input dispatch | ✔ | ✔ | ✔ | ⚠ [^wk6-input-js-only] | ✔ |
| Cookie get/set/delete | ✔ | ✔ | ✔ | ✔ | ✔ |
| Custom URL scheme handlers | ✔ | ✔ | ✔ | ✔ | ✔ [^wpe-scheme-coverage] |
| Cursor-shape reporting | ✔ | ✔ | ✔ | ✔ [^wk6-cursor-coverage] | ✔ [^wpe-cursor-coverage] |
| IME observability (focus/change/blur) | ? [^win-ime] | ? [^mac-ime] | ✔ | ✔ [^wk6-ime-coverage] | ✔ |
| Script-message bridge (host ↔ page postMessage) | ✔ | ✔ | ✔ | ✔ | ✔ |
| Download lifecycle observability | ✔ | ✔ | ✔ | ✔ | ✔ |
| Drag/drop | ✔ [^win-drag] | ✘ [^mac-drag] | ✔ [^gtk-drag] | ⚠ [^wk6-drag] | ✘ [^wpe-drag] |
| Find-in-page | ✔ | ✔ | ? | ? [^wk6-find] | ✘ |
| PDF rendering | ✔ | ✔ | ? | ? [^wk6-pdf] | ✘ |
| Profile data isolation | ✔ | ✔ | ? | ? [^wk6-profile] | ✔ |
| Process-isolation/recovery | ✔ | ✔ | ✔ | ? [^wk6-process] | ? [^wpe-process] |

[^win-frame]: WebView2 composition-controller → WinComp visual →
`Windows.Graphics.Capture::CreateFromVisual` → `Bgra8Unorm` D3D11
texture, bridged into the host as a shared NT handle for Graft import.

[^mac-frame]: WKWebView ships *two* frame transports: a CPU
`takeSnapshot:` path (one-shot, >50ms latency, `CpuSnapshot` tier) and
a ScreenCaptureKit → `IOSurfaceRef` → `MTLTexture` path for live
composited frames (`ImportedTexture` tier). The SCK path needs the
Screen Recording privacy permission. Headed hardware runs wrap the demo in
the stable `org.merely.scry.hardware-demo` app bundle and require a persistent
designated requirement so that permission survives rebuilds. The hardware
script uses a runner-safe ad-hoc signature by default; a provisioned Developer
ID/Apple Development identity can be selected with
`SCRY_MAC_CODESIGN_IDENTITY`. See
[`wkwebview_producer/mod.rs`](../scrying/src/wkwebview_producer/mod.rs).

[^wpe-dcc]: The wgpu 30 path is pixel-verified on AMD Renoir/RADV with
WPEWebKit 2.52.5 and an explicit DCC modifier. The hard integration test
sampled 4,096 center pixels through a host-owned render target; every pixel
was within ±8 of BGRA `[255, 144, 30, 255]`. Graft owns the foreign-queue
acquire and registers the established `RESOURCE` state at the wgpu HAL
boundary. See the full command and receipt in
[`docs/wpe-deployment.md#wgpu-vulkan-pixel-correctness-receipt`](wpe-deployment.md#wgpu-vulkan-pixel-correctness-receipt).

[^wpe-resize]: WPE's headless `WPEToplevelHeadless::resize` vfunc is
unimplemented in WPEWebKit 2.52.5. `wpe_toplevel_resize` returns TRUE
but dimensions stay at the construction-time defaults (1024×768). Pick
the final size at `WpeProducer::new` and do not resize at runtime.
Honored correctly on hosted (non-headless) WPE targets and on
WebKitGTK 4.1 / 6.0. See
[`docs/wpe-deployment.md#toplevel-resize-is-a-no-op`](wpe-deployment.md#toplevel-resize-is-a-no-op).

[^wpe-touch]: Touch dispatch through `wpe_view_event` blocks
indefinitely on headless WPE (the dispatch path expects
`WPEGestureController` + `WPEScreen` state headless doesn't provide).
The scrying → WPEEvent translation layer is unit-test covered; only
end-to-end dispatch is blocked on headless. Mouse and pen paths are
unaffected. See
[`docs/wpe-deployment.md#touch-input-hangs`](wpe-deployment.md#touch-input-hangs).

[^wpe-scheme-coverage]: Scheme handler registration ships and matches
the GTK precedent; runtime smoke is deferred until the cross-backend
trait test infra covers schemes. Unit tests cover the translation
layer. See [4c.5.c in the Phase 4 strategy doc](../design_docs/2026-05-15_phase4_strategy.md#L494-L517).

[^wpe-cursor-coverage]: `mouse-target-changed` → `CursorShape`
translation ships and is unit-tested; end-to-end runtime coverage
requires a real DOM hover, deferred until a non-headless WPE producer
exists.

[^wpe-drag]: Marked structurally unsupported in the producer, matching
the macOS WKWebView precedent. The producer trait keeps
`send_drag_input` available for a future host that injects HTML5
drag/drop DOM events through the JS message bridge. See the producer
guardrail comment in
[`scrying/src/wpe_producer/producer.rs`](../scrying/src/wpe_producer/producer.rs)
and [4c.4.2 in the strategy doc](../design_docs/2026-05-15_phase4_strategy.md).

[^wpe-process]: WPE's process model couples one `WebKitWebView` to one
process-global headless `WPEDisplay`; constructing a second producer
in the same process has been observed to SIGABRT (parallel) or hang
in WebKit teardown (sequential). Production callers hold one producer
per process or spawn a subprocess per producer. There is no
`web-process-terminated` recovery handler wired today. See
[`docs/wpe-deployment.md#one-wpe-display-per-process`](wpe-deployment.md#one-wpe-display-per-process).

[^wk6-dmabuf]: Phase A.8 wired the paintable-render capture path
(`GtkWidgetPaintable` → `GskRenderer::render_texture` → `GdkTexture`)
and probes the resulting texture against `gdk4::DmabufTexture`.
Detection works (`GDK_IS_DMABUF_TEXTURE`-equivalent type probe), but
extraction is blocked by an upstream GTK 4 API gap: through current
stable GTK 4.22, there is no public C accessor for
`GdkDmabufTexture`'s plane fds / fourcc / modifier / offset / stride
(the inverse accessors live exclusively on `GdkDmabufTextureBuilder`,
which is the *producer* side). `libgtk-4.so`'s symbol table on Fedora
44 confirms this — only `gdk_dmabuf_texture_get_type` is exported.
The producer therefore downloads pixels via `GdkTextureDownloader`
into the `CpuRgba` tier; a future `WebSurfaceFrame::Native(...)` arm
needs either a GTK upstream PR adding accessors, or a private
`GstWebKit` / web-process tap before GTK wraps the buffer. See
[`scrying/src/webkit6_producer/capture.rs`](../scrying/src/webkit6_producer/capture.rs)
module doc and the [Phase A.8 strategy
row](../design_docs/2026-05-15_phase4_strategy.md).

[^wk6-input-js-only]: WebKitGTK 6.0 ships JS-event-synthesis input
forwarding only — the GTK 3 producer's native `GdkEvent`-dispatch
primary (which closes the `isTrusted` gap via `gtk_main_do_event`) has
no analog because GTK 4 removed `gtk_main_do_event` (and
`gdk_event_new` / `gdk_event_put` with it). Synthesized DOM events
arrive with `event.isTrusted === false`; page code that
discriminates on `isTrusted` (some click-fraud defences,
`requestFullscreen()`, autoplay-gating user gestures) will reject
these events. Same caveat the macOS WKWebView producer documents. A
future native upgrade would route through `gtk4::GestureClick`
controller synthesis or direct `GdkSurface` event-queue manipulation,
both materially harder than the GTK 3 `gtk_main_do_event` call. See
[`scrying/src/webkit6_producer/input.rs`](../scrying/src/webkit6_producer/input.rs)
module doc.

[^wk6-touch]: WebKitGTK 6.0 has no `--touch-test` flag in the
demo-linux6 suite, and the JS-synthesis input path
(`webkit6_producer/input.rs`) doesn't ship a `TouchEvent` builder.
Marked `?` rather than guessing.

[^wk6-cursor-coverage]: `mouse-target-changed` → `CursorShape`
translation ships and is unit-tested in `webkit6_producer/cursor.rs`;
end-to-end runtime coverage requires a real DOM hover, deferred until
a non-headless webkit6 host exists (same `?` → ✔ standard the WPE
column applies to the analogous WPE row, except here cursor reporting
is signal-driven from the engine and is structurally identical to the
GTK 3 producer — ✔ with the deferred-runtime caveat).

[^wk6-ime-coverage]: `scryIme` UCM handler + DOM
focusin/focusout/input/selectionchange observer ships and is
unit-tested in `webkit6_producer/ime.rs` (5 `parse_event` tests
verbatim from the GTK 3 / WPE precedents). End-to-end runtime
coverage requires a real focused input element; deferred along with
`--cursor-test`.

[^wk6-drag]: WebKitGTK 6.0 implements `send_drag_input` via JS-event
synthesis only — `event.dataTransfer.files` is empty for pages whose
drop handlers read it; coordinate / type discrimination still works.
GTK 3 has the same JS-synthesis path PLUS the
`gtk_main_do_event`-based native primary, which webkit6 lacks. No
`--drag-test` flag in demo-linux6 because the synthesis path's
fidelity is materially lower than GTK 3's native path; a future
`gtk4::GestureDrag`-based upgrade would close the gap.

[^wk6-find]: WebKitGTK 6.0 has no `--find-test` flag in the
demo-linux6 suite. Marked `?` rather than guessing — webkit6 exposes
the same `WebViewExt::find_controller` API as the GTK 3 line, but
the producer doesn't currently surface a host-facing find API.

[^wk6-pdf]: WebKitGTK 6.0 has no `--pdf-test` flag in the demo-linux6
suite. Marked `?` rather than guessing.

[^wk6-profile]: Same `?` the GTK 3 row carries — the producer uses a
host-supplied `data_dir` and constructs an isolated `NetworkSession`
against it, so profile isolation is structurally present, but no
`--profile-test` flag exercises it in demo-linux6.

[^wk6-process]: Same `?` the WPE row carries — the producer doesn't
wire a `web-process-terminated` recovery handler today.

[^win-touch]: WebView2 has no `--touch-test` flag in the demo-win
suite; the producer's input surface is mouse/pointer/keyboard. Native
WebView2 supports `SendPointerInput` with a `Touch` pointer kind, but
no scrying-side runtime test exercises it. Marked `?` rather than
guessing.

[^mac-touch]: WKWebView has no `--touch-test` flag in the demo-mac
suite. Marked `?` rather than guessing.

[^gtk-touch]: WebKitGTK has no `--touch-test` flag in the demo-linux
suite. Marked `?` rather than guessing.

[^win-ime]: WebView2 has no `--ime-test` flag in the demo-win suite.
Marked `?` rather than guessing — Windows IME goes through `WM_IME_*`
messages which the producer's `WindowProc` does handle, but scrying
doesn't currently surface focus/change/blur events back to the host
the way the GTK and WPE producers do.

[^mac-ime]: WKWebView has no `--ime-test` flag in the demo-mac suite,
and the module doc does not describe a JS-side IME observability path
analogous to the GTK / WPE `scryIme` handler. Marked `?` rather than
guessing.

[^win-drag]: WebView2 exposes the native `DragEnter` / `DragOver` /
`DragLeave` API on the composition controller; see
[`webview2_composition_producer/input.rs`](../scrying/src/webview2_composition_producer/input.rs).
This is the OS drag-drop path, not the synthetic HTML5 DOM-event
injection path that WebKitGTK uses.

[^mac-drag]: WKWebView in scrying's capture-mode is structurally
unable to forward host drag/drop without SPI (`NSDraggingInfo`
synthesis). Overlay-mode hosts get drag/drop for free via AppKit's
responder chain without producer involvement. See
[`wkwebview_producer/trait_impl.rs`](../scrying/src/wkwebview_producer/trait_impl.rs)
around `send_drag_input`.

[^gtk-drag]: WebKitGTK 4.1 implements `send_drag_input` by injecting
HTML5 drag/drop DOM events through the JS message bridge — covered by
the `--drag-test` flag in the demo-linux suite.

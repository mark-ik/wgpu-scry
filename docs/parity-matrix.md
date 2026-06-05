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
| Frame transport | Shared D3D11 texture via WGC (`ImportedTexture`) [^win-frame] | CPU snapshot + ScreenCaptureKit → `IOSurface`/`MTLTexture` (`ImportedTexture`) [^mac-frame] | CPU snapshot (`CpuRgba`) via `webkit_web_view_get_snapshot` | CPU snapshot (`CpuRgba`) via `webkit_web_view_get_snapshot` → `gdk::Texture::download` | DMABUF fds + optional `VkSemaphore` opaque fd (`DmaBufImage`) [^wpe-dcc] |
| Navigation (load HTML, load URL, wait_for_load) | ✔ | ✔ | ✔ | ✔ | ✔ |
| Resize at runtime | ✔ | ✔ | ✔ | ✔ | ⚠ [^wpe-resize] |
| Keyboard input dispatch | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ |
| Mouse input dispatch | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ |
| Pointer input dispatch (pen/stylus) | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ |
| Touch input dispatch | ? [^win-touch] | ? [^mac-touch] | ? [^gtk-touch] | ✘ [^wk6-firstslice] | ⚠ [^wpe-touch] |
| Scroll input dispatch | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ |
| Cookie get/set/delete | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ |
| Custom URL scheme handlers | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ [^wpe-scheme-coverage] |
| Cursor-shape reporting | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ [^wpe-cursor-coverage] |
| IME observability (focus/change/blur) | ? [^win-ime] | ? [^mac-ime] | ✔ | ✘ [^wk6-firstslice] | ✔ |
| Script-message bridge (host ↔ page postMessage) | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ |
| Download lifecycle observability | ✔ | ✔ | ✔ | ✘ [^wk6-firstslice] | ✔ |
| Drag/drop | ✔ [^win-drag] | ✘ [^mac-drag] | ✔ [^gtk-drag] | ✘ [^wk6-firstslice] | ✘ [^wpe-drag] |
| Find-in-page | ✔ | ✔ | ? | ✘ [^wk6-firstslice] | ✘ |
| PDF rendering | ✔ | ✔ | ? | ✘ [^wk6-firstslice] | ✘ |
| Profile data isolation | ✔ | ✔ | ? | ? | ✔ |
| Process-isolation/recovery | ✔ | ✔ | ✔ | ? | ? [^wpe-process] |

[^win-frame]: WebView2 composition-controller → WinComp visual →
`Windows.Graphics.Capture::CreateFromVisual` → `Bgra8Unorm` D3D11
texture, bridged into the host as a shared NT handle for
`wgpu-native-texture-interop` import.

[^mac-frame]: WKWebView ships *two* frame transports: a CPU
`takeSnapshot:` path (one-shot, >50ms latency, `CpuSnapshot` tier) and
a ScreenCaptureKit → `IOSurfaceRef` → `MTLTexture` path for live
composited frames (`ImportedTexture` tier). The SCK path needs the
Screen Recording privacy permission. See
[`wkwebview_producer/mod.rs`](../scrying/src/wkwebview_producer/mod.rs).

[^wpe-dcc]: WPE delivers DMABUF frames correctly in *shape* (size,
format, plane layout verified on real WPE-on-AMD). Pixel-correctness
through the wgpu Vulkan importer is currently degraded on RADV with
DCC-compressed RGBA: wgpu 29.0.3's `create_texture_from_hal` tracks
every external texture as `UNDEFINED` regardless of imported state, so
wgpu's first-use barrier can discard contents under the Vulkan spec's
"transition from UNDEFINED may discard" rule. The producer already
emits a spec-correct foreign-queue acquire barrier; it stays dormant
until wgpu lands a `texture_from_raw` initial-state API.
See [`docs/wpe-deployment.md#wgpu-vulkan-pixel-correctness-note`](wpe-deployment.md#wgpu-vulkan-pixel-correctness-note).

[^wpe-resize]: WPE's headless `WPEToplevelHeadless::resize` vfunc is
unimplemented in WPEWebKit 2.52.3. `wpe_toplevel_resize` returns TRUE
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

[^wk6-firstslice]: WebKitGTK 6.0 is the Phase 5 first slice: navigate
+ offscreen-rendered CPU snapshot only. Cookies, URL schemes, input
forwarding, IME, cursor reporting, popup intercept, downloads, and
process-recovery are explicitly deferred to follow-on slices. See
[`scrying/src/webkit6_producer/mod.rs`](../scrying/src/webkit6_producer/mod.rs).

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

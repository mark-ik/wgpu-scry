# Linux port — WebKitGTK 4.1 first, three co-equal WebKit-family backends

**Date:** 2026-05-14
**Status:** Phase 2a landed; Phase 2b in flight.

This decision record captures the Linux producer work bootstrapped against a
fresh Fedora 44 development box. It supersedes the "WPE primary / WebKitGTK
fallback" framing in [`2026-05-07_platform_ceilings.md`](2026-05-07_platform_ceilings.md)
for the Linux section.

## Strategic decisions

### 1. Three co-equal WebKit-family backends, feature-gated

Linux gets **three** producers, not a primary + fallback pair:

| Backend | Toolkit | Engine | Status today |
| --- | --- | --- | --- |
| [`webkitgtk_producer`](../scrying/src/webkitgtk_producer/) | GTK 3 | WebKitGTK 4.1 | **Shipping** — Phase 2a |
| `webkitgtk6_producer` (planned) | GTK 4 + libadwaita | WebKitGTK 6.0 | **Deferred** — Phase 5+ |
| [`wpe_producer`](../scrying/src/wpe_producer.rs) | toolkit-less | WPE WebKit | **Scaffold** — Phase 4 |

The consumer picks via mutually-exclusive cargo features
(`webkitgtk-fallback` today; `webkit6` and `wpe` follow). `PlatformWebSurfaceProducer`
and `PlatformWebSurfaceConfig` in [`scrying::lib`](../scrying/src/lib.rs)
resolve at build time based on the selected feature. The producer trait
([`WebSurfaceProducer`](../scrying/src/lib.rs)) is unchanged across all three,
so host code stays uniform.

#### Build order: WebKitGTK 4.1 → WPE → WebKitGTK 6.0

- **WebKitGTK 4.1 first** because it has the largest installed base today
  (Tauri 1/2 still targets it; every Tauri-shipped Linux app pulls it in;
  legacy GNOME/GTK 3 embedders use it). Fedora 44 packages it as
  `webkit2gtk4.1-devel` 2.52.3.
- **WPE next** because it carries the strategic GPU-handoff frame
  contract (DMABUF + VkSemaphore — the only path that produces an
  importable native texture on Linux today). Packaging gap: WPE isn't
  in Fedora 44's repos, so we'll need a COPR or source build when we
  reach Phase 4.
- **WebKitGTK 6.0 last** because GNOME's GTK 4 migration is still
  catching up to the GTK 3 installed base; deferring lets the binding
  surface (`gtk4 = 0.11`, `webkit6 = 0.6`) settle further and gives the
  GTK 4 offscreen story time to converge.

### 2. Why `webkit2gtk = "2.0.2"` and `gtk = "0.18.2"` aren't "old pins"

A first read suggests we're stuck on outdated bindings. They aren't —
they are the **current head of the GTK 3 binding line**:

- gtk-rs split the lineage when it added `gtk4`; the GTK 3 `gtk` crate
  effectively froze at 0.18 because GTK 3 itself is in upstream
  maintenance.
- The Tauri-maintained `webkit2gtk` fork sits at 2.0.x for the same
  reason — the API surface it covers is complete for WebKitGTK 4.1.

Bumping inside that line is a no-op (`^2.0` and `^0.18` already pull the
latest patch versions). The real "modernize" move is **migrating to
`gtk4` + `webkit6`**, which is its own producer (Phase 5+), not a
version bump to this one.

### 3. Out of scope

- **CEF / Chromium** → [`wgpu-weld`](https://github.com/merely-made/wgpu-weld)'s
  job. Not WebKit-family, not "system webview" on Linux (apps vendor
  their own copy), different threading + sync story. Stays parked.
- **Servo** → [`wgpu-graft`](https://github.com/merely-made/wgpu-graft)'s
  job. Already covered there.
- **QtWebEngine** → cross-toolkit Chromium re-skin; same scope reasons
  as CEF without the installed-base argument.
- **`webkit2gtk-4.0`** (predecessor of 4.1, still on Ubuntu 22.04) —
  could be a feature flag if a downstream consumer needs it; not worth
  the maintenance burden speculatively.

## What landed in Phase 2a

### Code

- [`scrying/src/webkitgtk_producer/`](../scrying/src/webkitgtk_producer/)
  — replaces the single-file scaffold with a seven-module decomposition
  mirroring [`wkwebview_producer/`](../scrying/src/wkwebview_producer/):
  `mod` (capabilities + index), `config`, `producer` (construction +
  Drop), `navigation` (load + signal-handler-state + main-loop-pumped
  wait), `capture` (offscreen `webkit_web_view_get_snapshot` →
  `CpuRgba`), `helpers` (GTK init gate + main-loop pump), `trait_impl`.
  Each under the 600-LOC ceiling.
- [`scrying/src/lib.rs`](../scrying/src/lib.rs) — feature-gated
  producer selection. `SystemWebviewBackend::detect()` picks `WebKitGtk`
  when `webkitgtk-fallback` is on, `Wpe` otherwise. `PlatformWebSurfaceProducer`
  re-exports follow. `WebSurfaceCapabilities::probe()` delegates to per-backend
  `linux_*_capabilities()` shims.
- [`demo-linux/`](../demo-linux/) — new workspace member. Loads HTML
  into an offscreen WebView, takes a CPU RGBA snapshot, writes a PNG.
  Flags: `--probe-only`, `--snapshot-test` (non-zero exit on empty /
  zero-pixel snapshot), `--url`, `--out`, `--width`, `--height`.
- [`Cargo.toml`](../Cargo.toml) — `demo-linux` added to workspace
  members.

### Behavior verified

End-to-end on this Fedora 44 + Wayland + AMD Renoir Vega session:

```
backend: WebKitGtk
preferred mode: CpuSnapshot
CPU snapshot: Supported
navigating to inline HTML
committed: Some("about:blank")
CpuRgba snapshot: 800x600 gen=1
PASS: snapshot has non-zero pixel data
wrote /tmp/scrying-linux-smoke.png
```

The resulting PNG visually contains the rendered HTML (dark gradient
background, yellow "scrying · linux" text). Confirmed that the producer:

- Initializes GTK on the main thread once per process
- Builds a persistent `WebContext` rooted at the configured `data_dir`
- Hosts the `WebView` inside a self-owned `GtkOffscreenWindow` (the
  host never sees a GTK widget)
- Drives `load_html` / `load_uri` to `LoadEvent::Finished` via
  main-loop pumping
- Runs `webkit_web_view_get_snapshot` to delivery, un-premultiplies the
  cairo ARGB32 bytes to RGBA, emits `WebSurfaceFrame::CpuRgba`

## Known issues / gotchas

### GDK + WebKit accelerated compositing + Wayland

WebKitGTK 2.40+ default-enables a DMABUF renderer plus accelerated
compositing, which both go through GDK to create a GL context. On at
least this Fedora 44 + Wayland session that fails fatally:

```
** (demo-linux:NNNN): ERROR **: GDK is not able to create a GL context:
The current backend does not support OpenGL.
```

Notably, `glxinfo` reports a working `radeonsi` GL stack with direct
rendering — so the failure is specific to GDK 3 + WebKit's
context-creation path, not a missing GL stack.

The CPU snapshot path doesn't need AC, so `demo-linux/src/main.rs`
sets the following process-wide before any GDK / WebKit call:

```rust
unsafe {
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
}
```

Hosts that *need* WebKit's AC path (a future DMABUF capture upgrade)
must instead get GDK GL creation working on their target session.
The producer doesn't set these env vars itself — they're process-wide
and hosts that share the process with other WebKit instances must
opt in deliberately.

### `committed_uri` returns `about:blank` after `load_html`

WebKit treats inline HTML as a load against `about:blank` unless the
caller supplies a `base_uri`. `WebKitGtkProducer::committed_uri()`
reflects this honestly. Hosts that want a meaningful "where am I"
indicator should supply a base URI through a future `load_html_with_base`
inherent.

## Phase 2b plan (in flight)

Closing the [parity matrix](2026-05-07_platform_ceilings.md#cross-platform-parity-matrix)
rows that are `?` for Linux WebKitGTK 4.1. Order, smallest first:

1. **Browser controls** — `reload`, `stop`, `go_back`, `go_forward`,
   `can_go_back`, `can_go_forward`. Each is a one-line dispatch to
   `WebViewExt`.
2. **Navigation events polling** — `poll_navigation_event` drains an
   FIFO queued by the existing `connect_load_changed` /
   `connect_load_failed` handlers, plus `connect_title_changed` for
   `NavigationEvent::TitleChanged`.
3. **`apply_settings`** — zoom factor, JS enabled, devtools, custom
   user-agent. Maps onto `WebKitSettings`.
4. **PNG snapshot** — `capture_snapshot_png` encodes the existing
   `CpuRgba` path through `image::ImageOutputFormat::Png`.
5. **`open_devtools_window`** — `WebKitWebInspector::show` once
   `WebKitSettings::set_enable_developer_extras(true)`.
6. **JS messaging** — `post_web_message` + `poll_web_message` via
   `WebKitUserContentManager` script messages and `webview.run_javascript`.
7. **CI** — `scripts/test-linux.sh` + `.github/workflows/test-linux.yml`
   running `demo-linux --snapshot-test` and `--probe-only`.

### Phase 2b *heavy* (landed 2026-05-14): synthesized DOM event dispatch

Mouse, keyboard, pointer, and focus forwarding land via JS event
synthesis through `evaluate_javascript` — same shape the macOS
WKWebView producer uses for `send_pointer_input` ("mouse-shaped JS
pointer events" in the parity matrix), generalized to all input
kinds. Lives in
[`scrying/src/webkitgtk_producer/input.rs`](../scrying/src/webkitgtk_producer/input.rs).

Event-kind mapping:

- `MouseInput` → `MouseEvent` (`mousedown` / `mouseup` / `dblclick` /
  `mousemove` / `mouseleave`) or `WheelEvent` (`wheel` with
  `deltaX` / `deltaY` from the WebView2-shaped 120-per-notch
  convention).
- `PointerInput` → `PointerEvent` (`pointerdown` / `pointerup` /
  `pointermove` / `pointerenter` / `pointerleave` / `pointercancel`).
- `KeyboardInput` → `KeyboardEvent` (`keydown` / `keyup`) on
  `document.activeElement` with `key`, `code`, `keyCode`, modifier
  flags, and `repeat` plumbed through.
- `FocusReason` (any) → `Widget::grab_focus()` on the WebView plus
  `document.body.focus()` for a sensible JS-side target.

**Fidelity caveat:** synthesized events arrive with
`event.isTrusted === false`. Pages that gate behaviour on
`isTrusted` (autoplay user-gesture checks, click-fraud defences,
`requestFullscreen()`) refuse them. Native click side-effects the
engine fires from real gestures — form submission via Enter,
native context menu, focus stealing — don't fire. For most DOM
event handlers and form-field updates the path works; the
`demo-linux --input-test` mode verifies a click round-trip
(button `mousedown` / `mouseup` handlers postMessage back, host
asserts on the response) and a `keydown` round-trip.

### Phase 2c (landed 2026-05-14): native `GdkEvent` dispatch — `isTrusted = true`

The native path lives in
[`scrying/src/webkitgtk_producer/input_native.rs`](../scrying/src/webkitgtk_producer/input_native.rs).
Synthesizes `GdkEventButton` / `GdkEventMotion` / `GdkEventScroll` /
`GdkEventCrossing` / `GdkEventKey` against the WebView's realized
`GdkWindow` (the offscreen window's child surface) and dispatches via
`gtk_main_do_event`. Result: DOM events arriving at page handlers
report `event.isTrusted === true`, fire native click side-effects,
and let WebKit's engine-level shortcut handling run.

Architecture notes:

- **Field setting** through gdk-rs's `event.downcast_mut::<EventButton>()` →
  `&mut gdk_sys::GdkEventButton` raw struct access. Safe within an
  `unsafe` window for the `g_object_ref`-then-assign on the window
  pointer. The device pointer goes through the higher-level
  `event.set_device(Some(&device))` API which handles the
  event-type-specific storage details (some types carry device
  inline, some in a private side table).
- **Refcount discipline**: `gdk_event_free` walks the event's fields
  and unrefs owned `GObject` pointers. The producer's
  `widget.window()` / `seat.pointer()` references are bumped via
  `g_object_ref` before assignment so the event's destructor takes
  out the right ref count without underflowing the host's.
- **Pointer fallback**: the JS-event path
  ([`super::input`](../scrying/src/webkitgtk_producer/input.rs))
  remains compiled as a fallback for the case where the WebView's
  `GdkWindow` isn't realized yet — that path produces
  `isTrusted = false` events but at least DOM handlers still fire.
- **Verified by [`demo-linux --input-test`](../demo-linux/src/main.rs)**:
  asserts on `trusted=true` in the page-side messages. PASSES on
  Fedora 44 + Wayland.

Open items that didn't land in Phase 2c:

- **Full IME (CJK / non-Latin preedit + commit)** — requires wiring a
  `GtkIMContext` against the WebView, listening for `preedit-changed`
  / `commit` signals, and bridging to the host-side text-input
  contract that the macOS producer carries (`TextInputFocused` /
  `TextInputChanged` / `TextInputBlurred` events, caret-rect plumbing).
  Separate sub-slice.
- **True multi-touch `EventTouch`** — `dispatch_pointer` currently
  routes through the mouse path. Real touch needs per-sequence
  `GdkEventTouch` synthesis with `event_sequence` tracking.
- **Cursor-shape reporting** — observe the WebView's `GdkWindow`
  cursor via property notify, push as `CursorShape` for `poll_cursor_shape`.

## Phase 4 plan (WPE)

Out of scope for today; recorded for continuity:

- Source-build or COPR for `wpebackend-fdo` + `wpe-webkit` on Fedora 44
  (the libs aren't packaged in Fedora 44's repos).
- Implement [`wpe_producer`](../scrying/src/wpe_producer.rs)'s FFI
  callback bridge: `WPEViewBackendDMABuf` exports DMABUF fds +
  format/modifier + `VkSemaphore` opaque-fd; convert into
  [`DmaBufImage`](../scrying/src/native_frame/mod.rs) and drive
  `enqueue_dmabuf_frame`.
- Implement `import_dmabuf_image` in
  [`scrying/src/native_frame/mod.rs`](../scrying/src/native_frame/mod.rs)
  via wgpu-hal Vulkan escape hatch: `VK_KHR_external_memory_fd` for
  the DMABUF, `VK_KHR_external_semaphore_fd` for the per-frame
  semaphore, then `texture_from_raw` into a `wgpu::Texture`.

## Phase 5+ plan (WebKitGTK 6.0)

A sibling feature `webkit6` alongside `webkitgtk-fallback`, mutually
exclusive. Target stack: `gtk4 = "0.11"` + `webkit6 = "0.6"` against
Fedora's `webkitgtk6.0-devel`. Open question: GTK 4 dropped
`GtkOffscreenWindow`; the offscreen-render path needs replacing
(probably a hidden `gtk::Window` without `present()`, relying on
WebKit's GPU process rendering independently of widget visibility).
Verify empirically before committing.

## References

- [`scrying/src/webkitgtk_producer/mod.rs`](../scrying/src/webkitgtk_producer/mod.rs) — module-level docs
- [`2026-05-07_platform_ceilings.md`](2026-05-07_platform_ceilings.md) — original parity matrix (this doc supersedes the Linux section)
- [`2026-05-09_browser_parity_checklist.md`](2026-05-09_browser_parity_checklist.md) — per-row parity tracking
- [`2026-05-12_windows_decomposition_plan.md`](2026-05-12_windows_decomposition_plan.md) — the decomposition pattern the `webkitgtk_producer/` directory follows

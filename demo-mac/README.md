# demo-mac

Minimal winit + wgpu host probe for scrying's macOS WKWebView producer.
Counterpart to [`../demo-win`](../demo-win/) on Windows.

The demo's job is to **drive scrying's runtime paths from a real
event loop** so the producer's many slices (lifecycle, navigation,
input forwarding, JS messaging, snapshots, ScreenCaptureKit pipeline,
KVO observers, per-profile data store, etc.) get exercised against
actual AppKit / WebKit / Metal — not just compile-tested.

## Modes

By default the demo opens an overlay-mode WebView at
`https://example.com` and idles, logging navigation events and JS
messages to stdout. The CLI flags below add automated test modes.

### `--probe-snapshot`

Requests a CPU snapshot ~3 s after launch via the non-blocking
[`request_snapshot`](../scrying/src/wkwebview_producer.rs) /
[`poll_snapshot`](../scrying/src/wkwebview_producer.rs) pair, writes
it to `demo-mac-snapshot.png`, and exits. Automated proof-of-life for
slice F (no interactive input required).

### `--scripted`

Loads an inline test page with known DOM (input box + JS message
listener + scrollable region) and drives a state machine that:

- posts a host message and asserts the JS shim echoes it back
  → **slice E** (bidirectional JS messaging) full round-trip
- dispatches 6 scroll-wheel events
  → **slice G** (CGEvent → NSEvent → `scrollWheel:`) API-level
- dispatches 3 keyDown/keyUp pairs typing `abc`
  → **slice I** (`keyEventWithType:` → `keyDown:` / `keyUp:`)
  API-level

Prints `PASS` / `FAIL` and exits. The keyboard / scroll DOM-effect
assertions are best-effort because synthetic events from an offscreen
or unfocused window are filtered by WebKit's hit-testing — slice E,
which round-trips through the `chrome.webview` shim, is the strongest
end-to-end demonstration. G/I are asserted at the API-dispatch level.

### `--capture`

Sets up a Metal-backed wgpu surface tied to the winit window,
positions the WKWebView at the **left half** of the window, kicks
off [`start_capture_async`](../scrying/src/wkwebview_producer.rs)
~3 s after launch, and once `capture_status` reports `Live` starts
rendering each imported `wgpu::Texture` to the right half of the
surface (with a slight tint so the wgpu-rendered region is
distinguishable from the directly-composited WKWebView subview on
the left).

Verifies, end-to-end:

- Slice B (ScreenCaptureKit pipeline)
- Slice M2 (`native_frame::metal::import` — `wgpu::hal::metal::Device::texture_from_raw`)
- Slice N (`SCStream::updateConfiguration:` on resize, when paired
  with `--resize-test`)

### `--capture --dump-every N`

Every Nth imported texture is read back via `copy_texture_to_buffer`
and saved as `demo-mac-frame-NNNN.png`. Pixel-faithful proof of the
IOSurface → MTLTexture → wgpu chain.

### `--capture --resize-test`

Programmatically resizes the window mid-run on a 6/10/14 s schedule.
Each resize triggers the producer's `resize` and (because capture is
live) `SCStream::updateConfiguration:`. Verifies slice N at runtime;
combine with `--dump-every` to see captures at the new dimensions.

### `--browser-test`

Drives items 1, 3, 4, and 9 of the browser-class roadmap on a timed
schedule and asserts deterministic effects:

- **Item 1 — history controls**: registers a `scrying-test://` URL
  scheme, loads `scrying-test://history-1` then
  `scrying-test://history-2`, calls `go_back` and `go_forward`,
  observes the round-trip via `SourceChanged` nav events.
- **Item 3 — settings**: calls `apply_settings(zoom=1.5,
  user_agent=…, javascript_enabled=true, devtools_enabled=false)`,
  asserts `Ok`.
- **Item 4 — URL schemes**: any navigation to `scrying-test://*`
  succeeds via the registered scheme handler. Verified by item 1's
  navigation events.
- **Item 9 — find / PDF**: navigates to a page containing the
  marker `scrying-find-marker`, calls `find_in_page`, asserts
  `poll_find_match → Some(true)`. Then `request_pdf` and asserts
  `poll_pdf → Some(Ok(>100 bytes))`.

Items 2, 5, 6 (new-window intercept, process-failure recovery,
auth challenges) need network or harder-to-trigger conditions
and aren't covered by `--browser-test`. They'll pick up runtime
coverage as mere drives them. Item 8 (downloads) has its own
mode — see [`--download-test`](#--download-test) below.

### `--interaction-state-test`

Loads three pages via the `scrying-test://` scheme, serializes
the interaction state at the third, walks `go_back` twice so the
back-forward cursor is at the first page, restores the captured
blob, and asserts the WebView ends up back at the third page
with `can_go_back == true` / `can_go_forward == false`. Verifies
`serialize_interaction_state` ↔ `restore_interaction_state`
round-trip preserves the WebKit-internal back-forward list.

### `--pointer-input-test`

Loads a page with `pointerdown` / `pointermove` / `pointerup` /
`pointerleave` listeners that bridge each event to the JS host
bridge, then synthesizes Down → Update → Up → Leave via
`send_pointer_input` (with `PointerDevice::Touch`). Asserts the
JS side observes `pointerdown`, `pointermove`, and `pointerup`,
and records the JS-reported `pointerType` so the
"macOS collapses every device to mouse" mapping is documented.

### `--incognito-test`

Stands up two producers in one process: one with
`non_persistent = true`, one persistent at a separate `data_dir`.
Sets a uniquely-named cookie via `set_cookie` on the incognito
producer, queries both stores via `request_all_cookies` /
`poll_cookies`, and asserts the cookie is visible to the
incognito producer but absent from the persistent one — proves
non-persistent stores stay isolated from persistent ones.

### `--download-test`

Spins up a loopback HTTP server (random `127.0.0.1:0` port)
serving a 64 KiB known-pattern body with
`Content-Disposition: attachment`, then drives three sub-phases:

- **Basic**: navigates to the loopback URL, asserts
  `DownloadStarted`/`DownloadProgress`/`DownloadFinished` events
  carry a real `DownloadId` and `destination_path`, and that the
  bytes on disk byte-for-byte match what was served.
- **Host Cancel decision**: registers a `set_download_handler`
  that returns `DownloadDecision::Cancel`, triggers a second
  download, asserts `DownloadCancelled` fires for a fresh ID
  *without* either `DownloadStarted` or `DownloadFinished` for
  that ID.
- **`cancel_download(unknown_id)`**: asserts the API returns
  `Ok(false)` for an ID that was never issued.

The HTTP-server detour is necessary because WebKit doesn't
promote custom URL-scheme responses to downloads regardless of
MIME type or `decidePolicyForNavigationResponse:` override.

### `--capture-test`

Smoke test for the ScreenCaptureKit pipeline. Implies
`--capture` (so the wgpu render context, half-window webview,
and `start_capture_async` kickoff all light up) and forces
`--visible` (SCK can't bind an `SCContentFilter` to a hidden
window). Once `capture_status` reports `Live`, drains 5 frames
via `try_acquire_frame` and asserts each frame's reported
`(width, height)` matches the configured webview size.

The default `scripts/test-mac.sh` suite still holds this out because Screen
Recording permission can't be granted from inside the test process. Trusted
headed runners with a pre-grant execute it through `CAPTURE=1`; that suite also
runs `--capture-test --resize-test` and requires repeated frames at every
scheduled capture size. Run either gate manually:

```bash
cargo run -p demo-mac -- --capture-test
cargo run -p demo-mac -- --capture-test --resize-test
```

The self-hosted hardware workflow runs the full suite with
`VISIBLE=1 CAPTURE=1`. `VISIBLE=1` adds `--visible` to the modes that normally
hide their AppKit window; the hosted macOS workflow keeps the default headless
behavior. The bounded two-tab assertion is already visible and retains its
auto-exit deadline.

### `--profile-test`

Persistent-store complement to [`--incognito-test`](#--incognito-test):
asserts that two persistent producers at the **same** `data_dir`
share their cookie store. Stands up a primary + a secondary
producer at a PID-suffixed `target/demo-mac-profile-test-<pid>/`
path, sets a uniquely-named cookie on producer #1 via the
`set_cookie` API, then queries both producers' stores via
`request_all_cookies` and asserts the cookie shows up in both.
Proves `WKWebsiteDataStore::dataStoreForIdentifier:` returns the
same backing store for the same path-derived UUID across
producer instances. Self-contained — no manual two-run flow.

### `--two-tabs`

Multi-instance independence assertion (browser-class item 7):
two producers in one process, both subviews of the host
window, navigating independently to distinct
`scrying-test://` URLs. Drains nav events from each
producer's queue and asserts tab #1 saw history-1 events
(and not history-2), and vice versa — proving multiple
producers in one process keep independent event streams with
no cross-talk.

## Interactive keys

Outside test modes, the following hot-keys work:

- `S`: request a CPU snapshot. The completion handler writes
  `demo-mac-snapshot.png` when ready.
- `M`: post a JS-side host message
  (`window.chrome.webview.addEventListener('message', ...)`
  recipients see it).

All other input (typing, clicks, scrolls) goes to the WKWebView
via AppKit's natural responder chain — the demo doesn't
double-dispatch through `send_mouse_input` / `send_keyboard_input`
in overlay mode (that path is gated to `--capture` mode, where
the WKWebView is rendering into a captured texture and is
therefore not directly clickable).

## Architecture

| File | Role |
| --- | --- |
| `src/main.rs` | winit application handler, event routing, scripted state machine |
| `src/render.rs` | wgpu surface + import + render pipeline + readback |

The demo only depends on **stable scrying APIs** — no internal types,
no SPI. If you can build and run this, downstream consumers will be
able to wire up scrying the same way.

## Known runtime caveats observed during development

- **Blocking trait methods deadlock from inside winit handlers.**
  `navigate_to_url`, `navigate_to_string`, `capture_cpu_snapshot`,
  and `start_capture` all pump the main `NSRunLoop` to wait on a
  delegate callback. From inside winit's `resumed` /
  `window_event` (which runs *under* the same NSRunLoop), the pump
  re-enters winit's event handler and trips its
  "no nested event handling" guard, panicking. **Always prefer the
  non-blocking inherent variants** (`load_url`, `load_html`,
  `request_snapshot` + `poll_snapshot`, `start_capture_async` +
  `capture_status`) when calling from a host event-loop callback.
- **Synthetic input is filtered when the window isn't user-focused.**
  WebKit's hit-testing accepts AppKit-routed events but may ignore
  events delivered straight to the responder methods if the window
  isn't on top. The producer dispatches them correctly; whether
  WebKit acts on them is downstream of scrying's contract.
- **Capture mode shows recursive content.** SCK captures the entire
  window framebuffer. Our wgpu surface displays the imported texture.
  In capture mode that texture *includes* our previous render's
  output, so the right half visibly shows nested capture frames.
  Not a bug — a consequence of capturing the same window we render
  into. Production consumers will typically render capture into a
  *different* window or surface.

# Phase 4c.2 — WPE producer on WPEPlatform headless (DMABUF frame seam)

Implements checklist item **4c.2** from
[`2026-05-15_phase4_strategy.md`](2026-05-15_phase4_strategy.md), as
revised by the post-install findings in
[`2026-05-20_phase4b_wpe_bindings_decision.md`](2026-05-20_phase4b_wpe_bindings_decision.md):

> **4c.2** Inline FFI for WPEPlatform headless + `WPEBufferDMABuf` →
> `DmaBufImage` → `enqueue_dmabuf_frame` (the `buffer-rendered` seam)

This spec covers only the **frame production seam**: standing up a
headless WPE display + WebKitWebView, and turning rendered
`WPEBufferDMABuf` buffers into the existing `DmaBufImage` contract that
Phase 4a's Vulkan importer already consumes. Navigation (4c.3), input,
cookies, schemes, demo, and docs are out of scope here.

Verified against the installed runtime: **WPEWebKit 2.52.3, libwpe
1.16.2, wpebackend-fdo 1.16.1** on Fedora 44 (AMD Renoir / Mesa /
Wayland).

## Scope

In scope:
- A self-owned `WpeProducer` that constructs a `WPEDisplayHeadless`, a
  `WebKitWebView` bound to it, and connects the view's `buffer-rendered`
  signal.
- The `buffer-rendered` → `WPEBufferDMABuf` → `DmaBufImage` →
  `pending_frame` seam, including fd ownership.
- `glib`-backed GObject mechanics (refcount, signal connect) per the
  4b decision (option 2).

Out of scope (later 4c steps, unchanged by this spec):
- Navigation / `load_uri` / `load_html` completion waits (4c.3).
- Input forwarding, cookies, URI scheme handlers.
- Explicit producer-sync semaphores (see "Sync" below).
- Any `wpe-rs` published bindings (deferred per 4b).

## Design decisions

### 1. Threading: thread-affine producer + synchronous loop pump (model A)

The producer is **affine to the thread that constructs it**, and the
consumer drives progress by **synchronously pumping the GLib main
context** on that thread — exactly the pattern the other three backends
already use:

| Platform | Affinity | Pump |
|---|---|---|
| macOS / WKWebView | hard main-thread (asserts `MainThreadMarker`) | AppKit run loop |
| Windows / WebView2 | STA, creating thread | `pump_messages_for(16ms)` |
| Linux / WebKitGTK | GTK main thread | `gtk::main_iteration_do(false)` |
| **Linux / WPE (this)** | **construction thread** | **`glib::MainContext::iteration(false)`** |

A dedicated WPE thread with its own `GMainLoop` was rejected: it is the
**least** portable shape (macOS structurally forbids an off-main-thread
WebView), it would make Linux the odd backend out, and its only unique
benefit — frames accumulating without the consumer pumping — is offered
by none of the current backends. Frame delivery is already
async-callback-into-shared-state everywhere; under model A the
`buffer-rendered` closure writes the shared slot and a pumped
`acquire_frame` is non-blocking from the producer thread.

The pump helper mirrors `webkitgtk_producer::helpers::pump_until`
(`MainContext::iteration(false)` + a ~2ms nap, deadline-bounded).

### 2. Constructor: safe, self-owned `new(config)`

The scaffold's `unsafe fn new(view_backend: *mut c_void, config)` is
inherited from the legacy libwpe model where the host creates a
`wpe_view_backend *` and passes it in. It has **no live caller**. The
WPEPlatform headless pivot removes the host pointer entirely.

New shape, mirroring `WebKitGtkProducer::new` (which owns its own
`GtkOffscreenWindow`):

```rust
impl WpeProducer {
    pub fn new(config: WpeProducerConfig) -> Result<Self, WebSurfaceError>;
}
```

- Safe (no raw pointer in, no `unsafe`). The producer internally owns
  the `WPEDisplay`, `WebKitWebView`, and `WPEView`.
- Validates non-zero size (kept from scaffold).
- New failure mode: error if `wpe_display_headless_new()` returns null /
  the display is unavailable.
- Just `new` for now; `new_with_url_schemes` and friends land with the
  control surface in a later 4c step.
- `enqueue_dmabuf_frame` stays public **as a test-injection seam only**;
  real frames now arrive via the internal `buffer-rendered` closure, not
  this method.

### 3. Frame seam + fd lifecycle: dup-in-handler, producer-owned fds

**The collision.** Phase 4a's importer *consumes* the fd —
`dmabuf.rs` / `native_frame/mod.rs:187`: "ownership of `fd` transfers to
the driver on success — we must NOT close it ourselves." Meanwhile WPE
owns the buffer's fds (`wpe_buffer_dma_buf_get_fd`), valid only until
`wpe_view_buffer_released`, after which WPE may recycle/close them. Two
independent owners (Vulkan driver, WPE pool) cannot share one raw fd
without a double-close / use-after-close.

**Resolution: `dup()` the fd in the signal handler; the producer owns
the duped fds; release WPE's buffer immediately.** This is handle
duplication (the defined DMABUF handoff idiom), not code duplication.

Inside the `buffer-rendered` glib closure:

1. Downcast `WPEBuffer` → `WPEBufferDMABuf` (glib downcast).
2. For each of `wpe_buffer_dma_buf_get_n_planes`: **`dup()` the fd from
   `get_fd(plane)`**, read `get_offset(plane)` / `get_stride(plane)`.
   Read `get_format` / `get_modifier` once.
3. Build `DmaBufImage` holding the duped, producer-owned fds.
4. **Immediately `wpe_view_buffer_released(view, buffer)`** — return
   WPE's buffer so its pool is not starved by consumer latency.
5. Store into the single-slot `pending_frame`. **If evicting a stale
   frame, close that frame's plane fds (and `semaphore_fd` if any)
   first** — `DmaBufImage` is `Clone, Copy` with no `Drop`, so the
   producer is responsible or the fds leak.
6. On producer teardown, close fds of any unconsumed frame still in the
   slot.
7. `acquire_frame` transfers fd ownership to the caller → the Phase 4a
   importer consumes per the existing contract. The shared
   `DmaBufImage` / importer contract is **untouched**; all WPE-specific
   fd handling stays local to this producer.

Rejected alternative — holding a `WPEBufferDMABuf` ref until import:
couples WPE's buffer-pool occupancy to consumer import latency (slow
consumer → stalled rendering), threads a GObject handle through to
release time, and fights the single-slot drop-stale model.

### 4. Sync: implicit (`SyncMechanism::None`) for 4c.2

`WPEBufferDMABuf` exposes **no fence getter** (only
`format/n_planes/fd/offset/stride/modifier`; `wpe_buffer_dma_buf_new`
*takes* a fence on creation but nothing reads one back). So rendered
frames are tagged `SyncMechanism::None`. Phase 4a's
`VK_KHR_external_semaphore_fd` wait path stays dormant for WPE and
becomes additive if/when WPE surfaces a render fence.

## FFI surface (inline, `extern "C"`, linked via `wpe-webkit-2.0`)

All WPEPlatform symbols are bundled into `libWPEWebKit-2.0.so` on this
build (there is **no** standalone `libWPEPlatform-2.0.so`);
`pkg-config --libs wpe-platform-2.0` forwards to `-lWPEWebKit-2.0`. The
build script links `wpe-webkit-2.0`.

Confirmed exported symbols / headers:

```text
# wpe-platform (in libWPEWebKit-2.0.so), headers under
# /usr/include/wpe-webkit-2.0/wpe-platform/
wpe_display_headless_new() -> *WPEDisplay
wpe_buffer_dma_buf_get_format/_get_n_planes/_get_fd(plane)/
  _get_offset(plane)/_get_stride(plane)/_get_modifier
wpe_buffer_dma_buf_get_type   # for the glib downcast type check
wpe_view_buffer_released(view, buffer)
wpe_view_get_type

# wpe-webkit, headers under /usr/include/wpe-webkit-2.0/wpe/
webkit_web_view_get_wpe_view(web_view) -> *WPEView
webkit_web_view_get_display(web_view) -> *WPEDisplay
```

GObject mechanics (refcount, `g_signal_connect` for `buffer-rendered`,
type registration for downcasts) come from the `glib`/`gobject` crates,
not hand-written — per the 4b option-2 decision. Only the WPE-specific
calls above are hand-written `extern "C"`.

### Open detail to validate first in implementation

The exact **display → WebView binding** is the one thing headers can't
confirm (GObject properties are registered in the `.c`). WPE's
`webkit_web_view_new(WebKitWebViewBackend*)` is the *legacy* ctor; the
WPEPlatform path is expected to be
`g_object_new(WEBKIT_TYPE_WEB_VIEW, "display", display, NULL)` (a
"display" construct property). **First implementation step: construct
the WebView against the headless display, then assert
`webkit_web_view_get_display(view) == display` and that
`webkit_web_view_get_wpe_view(view)` is non-null.** The producer's
ownership structure (it owns display+view+webview) is the same whichever
the binding turns out to be, so this does not affect the rest of the
design.

## Producer structure changes

- `WpeProducer` gains owned GObject handles: the `WPEDisplay`, the
  `WebKitWebView`, and the `WPEView` (held for `wpe_view_buffer_released`
  + signal lifetime). `glib` refcounting manages their lifetimes.
- `pending_frame: Arc<Mutex<Option<DmaBufImage>>>` stays, but the
  `buffer-rendered` closure (not `&mut self`) writes it. The
  `generation` counter moves into shared state reachable by the closure
  (e.g. an `Arc<AtomicU64>` or inside the mutex) since the closure can't
  borrow `self`.
- A new internal module split is reasonable if `wpe_producer.rs` grows
  past its current ~210 lines (e.g. `wpe_producer/{mod,ffi,frame}.rs`),
  following the per-producer module layout the other backends use.

## Testing strategy

- **Unit (no WPE):** `enqueue_dmabuf_frame` validation + the stale-evict
  fd-close path (using pipe fds so close is observable) keep working
  without a display.
- **Runtime smoke (gated on WPE + a GPU):** construct the producer,
  bind a headless display, `load_html` a solid-color `data:` page, pump
  until one `buffer-rendered` lands, assert the resulting `DmaBufImage`
  has non-zero size, ≥1 plane, a valid duped fd, and a sane
  format/modifier. This mirrors `demo-linux`'s existing first-frame
  gate. Navigation arrives in 4c.3, so 4c.2's smoke may use the lowest
  navigation primitive needed to trigger a paint.
- Validation layers (`VK_LAYER_KHRONOS_validation`) on for the import
  round-trip, as Phase 4a already does.

## Deferred / explicitly not done here

- Navigation API (4c.3), input, cookies, scheme handlers.
- Explicit producer-sync semaphore (no WPE fence source yet).
- Published `wpe-rs` bindings (4b: separate repo, later).
- Multi-plane import beyond what Phase 4a's importer currently handles
  (it imports `planes[0]`); the seam captures all planes into
  `DmaBufImage` so the importer can grow into them.

## Checklist deltas

- [x] **4c.1** Working WPE install → philn COPR, 2.52.3, smoke-tested.
- [ ] **4c.2** This spec → implement the headless + `buffer-rendered`
  seam.
- [ ] **4c.3** WebView navigation bound to the headless display
  (next).

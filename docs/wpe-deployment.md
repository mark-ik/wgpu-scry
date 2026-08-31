# WPE deployment

Practical guide for running scrying's WPE producer on Linux. Audience:
a Rust developer who knows Cargo features but is new to WPEWebKit's
install + runtime story.

The WPE producer is a *headless* WebKit backend: it constructs its own
`WPEDisplayHeadless` + `WebKitWebView` and exports rendered content as
DMABUF fds (plus an optional `VkSemaphore` opaque fd). It is the
strongest Linux producer in the parity matrix — pre-composition GPU
extraction with explicit cross-API sync — but it is also the most
constrained at install time.

## Prerequisites

### WPEWebKit 2.52.5

The current headed receipt uses **WPEWebKit 2.52.5**. `build.rs` accepts the
2.52 stable line by enforcing `wpe-webkit-2.0 >= 2.52` via `pkg-config` and
fails the build with a clear error if the package is missing.

Companion versions used in development:

- libwpe 1.16.3
- wpebackend-fdo 1.16.1 (used by upstream WebKit; the producer itself
  is built on the *new* WPEPlatform API, not the legacy fdo path)

### Fedora 44: install via the philn COPR

Fedora 44 does not ship WPE. The canonical source is the
[`philn/wpewebkit`](https://copr.fedorainfracloud.org/coprs/philn/wpewebkit/)
COPR maintained by Philippe Normand (Igalia) — credible upstream
provenance.

Enable the COPR:

```sh
sudo dnf copr enable philn/wpewebkit
```

As of 2026-08-31 the Fedora 44 repository carries the complete 2.52.5 engine
and development package set, so the old direct-RPM URL workaround is gone:

```sh
sudo dnf install -y wpewebkit-devel libwpe-devel wpebackend-fdo-devel
```

There is no `cog` build for F44 in the COPR. For an out-of-band smoke
test, use the bundled MiniBrowser at
`/usr/libexec/wpe-webkit-2.0/MiniBrowser`:

```sh
/usr/libexec/wpe-webkit-2.0/MiniBrowser --headless 'data:text/html,<h1>hi</h1>'
```

### Runtime: GPU + Wayland session

The producer needs a *working* GPU initialization to construct the
headless display. Concretely:

- Mesa with a Vulkan driver. AMD: RADV (mesa-vulkan-drivers) or AMDVLK.
  Intel: ANV. NVIDIA: the proprietary stack.
- A Wayland session for the GPU bring-up path WPE's headless
  implementation uses internally. The producer never opens a window,
  but the GBM / EGL bootstrap chain expects the session.
- `vulkaninfo` should run cleanly and list your physical device.

If you are on a TTY or a plain SSH session with no display server,
`WpeProducer::new` will fail cleanly rather than panic — but the
producer is genuinely unusable in that environment.

### Build dependencies

- `pkg-config` (`dnf install pkgconf-pkg-config`).
- The `wpewebkit-2.0` pkg-config file, provided by `wpewebkit-devel`.
- `mesa-libgbm-devel` and `libdrm-devel` are needed for the dev-only
  `dmabuf_roundtrip` and `wpe_to_vulkan_roundtrip` integration tests
  (Mesa's `gbm` allocates the producer-side DMABUF those tests import).
  Not needed to build or run the producer itself.

## Building

The WPE producer is gated behind scrying's `wpe` cargo feature:

```sh
cargo build -p scrying --features wpe
```

The `wpe` feature pulls in three optional dependencies:

- `glib = "0.18"` — GObject refcount + signal mechanics.
- `soup3 = "0.5"` — `soup::Cookie` types for the cookie-store API.
- `libc = "0.2"` — POSIX fd plumbing.

It coexists cleanly with the `webkitgtk-fallback` feature (same `glib`
and `soup3` versions). It does *not* coexist with `webkit6` — both
pull incompatible gtk-rs version trees; pick one Linux producer per
build.

The runtime probe binary `demo-wpe` is a workspace member that pulls
`scrying` with the `wpe` feature already enabled:

```sh
cargo build -p demo-wpe
```

## Running

```sh
# Capability probe — no GPU required, prints WebSurfaceCapabilities and exits.
cargo run -p demo-wpe -- --probe-only

# Default — navigate to an inline HTML page, acquire one DMABUF frame,
# print plane metadata, close plane fds, exit 0. Needs GPU + Wayland.
cargo run -p demo-wpe

# Real-page snapshot.
cargo run -p demo-wpe -- --url https://example.com

# CI-shaped: exit 1 if no DMABUF frame arrives within ~10 s.
cargo run -p demo-wpe -- --snapshot-test
```

The integration test that exercises the full producer surface
(navigate + input + cookies + scripting) lives at
`scrying/tests/wpe_input.rs` and is `#[ignore]`d by default — run it
manually with:

```sh
cargo test -p scrying --features wpe --test wpe_input -- --ignored --test-threads=1
```

## Architectural constraints

These are not optional. They are baked into WebKit + WPE's process
model, not into scrying.

### Thread-affine producer

`WpeProducer` must be constructed *and* driven from the same OS
thread. The producer pumps a `glib::MainContext` synchronously (Model
A in the design docs) — sending it across threads or accessing it from
multiple threads is unsupported and will deadlock or abort.

If you need a producer accessible to a multithreaded host, run it on
its own dedicated thread and message-pass to it.

### One WPE display per process

Constructing a headless `WPEDisplay` + `WebKitWebView` initializes
process-global WebKit state. The headless module documents the
observed failure modes:

> Standing up a headless `WPEDisplay` + `WebKitWebView` initializes
> process-global WebKit state. Constructing a second producer in the
> same process — sequentially or in parallel — has been observed to
> SIGABRT (in parallel) or hang in WebKit teardown between displays
> (sequential, even under `--test-threads=1`).

Production callers must therefore either:

- Hold exactly one `WpeProducer` per process for that process's
  lifetime, sharing it across all consumers in-process, or
- Spawn a fresh subprocess for each `WpeProducer` instance.

The integration tests use `#[ignore]` + manual invocation for the
same reason; each integration target (`tests/*.rs`) is its own
binary, so its WebKit state is independent.

### DMABUF-only frame contract

The WPE producer's `WebSurfaceCapabilities` declares
`supported_frames = [DmaBufImage]` and `cpu_snapshot =
Unsupported(NativeImportNotYetImplemented)`. There is no CPU-pixel
fallback. Frames arrive as

```text
WebSurfaceFrame::Native(NativeFrame::DmaBufImage(DmaBufImage {
    size, format, drm_format, drm_modifier, planes,
    generation, producer_sync, semaphore_fd,
}))
```

The host imports the DMABUF through `WgpuTextureImporter`. Scry applies WPE's
modifier and semaphore policy, then delegates the low-level external-memory
boundary to `grafting::vulkan_dmabuf::import_dmabuf`. `demo-wpe` shows the
discipline of inspecting and releasing without importing; production
consumers hand the frame to the importer.

Construct that host with `build_dmabuf_capable_device`. It enables
`VK_EXT_queue_family_foreign` alongside the DRM-modifier and external-semaphore
extensions; the capability probe rejects an ordinary wgpu device before Graft
records a foreign-family acquire barrier.

### Plane-fd ownership

Every `DmaBufPlane.fd` and the optional `DmaBufImage.semaphore_fd`
are *owned* file descriptors. Ownership transfers with the frame the
moment `acquire_frame` returns it. `DmaBufImage` has no `Drop`
implementation — closing fds is the consumer's responsibility:

- If you use `WgpuTextureImporter`, Scry first owns the semaphore wait and
  then hands every plane fd to Graft. Graft closes all descriptors on
  validation and Vulkan error paths. On `vkAllocateMemory` success Vulkan
  owns the first fd and Graft closes the redundant same-buffer plane fds; a
  semaphore fd likewise transfers only after successful Vulkan import.
- If you do not import the frame, the consumer must `close(2)` every
  plane fd and the semaphore fd before dropping the frame. See
  `close_frame_fds_if_dmabuf` in `demo-wpe/src/main.rs` for the
  pattern.

`WpeProducer::Drop` closes the fds of any frame still in
`pending_frame` at drop time (a frame the producer received from
WebKit but the consumer never called `acquire_frame` for). That is
the producer's only fd-cleanup responsibility — once the frame is
handed out, it is the consumer's problem.

## Headless limitations

The producer is built on `wpe_display_headless_new()`, which is WPE's
offscreen path. Several features behave differently here than they
would on a hosted display.

### Toplevel resize is a no-op

WPE 2.52.5's `WPEToplevelHeadless` class has an unimplemented `resize`
vfunc. `wpe_toplevel_resize` returns `TRUE` (suggesting success), but
the underlying dimensions stay at the construction-time defaults
(1024×768). `WpeProducer::resize` calls `wpe_view_resized` after the
toplevel resize to trigger a `buffer-rendered` repaint, but the
repaint stays at the original size.

**Workaround:** pick the final size at `WpeProducer::new` (via
`WpeProducerConfig::new(size, data_dir)`) and do not resize at
runtime. The call shape exists on the producer for compatibility with
the WebKitGTK / WebKit6 producers (where it works correctly); it is
dormant on the headless path.

### Touch input hangs

End-to-end touch dispatch through `wpe_view_event` blocks indefinitely
on the headless display — the underlying path expects a
`WPEGestureController` and `WPEScreen` state that headless does not
provide (`futex_do_wait` inside dispatch). Mouse and pen input work
fine; only touch is affected.

**Workaround:** none on headless. Unit tests cover the input → WPEEvent
translation. End-to-end touch testing belongs in a non-headless target
or behind a producer-provided `wpe_view_set_gesture_controller`. The
producer's `send_pointer_input` trait impl will still build and
dispatch the touch event — it just won't return.

### Untested-on-headless surface

Some Phase 2b-2e ports have unit-test coverage for the translation
layer but no end-to-end runtime coverage on headless:

- Some cookie operations (`set_cookie_change_handler`, persistent
  storage policy).
- Scheme handlers (integration smoke deferred until cross-backend
  trait test infra covers them).
- Cursor `mouse-target-changed` (needs a real DOM hover to fire).

These are deferred to the user's manual smoke or to a non-headless
target.

## API deviations from WebKitGTK

If you are reading the `webkitgtk_producer/` or `webkit6_producer/`
source as a reference, watch for these WPE 2.0 deviations. Both are
forced by `ENABLE_2022_GLIB_API=ON` in the WPE 2.0 build.

### `download-started` moved off `WebKitWebContext`

Under the 2022 GLib API, the `download-started` signal was moved off
the WebContext (the declaration is gated on `#if
!ENABLE(2022_GLIB_API)` in `Source/WebKit/UIProcess/API/glib/WebKitWebContext.cpp`).
It lives on the per-WebView `WebKitNetworkSession` instead. The WPE
producer fetches the session via `webkit_web_view_get_network_session`
and connects the signal there.

Source reference:
`Source/WebKit/UIProcess/API/glib/WebKitNetworkSession.cpp:204`.

### `webkit_download_set_destination` wants an absolute path, not a `file://` URI

Under `ENABLE(2022_GLIB_API)` the impl is
`g_return_if_fail(g_path_is_absolute(destination))`. The older
`file://` URI form the WebKitGTK precedent uses will trip the
fail-on-not-absolute check; the WPE producer passes a plain absolute
path.

Source reference: `WebKitDownload.cpp:532`.

### `chrome.webview` shim and native handler names

The producer installs a `chrome.webview.postMessage` /
`addEventListener('message', cb)` shim on every navigation, at
document-start, in all frames — same convention as the Windows and
macOS producers. The shim forwards to two WebKit-side
`WebKitUserContentManager` script-message handlers:

- `scry` — `window.chrome.webview.postMessage` and the JS-host
  message bridge.
- `scryIme` — IME observability (focus + selection + composition
  events surfaced as `NavigationEvent::TextInputFocused / Changed /
  Blurred`).

Page code that wants to be portable across all three platforms
should use the `window.chrome.webview` surface. Page code that wants
the WPE-native escape hatch can call
`window.webkit.messageHandlers.scry.postMessage(...)` directly.

## wgpu Vulkan pixel-correctness receipt

The wgpu 30 WPE path is a hard pixel gate. Graft imports the explicit DRM
modifier, intersects the image and fd memory-type masks, acquires the image
from `VK_QUEUE_FAMILY_FOREIGN_EXT`, and registers the resulting shader-readable
`RESOURCE` state at wgpu's HAL boundary.

Headed receipt from 2026-08-31:

- Fedora 44, AMD Renoir/RADV, WPEWebKit 2.52.5, wgpu 30.0.1.
- WPE exported 1024×768 `XR24`, explicit AMD DCC modifier
  `0x020000044051ba01`, with color and auxiliary planes sharing one kernel
  DMABUF.
- The test discarded WPE's transparent startup buffer, forced the content
  repaint, sampled the imported texture through a host-owned 64×64 target,
  and required all 4,096 pixels to be within ±8 of BGRA
  `[255, 144, 30, 255]`.
- The host device explicitly enabled `VK_EXT_queue_family_foreign`; the
  requested Khronos validation layer emitted no VUID diagnostics.
- Result: 4,096/4,096 pixels passed in 0.48 seconds.

```sh
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
cargo test -p scrying --features wpe \
  --test wpe_to_vulkan_roundtrip -- \
  --ignored --nocapture --test-threads=1
```

The wgpu 28 and 29 rows remain compile-compatible for other Scry backends, but
the WPE DMABUF capability probe does not claim this content-preserving foreign
import path on those rows. Hosted Fedora CI builds the producer, demo, and hard
test binary. Execution remains a headed physical-GPU gate because hosted
containers do not expose the required render node and Wayland session.

## Troubleshooting

### `WpeProducer::new` fails

> `wpe_display_headless_new() returned null; no headless WPE display
> available`

The GPU bring-up failed. Check:

- `vulkaninfo` runs cleanly and lists your physical device.
- You are in a Wayland session (`echo $WAYLAND_DISPLAY`).
- Your user is in any required render-node groups (`ls -l
  /dev/dri/renderD128`; on Fedora the `video` group typically
  suffices).
- You are not on a TTY or plain SSH session without display
  forwarding.

X11 sessions have not been validated as a runtime target for the
headless WPE path; if you must run on X11, expect rough edges.

### `pkg-config` cannot find `wpe-webkit-2.0`

`build.rs` will print:

> wpe feature requires WPEWebKit ≥ 2.52 dev libs (dnf install
> wpewebkit-devel); pkg-config wpe-webkit-2.0 failed

Check:

- The philn COPR is enabled (`dnf copr list-enabled` shows
  `philn/wpewebkit`).
- The `wpewebkit-devel` RPM is installed (`rpm -q wpewebkit-devel`).
  If not, use the direct-URL install above — the standard `dnf
  install wpewebkit-devel` will not find the F44 build.
- `PKG_CONFIG_PATH` is not blocking discovery (`pkg-config --modversion
  wpe-webkit-2.0` should print a 2.52.x version; the current receipt is
  `2.52.5`).

### Build pulls in incompatible glib versions

If you have both `wpe` and `webkit6` features enabled, Cargo will
resolve incompatible glib version trees (0.18 vs 0.22). Pick exactly
one Linux producer per build. `wpe` + `webkitgtk-fallback` is fine
(both are pinned to glib 0.18 / soup3 0.5).

### `demo-wpe` exits with `no DMABUF frame within 10s`

The headless display started but `buffer-rendered` never fired. Most
common cause is a navigation failure — try `--url
data:text/html,<h1>hi</h1>` to remove network from the picture, and
inspect any `NavigationEvent::LoadFailed` payloads. If a real URL is
needed, ensure DNS + outbound HTTPS work from inside whatever sandbox
you are running under.

### Many parallel runs SIGABRT

You are constructing multiple `WpeProducer`s in the same process.
That's unsupported (see "One WPE display per process" above). Use a
fresh subprocess per producer instance.

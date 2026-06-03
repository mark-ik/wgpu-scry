# Phase 4c.2 retrospective — what the implementation actually taught us

**Date:** 2026-06-03
**Scope:** Findings during execution of the
[4c.2 plan](2026-05-20_phase4c2_implementation_plan.md) against the
[4c.2 spec](2026-05-20_phase4c_wpe_platform_producer.md), captured here
so 4c.3+ doesn't have to relearn them.

The implementation matched the spec; every empirical decision below was
genuinely unknowable from headers alone and only fell out of running
code against WPEWebKit 2.52.3 on Fedora 44.

## Five things the runtime taught us

### 1. The ephemeral `network-session` is mandatory, not optional

The spec called for binding `display` as a GObject construct property on
`WebKitWebView`. That works — but as written it also SIGABRTs the
process during `atexit`, deep inside
`WebKit::WebsiteDataStore::~WebsiteDataStore()` →
`WTFCrashWithInfo` → `abort`. WebKit auto-creates a *persistent*
default data store the first time anything touches one; that store's
destructor asserts during teardown.

**Fix:** pass an explicit `webkit_network_session_new_ephemeral()` as a
second construct property (`network-session`). The session is in-memory,
its destructor is well-behaved, and the abort goes away. This is now a
load-bearing line in `build_producer_view` and is documented in the
headless.rs module doc.

**Track:** when an upstream WPE/WebKit release ships a default
data-store destructor that doesn't abort, this workaround can be
revisited. Until then it stays.

### 2. WPE/WebKit cannot init+teardown more than one headless display per process

The spec mentioned this only as a vague risk. The runtime made it
concrete: with three `#[ignore]`d unit tests each constructing a
producer, `cargo test --features wpe -- --ignored`:

- **Multi-thread (default):** **SIGABRT (signal 6)** before any test
  output landed — parallel GObject construction across test threads
  collided.
- **Single-thread (`--test-threads=1`):** **hung in `futex_wait`** with
  the `WPENetworkProcess` subprocess still alive, never proceeding past
  the first test — WebKit's teardown of one display blocks the next
  construction.

So: **at most one ignored runtime-WPE test per `cargo test` invocation
in this crate's unit-test binary.** End-to-end coverage that needs a
second WebKit init goes into a separate `tests/*.rs` integration target
(each is its own binary → independent process → independent WebKit
state). This is documented in
`scrying/src/wpe_producer/headless.rs`'s module doc; 4c.3 honors it.

### 3. glib stayed at 0.18 — the modern version would have torn the build apart

The plan asked for `glib = "0.22"` to match the modern stack. Cargo
rejects two entries for the same crate name in one dependency table,
and the existing `webkitgtk-fallback` feature already pulls
`glib = "0.18"`. There is no clean way to have both webkitgtk-fallback
and wpe coexist with different glib majors short of a rename hack.

The 4c.2 plan explicitly allowed reusing the existing entry; that's
what we did. The crate version is the Rust binding surface, not the
underlying libglib ABI, so glib 0.18 binds the same `libglib-2.0` and
everything works. The high-level glib API differences between 0.18 and
0.22 (e.g. `connect_closure` / `closure_local!`) matter only for the
seam code; the differences are small and the 0.18 names are
established.

**Carry forward:** if anyone bumps `webkit2gtk`/`gtk` to a glib-0.22-era
line in the future, bump WPE's glib at the same time.

### 4. The `buffer-rendered` signal is real — and `connect_closure` panics if it isn't

The WPEPlatform headers expose `wpe_view_buffer_rendered` only as a
*method* (the emit function), not as a `signal` keyword anywhere. We
were prepared to need GObject signal introspection to discover the real
frame-seam name. We did the introspection (`g_signal_list_ids` on the
live view type, walking the parent chain) and confirmed:

```
[WPEView] signal: closed
[WPEView] signal: resized
[WPEView] signal: buffers-changed
[WPEView] signal: buffer-rendered      ← the seam
[WPEView] signal: buffer-released
[WPEView] signal: event
[WPEView] signal: toplevel-state-changed
[WPEView] signal: preferred-buffer-formats-changed
```

`connect_closure("buffer-rendered", ...)` connects cleanly with no
`GLib-CRITICAL`. **And: `connect_closure` panics on a missing signal**,
so a passing connect-and-render test is a strong positive (not a
"connect silently no-op'd" trap).

### 5. AMD's WPE output is XR24 + DCC metadata, two planes

Smoke output observed on a Renoir/Mesa box:

```
smoke: 1024x768 fourcc=0x34325258 mod=0x020000044051ba01 planes=2
```

- `0x34325258` = `XR24` = `DRM_FORMAT_XRGB8888` — byte-equivalent to
  BGRA for opaque content. `wgpu::TextureFormat::Bgra8UnormSrgb` maps
  it correctly with no change.
- `0x020000044051ba01` — vendor=AMD DRM modifier; concretely an
  AMD-tiled DCC-compressed layout. The 2 planes are: plane 0 the actual
  color data, plane 1 the DCC compression metadata.

**Implications for Phase 4a importer integration:** today the importer
reads `planes[0]` only. For AMD DCC-tiled exports it needs to either
(a) consume both planes via Vulkan's
`VK_EXT_image_drm_format_modifier` multi-plane import path or (b)
import the modifier as-is and let the driver decompress on read. The
seam already captures all planes in `DmaBufImage`, so the change lives
entirely in the importer when it's ready.

**Carry forward:** ignoring plane 1 may not produce visually-correct
imports on AMD; either implement the multi-plane import path or accept
the visual artifact until then. The producer side is done.

## The frame seam: how an fd lives and dies

Walking it end-to-end, since this was the design's central concern and
the cross-cutting review confirmed it holds together:

```
WPE renders → buffer-rendered fires
            ↓
   dmabuf_to_image(): libc::dup(plane.fd) for each plane
                      → DmaBufImage owns dup'd fds
            ↓
   wpe_view_buffer_released(view, buffer)
        — hand WPE's buffer back IMMEDIATELY so its pool isn't
          starved by consumer latency
            ↓
   FrameSink::submit(image)
        - if slot already had a frame → close_frame_fds(old) first
        - then *slot = Some(image)
            ↓
   acquire_frame() takes from slot → caller (importer) owns fds
        OR
   FrameSink::submit() with newer frame → evict + close_frame_fds(old)
        OR
   WpeProducer::drop() → close_frame_fds(any frame still in slot)
```

Every fd's life ends in **exactly one** of three places. `close_frame_fds`
is the single closure site. Verified with real pipe-fd unit tests
(`fcntl(F_GETFD)` observability) without needing WPE.

## GObject ref accounting

Every transfer-full ref the constructors hand us is released exactly
once:

- `wpe_display_headless_new()` → transfer-full → consumed by
  `g_object_new` as the `display` construct property, which takes its
  own ref → we `g_object_unref` our original.
- `webkit_network_session_new_ephemeral()` → same shape, same release.
- `g_object_new` returns a transfer-full WebView → `from_glib_full`
  adopts it → `glib::Object`'s Drop unrefs on producer drop. The
  WebView's internal refs on display + session keep both alive for its
  lifetime.
- Error paths (display null, `g_object_new` null, binding guard fail,
  view null) each release exactly the refs they hold at that point —
  no leaks, no double-frees. Verified by the cross-cutting review.

## What's still on the table

These are explicit non-goals for 4c.2; they live in 4c.3+ or the
importer-integration step:

- **Navigation API.** `navigate_to_string` still returns `Unsupported`.
  `load_html_for_smoke` + `pump_until` are the right primitives, ready
  to promote.
- **Resize.** Producer field updates; the WPEView itself isn't resized
  yet. The runtime gave us 1024×768 instead of the requested 256×256.
- **End-to-end importer round-trip.** The smoke closes its own fds
  rather than handing them to `native_frame::dmabuf::import`. A
  separate `tests/wpe_to_vulkan_roundtrip.rs` integration target
  (independent WPE init) would close the actual Phase-4a loop.
- **Multi-plane / non-XR24 fourcc map.** Currently hard-coded
  BGRA-equivalent.
- **Explicit producer-sync semaphore.** WPE 2.52.3 exposes no
  `WPEBufferDMABuf` fence getter; `SyncMechanism::None`. Additive in
  Phase 4a's existing
  `VK_KHR_external_semaphore_fd` path if a future WPE release surfaces
  one.

## A note on process

This phase ran the
[brainstorming → spec → plan → subagent-driven-development → review →
finishing](https://docs.anthropic.com/superpowers) cycle end-to-end.
Two empirical "spike" tasks (the display binding in Task 2 and the
signal name in Task 4) were structured as "the test is the oracle" —
implement the most-likely call, run, iterate. Both resolved on the
first attempt, which is encouraging evidence that header reading +
prior-art pattern matching is good enough to converge most GObject
runtime questions in one shot. The two-stage review caught one real
issue (the `g_object_new`-null error-path ref leak), and the
cross-cutting final review caught one architectural test-suite issue
(the one-per-process constraint surfaced as a hung run). Both are
exactly the class of bug per-task review should catch and per-task
review missed in isolation.

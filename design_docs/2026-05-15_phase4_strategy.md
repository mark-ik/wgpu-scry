# Phase 4 strategy — Vulkan DMABUF import + WPE producer

**Date:** 2026-05-15
**Status:** 4a + 4a.x + 4b.1 + 4c.1 + 4c.2 + 4c.3 + 4c.4 + 4c.4.1-3 + (β) + 4c.5.a + 4c.5.b + 4c.5.c shipped; 4c.5.d-f, 4c.6+ in flight.

This doc captures the plan for the Linux producer's only remaining
structural row in the [parity matrix](2026-05-07_platform_ceilings.md#cross-platform-parity-matrix):
`ImportedTexture` (the GPU-handoff frame contract). It supersedes the
single-paragraph "Phase 4" notes in earlier docs.

## Context

After Phases 2 + 5, scrying ships three working Linux backends:

- **WebKitGTK 4.1** (production-shaped, 12 runtime smokes green)
- **WebKitGTK 6.0** (Phase 5 first slice — navigate + snapshot)
- **WPE** (still a scaffold)

All three deliver at the `CpuRgba` tier. The parity-matrix
`ImportedTexture` row is `—` for the WebKit-family producers (we
chose offscreen + snapshot, no native composition path) and `?` for
WPE (always intended as the GPU-handoff target).

The strategic question isn't "make WPE work." It's "**bring native
DMABUF → wgpu import into scrying as a reusable capability**,
because every plausible Linux GPU-handoff path produces DMABUFs."
WPE is the most immediate consumer; WebKitGTK 6.0's accelerated-
compositing DMABUF renderer (2.46+) is the second; wlroots
`zwlr_screencopy_manager_v1` is the third.

## The three sub-phases

Phase 4 splits into three sub-phases that can ship **independently**
of each other:

| Sub-phase | What ships | Blocks on |
| --- | --- | --- |
| **4a — Vulkan DMABUF import** | [`import_dmabuf_image`](../scrying/src/native_frame/mod.rs) implementation; wgpu-side export/round-trip test | nothing |
| **4b — `wpe-sys` + `wpe-webkit-sys`** | Two new `gir`-generated FFI crates published to crates.io | nothing — pure bindings work |
| **4c — `wpe_producer` real implementation** | The producer wired to 4a + 4b; runtime-verified end-to-end | 4a + 4b + working WPE install |

This ordering is deliberate. Sub-phase 4a is the highest-leverage
piece — it unlocks `ImportedTexture` for **every** future Linux
DMABUF source, not just WPE. 4b is foundational ecosystem work. 4c
is the final assembly.

---

## Sub-phase 4a — Vulkan DMABUF import

### Goal

Implement [`native_frame::import_dmabuf_image`](../scrying/src/native_frame/mod.rs#L312)
so a [`DmaBufImage`](../scrying/src/native_frame/mod.rs#L196) lands
as a `wgpu::Texture` ready for sampling, with optional
`VkSemaphore`-based ordering against the producer.

### API contract

Inputs (per the existing [`DmaBufImage`](../scrying/src/native_frame/mod.rs#L196)):

```rust
DmaBufImage {
    size: PhysicalSize<u32>,
    format: wgpu::TextureFormat,
    drm_format: u32,        // DRM_FORMAT_* fourcc
    drm_modifier: u64,      // DRM_FORMAT_MOD_* (INVALID == implicit modifier)
    planes: Vec<DmaBufPlane>,   // (fd, offset, stride) per plane
    generation: u64,
    producer_sync: SyncMechanism,  // None | ExplicitExternalSemaphore
    semaphore_fd: Option<i32>,     // opaque-fd-imported VkSemaphore
}
```

Output: `ImportedTexture` with a `wgpu::Texture` whose backing
`VkImage` aliases the DMABUF memory.

### Implementation outline

Through wgpu-hal's Vulkan escape hatch (`device.as_hal::<Vulkan>()`):

1. **Confirm host backend is Vulkan.** Return `BackendMismatch`
   otherwise.
2. **Build `VkImage` with `VkImageDrmFormatModifierExplicitCreateInfoEXT`**
   (or `*ListCreateInfoEXT` for `DRM_FORMAT_MOD_INVALID`) and
   `VkExternalMemoryImageCreateInfo`. Tiling
   `DRM_FORMAT_MODIFIER_EXT`, sharing mode `EXCLUSIVE`, usage
   `SAMPLED | TRANSFER_SRC`.
3. **Allocate `VkDeviceMemory` via `VK_KHR_external_memory_fd`**
   with `VkImportMemoryFdInfoKHR { handle_type: DMA_BUF_EXT, fd }`.
   Memory-type index from the image's memory requirements
   intersected with `VkMemoryFdPropertiesKHR`.
4. **Bind memory to image.**
5. **Wrap as `wgpu::Texture` via `Device::create_texture_from_hal::<Vulkan>`.**
6. **If `semaphore_fd.is_some()`**: import as `VkSemaphore` via
   `VK_KHR_external_semaphore_fd` (handle type `OPAQUE_FD`).
   `ExplicitExternalSemaphoreSynchronizer` waits on it before the
   first consumer submit.

### Required Vulkan extensions

The host wgpu device must have these enabled for the import to work:

- `VK_KHR_external_memory_fd` (mandatory)
- `VK_EXT_image_drm_format_modifier` (mandatory)
- `VK_KHR_external_semaphore_fd` (mandatory for explicit fence path)
- `VK_KHR_external_memory` (transitive)
- `VK_KHR_external_semaphore` (transitive)

Most modern Mesa + AMD/Intel iGPUs support all three. We probe at
`HostWgpuContext::new` time and report unsupported via the
capability struct if any are missing.

### Format mapping

DRM fourcc ↔ Vulkan format ↔ wgpu format table (initial scope —
single-plane, RGBA/BGRA only):

| DRM fourcc | VkFormat | wgpu::TextureFormat | Notes |
| --- | --- | --- | --- |
| `DRM_FORMAT_ABGR8888` | `R8G8B8A8_UNORM` | `Rgba8Unorm` | WPE typical |
| `DRM_FORMAT_ARGB8888` | `B8G8R8A8_UNORM` | `Bgra8Unorm` | WebKitGTK AC typical |
| `DRM_FORMAT_XBGR8888` | `R8G8B8A8_UNORM` | `Rgba8Unorm` | strip alpha |
| `DRM_FORMAT_XRGB8888` | `B8G8R8A8_UNORM` | `Bgra8Unorm` | strip alpha |

Multi-plane formats (NV12, YUV420, P010) are deferred — only
needed if a video-decoded WebKit page produces those, which is
rare for normal embedding.

### Testing

Hard to runtime-verify without a real producer producing DMABUFs.
Two approaches that don't need WPE:

- **wgpu round-trip**: render a known pattern into a wgpu texture,
  export as DMABUF via `vkGetMemoryFdKHR`, re-import via our new
  path, sample, assert pixel identity. Tests both halves of the
  external-memory protocol against scrying's own pixels.
- **libgbm-allocated DMABUF**: use Mesa's `gbm` to create a
  `gbm_bo`, fill with a known pattern via `mmap`, export fd,
  import via our path, sample, assert. Tests against a
  third-party-produced DMABUF, closer to real WebKit behaviour.

Both are useful; the first is cheaper to write because we already
have the wgpu device.

---

## Sub-phase 4b — Rust bindings ecosystem

### The gap

crates.io has no WPE WebKit bindings today:

- `wpe = "0.0.19"` is unrelated (WP Engine hosting CLI)
- `wpe-sys`, `wpe-webkit`, `wpe-webkit-sys`: don't exist
- gtk-rs publishes `webkit6` (GTK 4 WebKit) but not WPE

The Tauri community has asked for WPE bindings repeatedly; no one
has shipped them. This is a real, fillable ecosystem gap.

### Approach: `gir`-generated, following gtk-rs conventions

Upstream WPE ships GIR files (`WPEBackend-fdo` + `WPEWebKit-1.0` /
`WPEWebKit-2.0`). gtk-rs's `gir` tool already does this codegen for
the GTK / WebKit family. The work is:

1. Fork the `gtk-rs/gir-files` repo, add WPE's GIR files
2. Configure `Gir.toml` for `wpe` + `wpe-webkit` crates
3. Run `gir` → get `wpe-sys` + `wpe-webkit-sys` (FFI) + safe
   `wpe-webkit` wrapper crate
4. Hand-write the few manual extensions (signal connectors, IsA
   chains for newtypes that GIR doesn't capture)
5. Publish

Estimated effort: a focused weekend if upstream GIR is clean and
gtk-rs's `gir` handles WPE without changes; up to two weeks if
either has rough edges.

### Where the bindings live

**Not in scrying.** These should be standalone crates published by
some Rust-WPE-shaped project (gtk-rs ecosystem, a new repo we
maintain, or upstream Tauri/wry).

Until they exist, `wpe_producer` keeps its own inline FFI in-tree
for the API surface it actually needs — pragmatic and avoids
blocking 4c on a parallel publishing effort.

### Strategic note

Publishing `wpe-sys` / `wpe-webkit` would be a real contribution to
the Rust + Linux + embedded space. If the work is funded by
scrying's needs anyway, the ecosystem benefit is "free."

---

## Sub-phase 4c — `wpe_producer` real implementation

### Goal

Wire the existing
[`wpe_producer`](../scrying/src/wpe_producer.rs) scaffold to a
working WPEWebKit instance, with `WPEViewBackendDMABuf` exporting
DMABUF fds + `VkSemaphore` opaque fds that flow through sub-phase
4a's import path.

### Build prerequisites

- libwpe, WPEBackend-fdo, WPEWebKit runtime libraries
- Either: a working `wpe-sys` / `wpe-webkit-sys` (sub-phase 4b), or
  inline FFI for the symbols this producer uses
- A way to run WPE on this Fedora box (see below)

### Getting WPE on the developer machine

Fedora 44 doesn't ship WPE. Three workable paths:

- **Flatpak SDK** — `flatpak install flathub org.webkit.WPEWebKit.Sdk`.
  Run cargo development inside the SDK runtime. Awkward but works.
- **COPR** — none known today, but `dnf copr search wpe` or
  `dnf copr search webkit` is worth trying. If a maintained COPR
  exists, it's the cleanest path.
- **Source build** — `git clone https://github.com/WebKit/WebKit`,
  `Tools/Scripts/update-webkit-wpe-libs && Tools/Scripts/build-webkit --wpe`.
  ~10 GB source, 30–60 minutes compile on this ThinkPad.

For Phase 4c we'll likely pick **Flatpak SDK** because it's
reproducible and matches the consumer distribution story (below).

### Producer architecture

The existing scaffold is roughly right — what fills in:

- **WPEView + WPEViewBackendDMABuf construction**: call libwpe via
  `wpe_view_backend_create_with_dmabuf`. Backend exports DMABUF fds
  + DRM format/modifier + an optional `VkSemaphore` opaque fd per
  frame via the EGL / Vulkan interop protocol.
- **WPEWebKit WebKitWebView**: `webkit_web_view_new_with_view_backend`
  attaches our backend.
- **Frame callback**: when `WPEViewBackendDMABuf` exports a frame,
  build a [`DmaBufImage`](../scrying/src/native_frame/mod.rs#L196)
  and call `enqueue_dmabuf_frame` (already present in the scaffold).
- **Input forwarding**: `wpe_view_backend_dispatch_*_event` — clean
  C API, no GdkEvent equivalents needed. Each `MouseInput` /
  `KeyboardInput` translates directly.
- **Same Phase 2b/2c/2d/2e surface**: navigation events, settings,
  cookies, URL schemes, JS messaging — same WebKit signal names
  and shapes as WebKitGTK; mostly copy-paste from `webkitgtk_producer/`.

### Capabilities

When 4c lands, the parity matrix WPE column upgrades:

- Imported GPU texture per frame: ✅ (Vulkan + DMABUF + VkSemaphore)
- Pre-composition extraction: ✅ (only platform — already noted in
  ceilings doc)
- Cross-API GPU sync: ✅ (`VkSemaphore`, explicit, standards-correct)

This is the **strategically strongest** Linux backend; the producer
contract is what every other Linux WebKit-family integration
*wishes* it had.

---

## Consumer distribution story

scrying's job is to be the integration layer that knows how to
talk to WPE when it's present. **Not to ship WPE itself.**
Distribution is the consumer's responsibility — and the realistic
paths for `mere`-shaped downstream apps:

| Path | Suitable for | Friction |
| --- | --- | --- |
| **Flatpak with `org.webkit.WPEWebKit.Sdk`** | Desktop Linux apps | Low — pre-built runtime, declarative manifest, works on every distro |
| **AppImage bundling WPE libs** | Single-file desktop binaries | Medium — manual lib bundling, big binary |
| **Snap with WPE base** | Ubuntu-flavoured deployments | Medium |
| **Yocto / Buildroot custom image** | Embedded systems | Higher but their normal flow — WPE's native target |
| **Source build by developers** | Contributor onboarding only | Highest |

For desktop apps on stock Linux distros, **Flatpak with the WPE
SDK is the clear recommendation** — it's how GNOME's own WebKit-
using apps distribute. We'd document a sample manifest in
`docs/wpe-deployment.md` alongside Phase 4c.

The producer makes no assumption about *how* WPE got installed —
it just needs the runtime libraries discoverable via `pkg-config`
or the equivalent dlopen path inside the consumer's deployment
artifact.

---

## Open questions

- **Vulkan extension probe surfacing**: today's `HostWgpuContext::new`
  doesn't gate `ImportedTexture` on the required VK extensions. We
  need to either probe at construction and downgrade capabilities,
  or fail at first `import_dmabuf_image` with a clear error. Pick
  one before sub-phase 4a ships.
- **Single-plane only vs multi-plane**: WebKit's DMABUF output is
  effectively always single-plane BGRA/RGBA. Defer multi-plane
  (NV12 / P010) until a real consumer needs it.
- **Implicit-modifier (`DRM_FORMAT_MOD_INVALID`) support**: needs
  `VkImageDrmFormatModifierListCreateInfoEXT` instead of
  `*Explicit*`. Slightly different code path. Land both, gate on
  the producer's reported modifier.
- **Where do `wpe-sys` / `wpe-webkit` actually live?** A new
  github.com/mark-ik repo? Contribute upstream to gtk-rs? Get
  Tauri's wry team to maintain? Decision needed before sub-phase
  4b kicks off — but doesn't block 4a.

## Phase 4 deliverables checklist

- [x] **4a.1** `import_dmabuf_image` implementation — single-plane,
      explicit modifier, no semaphore
- [x] **4a.2** `VK_KHR_external_semaphore_fd` import path (wait-only
      `vkQueueSubmit` on the consumer queue; runtime-exercised by 4a.6)
- [x] **4a.3** Capability probe — `probe_dmabuf_extensions` reports
      required VK extensions, downgrades `imported_texture` if any
      are missing
- [x] **4a.4** Round-trip test: libgbm-produced DMABUF → import →
      readback → pixel verify (65536/65536)
- [x] **4a.5** Implicit-modifier (`DRM_FORMAT_MOD_INVALID`) support —
      substitute `DRM_FORMAT_MOD_LINEAR`; multi-plane (YUV/ycbcr)
      deferred
- [x] **4a.6** Signaled-semaphore round-trip exercising the 4a.2 wait
      path end-to-end
- [x] **4a.7** `build_dmabuf_capable_device` helper — enables
      `VK_EXT_image_drm_format_modifier` + `VK_KHR_external_semaphore_fd`
      at device creation (validated under `VK_LAYER_KHRONOS_validation`)
- [x] **4a.x** Multi-plane shared-fd DMABUF import (DCC-compressed RGBA)
      + foreign-queue acquire barrier — non-YUV multi-plane: planes
      share one kernel DMABUF, communicated via N-entry plane_layouts.
      Adds a transient acquire barrier from `VK_QUEUE_FAMILY_FOREIGN_EXT`
      between bind and texture wrap (Vulkan-spec discipline for taking
      ownership of producer-written DMABUFs). Spec
      [`2026-06-04_phase4a_x_multiplane_dcc_import.md`](2026-06-04_phase4a_x_multiplane_dcc_import.md),
      plan [`2026-06-04_phase4a_x_multiplane_dcc_plan.md`](2026-06-04_phase4a_x_multiplane_dcc_plan.md).
      Import shape (size + format) verified on real WPE-on-AMD; pixel
      correctness BLOCKED on an upstream wgpu API gap —
      `create_texture_from_hal` (wgpu 29.0.3) tracks every external
      texture as `UNINITIALIZED → vk::ImageLayout::UNDEFINED`, so wgpu's
      first-use barrier transitions from UNDEFINED regardless of what
      we left the image in, and Vulkan's spec allows that transition to
      discard contents. RADV enforces strictly; gbm-linear escapes
      because linear transitions are no-ops on most drivers. macOS Metal
      and Windows D3D12 sidestep this entirely (their resource models
      preserve contents on import). Fix is a wgpu `texture_from_raw`
      initial-state parameter, upstream. Round-trip test logs observed
      BGRA as a diagnostic; flips to assertion mode when wgpu lands the
      API. Disjoint-fd multi-plane / YUV ycbcr stay deferred.
- [x] **4b.1** Decide where the WPE bindings crates live →
      [`2026-05-20_phase4b_wpe_bindings_decision.md`](2026-05-20_phase4b_wpe_bindings_decision.md):
      inline in-tree FFI now, dedicated `wpe-rs` repo later
- [ ] **4b.2** ~~`wpe-sys` GIR-generated~~ — superseded: libwpe is
      plain C, bound via inline FFI (see 4b decision doc)
- [ ] **4b.3** `wpe-webkit-sys` + safe `wpe-webkit` published (gir;
      Gir.toml sketched in 4b decision doc; blocked on a WPE install)
- [x] **4c.1** Working WPE install on the dev machine — philn COPR,
      WPEWebKit 2.52.3 + libwpe 1.16.2 + wpebackend-fdo 1.16.1; pivot
      to WPEPlatform headless captured in
      [`2026-05-20_phase4b_wpe_bindings_decision.md`](2026-05-20_phase4b_wpe_bindings_decision.md)
- [x] **4c.2** `wpe_producer` frame seam wired on WPEPlatform headless
      (`buffer-rendered` signal → `WPEBufferDMABuf` → `DmaBufImage` →
      `FrameSink`); spec
      [`2026-05-20_phase4c_wpe_platform_producer.md`](2026-05-20_phase4c_wpe_platform_producer.md),
      plan [`2026-05-20_phase4c2_implementation_plan.md`](2026-05-20_phase4c2_implementation_plan.md),
      retrospective [`2026-06-03_phase4c2_retrospective.md`](2026-06-03_phase4c2_retrospective.md).
      Smoke renders a real `DmaBufImage` (1024×768 XR24 on AMD-tiled
      modifier); explicit-sync stays dormant (no fence getter yet).
- [x] **4c.3** Producer navigation (`navigate_to_string`,
      `navigate_to_url`, `poll_navigation_event`) + resize via
      `WPEToplevel`; spec
      [`2026-06-03_phase4c3_navigation_resize.md`](2026-06-03_phase4c3_navigation_resize.md),
      plan [`2026-06-03_phase4c3_implementation_plan.md`](2026-06-03_phase4c3_implementation_plan.md).
      Empirical findings: (a) `WebKitLoadEvent` arrives in glib closures as
      the GObject enum type, not gint — required a hand-rolled
      `RustClosure::new_local` over `&[glib::Value]` extracting via
      `.get::<i32>()`. (b) WPE auto-paints on resize (no renavigate
      fallback needed). (c) The headless toplevel silently coerces all
      dimensions to 1024×768. Follow-up investigation (commit `b22ad55`)
      proved this is a WPE 2.52.3 headless-platform limitation: the
      `WPEToplevelHeadless` class's `resize` vfunc is unimplemented, so
      `wpe_toplevel_resize` returns TRUE but the underlying dimensions
      don't change. Calling `wpe_view_resized` afterward DOES trigger a
      buffer-rendered repaint (so the producer is responsive to the
      attempt), but the repaint stays at the default size. Honoring
      requested dimensions on the headless display requires either
      subclassing `WPEToplevel` ourselves (substantial GObject work) or
      waiting on a non-headless WPE producer for hosted use cases. The
      WebKitGTK / WebKit6 producers don't have this constraint; the
      call-shape `WpeProducer::resize` now ships (toplevel resize → view
      notify) is correct for them and dormant on headless.
- [x] **4c.4** Input forwarding MVP — keyboard + mouse-pointer +
      scroll via `wpe_view_event(WPEEvent*)`. Real
      `send_keyboard_input` / `send_mouse_input` / `send_pointer_input`
      trait impls under `--features wpe`; pure-Rust unit tests cover
      the scrying-input → WPEEvent translation; runtime integration
      smoke `tests/wpe_input.rs` (independent binary) verifies dispatch
      doesn't crash. Spec
      [`2026-06-04_phase4c4_input_mvp.md`](2026-06-04_phase4c4_input_mvp.md),
      plan [`2026-06-04_phase4c4_implementation_plan.md`](2026-06-04_phase4c4_implementation_plan.md).
      MVP punts keyval derivation (uses 0; WebKit derives from
      keycode), event timestamps (always 0), touch input, drag input,
      and IME composition to 4c.4.x sub-phases or 4c.5. Empirical
      finding: the first runtime smoke surfaced a real FFI-signature
      bug — the four `wpe_event_*_new` constructors all take a
      `WPEInputSource` as their third arg with a specific param order
      that disagreed with the plan's signatures. Fixed; the smoke now
      runs without any GLib CRITICAL.
- [x] **4c.4.1** Touch input via `wpe_event_touch_new` + sequence-id
      mapping (PointerInput.device == Touch). `dispatch_pointer` now
      builds a `WPE_EVENT_TOUCH_{DOWN,UP,MOVE,CANCEL}` event from the
      scrying `PointerEventKind` and dispatches via `wpe_view_event`,
      with `sequence_id = pointer_id` for simultaneous touches. Unit
      test `touch_kind_translation` covers the kind→type mapping
      (including `Leave`/`CaptureChanged` → `TOUCH_CANCEL`, the
      closest semantic match). **Empirical headless caveat:** on WPE
      2.52.3 headless the touch path through `wpe_view_event` blocks
      indefinitely (`futex_do_wait` inside dispatch; the headless
      display doesn't provide the `WPEGestureController`/`WPEScreen`
      state WPE's touch path needs to complete synchronously). Same
      class of headless-platform limitation as (β) resize. Mouse + pen
      paths unaffected; touch end-to-end belongs in a non-headless
      target or behind a producer-provided `wpe_view_set_gesture_controller`.
- [x] **4c.4.2** Drag input — investigated and resolved as
      "intentionally Unsupported." Drag-from-host into a webview is a
      *capture-mode-wide limitation* shared by macOS (which documents
      it precisely: `NSDraggingInfo` synthesis requires SPI;
      overlay-mode works via AppKit's responder chain without
      producer involvement) and Windows (whose webview2 producer
      similarly leaves the trait default in place). WPE has no
      overlay mode at all (the producer is purely offscreen/headless),
      so the gap is structural. The producer trait keeps
      `send_drag_input` available for a future host that injects
      HTML5 drag/drop DOM events through the JS message bridge (a 4c.5
      surface), but no producer needs to do anything beyond that. The
      WPE producer's `impl WebSurfaceProducer` carries a guardrail
      comment in `producer.rs` documenting this stance.
- [x] **4c.4.3** IME composition — investigated and resolved as
      "absorbed into 4c.5." scrying's IME design across all backends
      is **JS-side observability**, not native `WebKitInputMethodContext`
      plumbing. WebKitGTK's `ime.rs` (156 lines) installs a `scryIme`
      `UserContentManager` script-message handler + a user script
      watching `focusin`/`focusout`/`input`/`selectionchange`, then
      surfaces results as `NavigationEvent::TextInput{Focused,Changed,Blurred}`
      payloads for the host's IM widget to consume (winit's
      `set_ime_cursor_area`, etc.). The producer trait has no
      `send_ime_input` — IME flows back through the nav-event queue.
      WPE's WebKitWebView exposes the same primitives (`UserContentManager`,
      script-message handlers) — the implementation lands when 4c.5's
      script-message bridge does. Not a standalone WPE-only sub-phase.
- [ ] **4c.5** Phase 2b–2e surface ported from
      `webkitgtk_producer/` (cookies, schemes, popups, downloads,
      cursor, IME state). Decomposed into 4c.5.a-f below — each
      independent except `e` (IME) depending on `a` (script-message).
  - [x] **4c.5.a** Script-message bridge — `webkit_user_content_manager`
        + `script-message-received::scry` signal + `chrome.webview`
        shim injection at document-start + `WpeProducer` queue +
        `post_web_message` / `poll_web_message` trait method impls +
        inherent `wait_for_web_message(timeout)`. Spec
        [`2026-06-05_phase4c5a_script_message.md`](2026-06-05_phase4c5a_script_message.md).
        End-to-end verified: `tests/wpe_input.rs` round-trips
        `window.chrome.webview.postMessage('hi from page')` → host's
        `wait_for_web_message` returning `Some("hi from page")` on a
        live headless WebKit. Empirical finding: `JSCValue` isn't a
        registered `glib::ValueType`, so the signal closure went
        directly to the `RustClosure::new_local` over `&[glib::Value]`
        pattern (same shape as `navigation.rs`'s load-changed handler);
        extracts the string via `jsc_value_to_string` + `g_free`.
  - [x] **4c.5.b** Cookies — port of `webkitgtk_producer/cookies.rs`
        landed in `scrying/src/wpe_producer/cookies.rs`. Inherent
        `request_cookies_for_url` / `set_cookie` / `delete_cookie` on
        `WpeProducer`; cookie manager borrowed transfer-none on demand
        off the WebView via
        `webkit_web_view_get_network_session ->
        webkit_network_session_get_cookie_manager`, so no cookie state
        lives on `WpeProducer` itself. Three trampolines bridge the
        GAsync APIs into sync calls by pumping the producer's
        `MainContext` (`headless::pump_until`) until an
        `Rc<RefCell<Option<...>>>` cell fills. Translators
        (`soup_to_scry`, `scry_to_soup`) verbatim from the GTK
        precedent. Empirical findings: (1) `soup3 = "0.5"` pin
        coexists cleanly with the existing `webkitgtk-fallback`
        transitive — same version, no resolver split. (2) All eight
        FFI symbols exist on WPEWebKit 2.52.3 (verified in
        `/usr/include/wpe-webkit-2.0/wpe/WebKit{WebView,
        NetworkSession,CookieManager}.h`); no fallback to a
        construction-time session ref needed. Smoke is the cookie
        round-trip extension to `tests/wpe_input.rs` (set + get +
        assert + delete) — gated by the `#[ignore]` discipline. Out:
        `set_cookie_change_handler` (separate signal-wiring chunk),
        cookie persistence policy.
  - [x] **4c.5.c** Scheme handlers — port `scheme_handler.rs`.
        Shipped. WPE side mirrors the GTK precedent: `register_all`
        registers each `(scheme, handler)` pair against a
        `WebKitWebContext` before the WebView is built; per-request
        trampoline builds a `WebKitURISchemeResponse` backed by a
        `glib::Bytes`-owned `GMemoryInputStream` and a soup3
        `MessageHeaders` block. Empirical findings: (1) WPE 2.52.3
        `WebKitWebView` accepts `"web-context"` as a construct property
        (same string the gtk-rs `WebView::with_context` builder uses,
        verified in webkit2gtk-2.0.2's auto/web_view.rs), so the
        existing `g_object_new`-based construction in
        `build_producer_view` extends cleanly — just one more
        `display`/`network-session`/`web-context` triple plus matching
        post-construction unrefs. (2) `webkit_web_context_register_uri_scheme`
        on WPE takes a `GDestroyNotify` 4th arg identical to GTK; the
        per-scheme `HandlerPayload` box lives until the WebContext is
        finalized (which happens when the producer's WebView drops),
        so no explicit teardown on `WpeHandles` is needed. (3) Kept
        the wpe feature lean by hand-rolling FFI for
        `g_memory_input_stream_new_from_bytes` rather than pulling
        `gio`; soup3's `MessageHeaders` is already available through
        the existing dep so headers go through that. Out: integration
        smoke (the cross-backend trait test infra doesn't yet cover
        scheme handlers, and per the brief we defer until it does).
  - [ ] **4c.5.d** Cursor — port `cursor.rs`.
  - [ ] **4c.5.e** IME observability — install `scryIme` handler +
        DOM focusin/focusout/input watcher script + `TextInput*`
        nav events. Depends on `4c.5.a`'s bridge.
  - [ ] **4c.5.f** Downloads — port `downloads.rs`.
- [ ] **4c.6** `demo-wpe` runtime probe — mirrors demo-linux
- [ ] **4c.7** `docs/wpe-deployment.md` — Flatpak SDK manifest
      walkthrough
- [ ] **4c.8** Parity matrix + README updates

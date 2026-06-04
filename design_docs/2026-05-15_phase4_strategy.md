# Phase 4 strategy — Vulkan DMABUF import + WPE producer

**Date:** 2026-05-15
**Status:** 4a + 4b.1 + 4c.1 + 4c.2 + 4c.3 shipped; 4c.4+ in flight.

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
      dimensions to 1024×768 — `wpe_toplevel_resize` returns TRUE but
      rendered size doesn't change. `resize()` is currently a shape
      no-op on headless; honoring requested size needs additional WPE
      API (likely `wpe_view_resized` or toplevel-state changes), tracked
      for 4c.4+.
- [ ] **4c.4** Input forwarding via `wpe_view_event(WPEEvent*)` —
      keyboard / pointer / scroll / touch / IME. WPEPlatform path, not
      legacy libwpe.
- [ ] **4c.5** Phase 2b–2e surface ported from
      `webkitgtk_producer/` (cookies, schemes, popups, downloads,
      cursor, IME state).
- [ ] **4c.6** `demo-wpe` runtime probe — mirrors demo-linux
- [ ] **4c.7** `docs/wpe-deployment.md` — Flatpak SDK manifest
      walkthrough
- [ ] **4c.8** Parity matrix + README updates

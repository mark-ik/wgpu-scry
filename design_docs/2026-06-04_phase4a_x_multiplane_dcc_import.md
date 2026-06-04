# Phase 4a.x — Multi-plane DRM-modifier DMABUF import (shared-fd / DCC)

Closes the round-trip Outcome B that's been a known-failing `#[ignore]`d
test since the 4c.x integration smoke landed. Extends Phase 4a's
`dmabuf::import` to handle the non-YUV multi-plane case where all plane
fds reference the same kernel DMABUF — the Mesa-RADV convention for
DCC-compressed RGBA exports.

## Empirical foundation

`scrying/tests/wpe_to_vulkan_roundtrip.rs`'s plane-fd diagnostic
(committed `48c8749`) measured the real WPE-on-AMD output on
Fedora 44 / Mesa-RADV:

```
plane[0]: fd=31 st_ino=681 offset=0       stride=4096
plane[1]: fd=32 st_ino=681 offset=3145728 stride=1024
```

- **Shared `st_ino=681`** — both dup'd fds reference the same kernel
  DMABUF.
- **Plane 0:** color data (offset=0, stride=4096; 1024 px × 4 B = linear
  BGRA).
- **Plane 1:** DCC metadata (offset=3 MiB; that's exactly
  `1024 × 768 × 4 = 3145728` — the color plane's footprint; stride=1024,
  much smaller because compression metadata is dense).

This confirms the design: single underlying buffer + multiple
per-plane offsets, all pointing into one VkDeviceMemory.

## Scope

In:
- `dmabuf::import` accepts `planes.len() > 1` when an `fstat` check shows
  all plane fds map to the same st_ino (same kernel DMABUF).
- Builds an N-entry `plane_layouts` array threading each plane's
  `(offset, stride)` into `VkImageDrmFormatModifierExplicitCreateInfoEXT`.
- Single VkDeviceMemory import from `planes[0].fd`; redundant fds in
  `planes[1..N]` closed after the import succeeds.
- `wpe_to_vulkan_roundtrip.rs` flips from Outcome B (panic on Err) to
  Outcome A (Ok + dimension/format assertions + pixel-correctness
  sampling).
- Pure-Rust unit tests for the `planes_share_kernel_object(planes)`
  shared-fd predicate.

Out (deferred):
- **Disjoint-fd multi-plane** — different st_inos across planes would
  mean separate kernel DMABUFs (`VK_IMAGE_CREATE_DISJOINT_BIT` + N
  VkDeviceMemory imports + `VkBindImagePlaneMemoryInfo` chained bind).
  Still returns `NativeImportNotYetImplemented`.
- **YUV ycbcr conversion** — multi-planar VkFormats + Vulkan sampler
  ycbcr conversion. Different shape entirely; wgpu 29 doesn't expose
  ycbcr cleanly. Stays deferred per the existing dmabuf.rs comment.

## Design

### `dmabuf::import` changes

Locate the existing rejection at `scrying/src/native_frame/dmabuf.rs:69-83`:

```rust
if frame.planes.len() > 1 {
    // ... (multi-plane deferred YUV / DCC comment) ...
    return Err(InteropError::Unsupported(
        UnsupportedReason::NativeImportNotYetImplemented,
    ));
}
```

Replace with a shared-fd guard + branch:

```rust
if frame.planes.len() > 1 && !planes_share_kernel_object(&frame.planes) {
    // Disjoint multi-plane (different kernel DMABUFs per plane) means
    // YUV-style separate-memory binding or genuinely independent fds.
    // Both need DISJOINT_BIT + per-plane VkBindImagePlaneMemoryInfo;
    // defer.
    return Err(InteropError::Unsupported(
        UnsupportedReason::NativeImportNotYetImplemented,
    ));
}
// At this point: either single-plane, or multi-plane sharing one
// kernel DMABUF. Both paths use a single VkDeviceMemory import; the
// multi-plane case just provides extra per-plane layouts.
```

Update the `plane_layouts` array construction (currently hard-coded
length 1 at lines 124-130) to N entries:

```rust
let plane_layouts: Vec<vk::SubresourceLayout> = frame.planes.iter()
    .map(|p| vk::SubresourceLayout {
        offset: p.offset as u64,
        size: 0, // implementation-defined; ignored for DMABUFs
        row_pitch: p.stride as u64,
        array_pitch: 0,
        depth_pitch: 0,
    })
    .collect();

let mut drm_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
    .drm_format_modifier(effective_modifier)
    .plane_layouts(&plane_layouts);
```

The rest of the Vulkan plumbing (image creation, memory requirements,
single VkDeviceMemory allocation from `planes[0].fd`, bind to offset 0,
texture wrap) stays unchanged — the multi-plane layout is communicated
entirely via `plane_layouts`.

**After successful `bind_image_memory`**, close the redundant fds:

```rust
// All planes share one kernel DMABUF; Vulkan owns it now via
// planes[0].fd. The other plane fds are extra dups we no longer need.
for plane in &frame.planes[1..] {
    // SAFETY: producer-allocated dup'd fd, redundant after Vulkan took
    // ownership of planes[0].fd (which references the same kernel obj).
    unsafe { libc::close(plane.fd); }
}
```

### `planes_share_kernel_object` predicate

Pure-Rust helper, unit-testable:

```rust
/// Returns true iff all plane fds map to the same kernel object
/// (same `st_ino`). On AMD-Mesa-RADV, multi-plane DCC-compressed
/// buffers emit N dup'd fds all referencing the same DMABUF — that's
/// the case this importer's multi-plane path supports today. Disjoint
/// fds (different `st_ino`s) imply genuinely separate kernel buffers,
/// which requires `VK_IMAGE_CREATE_DISJOINT_BIT` + per-plane memory
/// bindings; that path is deferred.
///
/// On fstat failure, conservatively returns false (treat as disjoint).
fn planes_share_kernel_object(planes: &[DmaBufPlane]) -> bool {
    if planes.len() <= 1 {
        return true;
    }
    let first_ino = match fstat_inode(planes[0].fd) {
        Some(ino) => ino,
        None => return false,
    };
    planes[1..].iter().all(|p| fstat_inode(p.fd) == Some(first_ino))
}

fn fstat_inode(fd: i32) -> Option<u64> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
    if rc == 0 {
        Some(unsafe { st.assume_init() }.st_ino)
    } else {
        None
    }
}
```

### `wpe_to_vulkan_roundtrip.rs` changes

Currently the test panics on Err with the actionable signal message.
After (α) lands the import returns Ok, so the test must:

1. Drop the panic-on-Err arm; assert Ok instead.
2. Keep the diagnostic eprintlns (plane-fd inode/offset/stride + the
   smoke line) — they're useful runtime documentation.
3. Assert imported texture dimensions match `image.size` (already in the
   spec for that test; now actually reachable).
4. **NEW: pixel-correctness sampling.** Read back a small region of the
   imported texture (mirror `tests/dmabuf_roundtrip.rs::read_back_texture`,
   which already exists for the gbm-buffer case) and assert the readback
   is predominantly **dodger-blue** (`#1e90ff` — the rendered HTML
   background). Tolerance: ±8 per channel to account for sRGB rounding /
   subpixel rendering / DCC decompression precision.

Pixel correctness is what genuinely proves the DCC-aware multi-plane
import worked — without it, "Vulkan returned Ok" only proves the import
shape didn't reject, not that the GPU is sampling the right bytes.

### Sample-region size

Sample a small inner rectangle (e.g. 64×64 at center) rather than the
whole 1024×768 buffer. Smaller readback = faster test; the rendered
background is uniform so a center sample is representative. If the
imported texture is mis-tiled (DCC metadata ignored), the center sample
would still be garbage — the test catches it.

### Color-equivalence check

`#1e90ff` is `(30, 144, 255)` in sRGB. With wgpu format
`Bgra8UnormSrgb`, readback bytes are sRGB-encoded. The HTML
`background:#1e90ff` paints linear-light; when the GPU rasterizes into
the sRGB-typed render target, it converts to sRGB-encoded bytes. So
readback bytes should be approximately `(255, 144, 30, 255)` in BGRA
order. Test tolerance ±8 per channel.

## File structure

- **Modify:** `scrying/src/native_frame/dmabuf.rs` — replace the
  multi-plane reject with the shared-fd guard; expand `plane_layouts`
  to N entries; close redundant fds after import; add the
  `planes_share_kernel_object` + `fstat_inode` helpers.
- **Modify:** `scrying/tests/wpe_to_vulkan_roundtrip.rs` — drop the
  panic-on-Err, assert Ok + pixel-correctness via a small readback.
- **Add pure-Rust unit tests** to `dmabuf.rs` (or a sibling test file
  if existing tests aren't there) for `planes_share_kernel_object` —
  using `dup`'d pipe fds (same kernel object → same st_ino) and
  separate `pipe2` allocations (different kernel objects).

## Testing

**Pure-Rust unit tests (no display, no GPU):**
- `planes_share_kernel_object` with one plane → `true`.
- `planes_share_kernel_object` with two dup'd fds of the same pipe →
  `true` (same st_ino).
- `planes_share_kernel_object` with two fds from independent `pipe2`
  calls → `false` (different st_inos).
- `planes_share_kernel_object` with an invalid fd → `false`
  (conservative).

**Runtime integration test:**
- `wpe_to_vulkan_roundtrip` flips from FAIL (Outcome B) to PASS
  (Outcome A) with: imported texture has correct dimensions, correct
  format, and a 64×64 center-sample reading approximately dodger-blue.

## Empirical risk

The fstat-and-merge import path is well-defined by the Vulkan spec, but
RADV may still reject if it expects a specific binding shape we got
wrong (e.g. `VK_KHR_image_format_list` threaded through, or N-plane
support gated on a separate device extension). The runtime test is the
oracle: Outcome A passing is the proof. If RADV rejects, the iteration
path is to inspect the validation-layer message and adjust (e.g. try
`vkBindImageMemory2`, check device features, etc.).

If pixel-correctness fails but Vulkan accepts the import — the import
succeeds but readback bytes are garbage — that suggests DCC metadata
wasn't threaded correctly. We'd need to re-check the plane_layouts
order (plane 0 = color, plane 1 = aux per Mesa convention) and the
`VK_IMAGE_USAGE_*` flags.

## Followups this informs

- **Disjoint-fd multi-plane** when a producer that needs DISJOINT_BIT
  shows up (rare; YUV is the usual case but wgpu 29 still can't
  represent that cleanly).
- **`wpe-on-Intel-Iris` or similar** where the modifier-and-plane
  convention may differ — the diagnostic in
  `wpe_to_vulkan_roundtrip.rs` is the source of truth; reading its
  output on a different driver will reveal whether the shared-fd
  assumption generalizes.
- **demo-wpe** (4c.6) can now show actual rendered pixels via an
  imported wgpu texture path.

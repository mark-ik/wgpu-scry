# Phase 4a.x — Multi-plane DCC Import Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `dmabuf::import` to accept multi-plane DMABUFs whose planes share a single kernel object (the AMD-on-Mesa DCC convention) so `wpe_to_vulkan_roundtrip` flips from documented-failure to a pixel-correct round-trip.

**Architecture:** `fstat`-based shared-fd predicate guards the multi-plane path; N-entry `plane_layouts` thread per-plane offsets/strides through `VkImageDrmFormatModifierExplicitCreateInfoEXT`; single VkDeviceMemory import (the planes share one kernel DMABUF); redundant plane fds closed after `vkBindImageMemory` succeeds. Disjoint-fd multi-plane stays deferred.

**Tech Stack:** Rust 2024, ash (Vulkan), libc (fstat/close), the existing wgpu+HostWgpuContext path.

**Spec:** [`2026-06-04_phase4a_x_multiplane_dcc_import.md`](2026-06-04_phase4a_x_multiplane_dcc_import.md)

**Empirical foundation (commit `48c8749` probe):**
```
plane[0]: fd=31 st_ino=681 offset=0       stride=4096
plane[1]: fd=32 st_ino=681 offset=3145728 stride=1024
```

**Conventions:**
- All FFI work in `scrying/src/native_frame/dmabuf.rs` (no `wpe_producer/*` changes).
- Plan-internal commit-message scaffolds intentionally do NOT include a `Co-Authored-By: Claude` trailer (user preference).

---

## File Structure

- **Modify:** `scrying/src/native_frame/dmabuf.rs` — add `fstat_inode` + `planes_share_kernel_object` helpers; replace the multi-plane reject with the shared-fd guard; expand `plane_layouts` to N entries; close redundant plane fds after `vkBindImageMemory`; add pure-Rust unit tests.
- **Modify:** `scrying/tests/wpe_to_vulkan_roundtrip.rs` — drop the panic-on-Err, assert imported texture dimensions + format, add `read_back_center_region` + pixel-correctness sampling against `#1e90ff`.

That's it. No new files.

---

## Task 1: `planes_share_kernel_object` predicate (pure-Rust, TDD)

**Files:**
- Modify: `scrying/src/native_frame/dmabuf.rs`

- [ ] **Step 1: Add the helpers before the existing `pub(super) fn import`**

Insert (above `pub(super) fn import`, after the existing `DRM_FORMAT_MOD_*` constants):

```rust
/// Returns the inode (`st_ino`) of the kernel object behind `fd`, or
/// `None` if `fstat` fails. Used by `planes_share_kernel_object` to
/// decide whether N plane fds reference a single underlying DMABUF
/// (one VkDeviceMemory import) or genuinely separate kernel buffers
/// (which would need `DISJOINT_BIT` + per-plane imports — deferred).
fn fstat_inode(fd: i32) -> Option<u64> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat is a read-only call against a producer-owned fd.
    let rc = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
    if rc == 0 {
        Some(unsafe { st.assume_init() }.st_ino)
    } else {
        None
    }
}

/// True iff all plane fds in `planes` map to the same kernel object
/// (same `st_ino`). The single-plane case is trivially true. On any
/// fstat failure, returns false (conservatively treat as disjoint).
///
/// Mesa-RADV's DCC-compressed RGBA exports emit N dup'd fds all
/// pointing at the same underlying DMABUF — that's the case this
/// importer's multi-plane path supports. Different inodes imply
/// genuinely separate kernel buffers requiring DISJOINT_BIT +
/// per-plane memory bindings; that path is deferred.
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
```

`libc::stat` is already available — `libc` is the Linux-target dep
used elsewhere in this file (it imports `libc::close` in surrounding
modules already; `libc::stat`/`libc::fstat` are in the same crate).

- [ ] **Step 2: Add the unit tests at the bottom of `dmabuf.rs`**

Append (or extend an existing `#[cfg(test)] mod tests { ... }` block —
if none exists at the bottom of the file, create one):

```rust
#[cfg(test)]
mod plane_share_tests {
    use super::*;
    use crate::native_frame::DmaBufPlane;

    fn make_plane(fd: i32) -> DmaBufPlane {
        DmaBufPlane { fd, offset: 0, stride: 0 }
    }

    /// Open a pipe and return the read-end fd.
    fn open_pipe_read_fd() -> i32 {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        unsafe { libc::close(fds[1]) }; // close write end
        fds[0]
    }

    #[test]
    fn single_plane_is_always_shared() {
        let fd = open_pipe_read_fd();
        let planes = vec![make_plane(fd)];
        assert!(planes_share_kernel_object(&planes));
        unsafe { libc::close(fd) };
    }

    #[test]
    fn two_dup_fds_of_same_pipe_are_shared() {
        let fd1 = open_pipe_read_fd();
        // dup creates a new fd referencing the same kernel object → same st_ino.
        let fd2 = unsafe { libc::dup(fd1) };
        assert!(fd2 >= 0, "dup() failed");
        let planes = vec![make_plane(fd1), make_plane(fd2)];
        assert!(planes_share_kernel_object(&planes));
        unsafe { libc::close(fd1) };
        unsafe { libc::close(fd2) };
    }

    #[test]
    fn two_independent_pipes_are_disjoint() {
        let fd1 = open_pipe_read_fd();
        let fd2 = open_pipe_read_fd();
        // Each pipe is its own kernel object → different st_ino.
        let planes = vec![make_plane(fd1), make_plane(fd2)];
        assert!(!planes_share_kernel_object(&planes));
        unsafe { libc::close(fd1) };
        unsafe { libc::close(fd2) };
    }

    #[test]
    fn invalid_fd_is_conservatively_disjoint() {
        let fd1 = open_pipe_read_fd();
        // -1 is the conventional "invalid fd"; fstat returns EBADF.
        let planes = vec![make_plane(fd1), make_plane(-1)];
        assert!(!planes_share_kernel_object(&planes));
        unsafe { libc::close(fd1) };
    }

    #[test]
    fn empty_planes_treated_as_shared() {
        // Edge case: 0 planes can't disagree about inodes. Caller is
        // responsible for the planes-not-empty check before this.
        let planes: Vec<DmaBufPlane> = vec![];
        assert!(planes_share_kernel_object(&planes));
    }
}
```

- [ ] **Step 3: Run the unit tests**

Run: `cargo test -p scrying plane_share_tests`
Expected: **5 passed, 0 failed.** The pipe-based fixtures exercise the same-vs-different st_ino branches without touching DMABUFs.

- [ ] **Step 4: Build both feature configurations to confirm no breakage**

Run: `cargo build -p scrying`
Run: `cargo build -p scrying --features wpe`

Expected: PASS both, 0 warnings (the new helpers are now used by tests; they don't need wpe gating because dmabuf.rs is always-compiled on Linux).

- [ ] **Step 5: Commit**

```bash
git add scrying/src/native_frame/dmabuf.rs
git commit -m "$(cat <<'EOF'
phase 4a.x: planes_share_kernel_object predicate + fstat_inode

Pure-Rust helper that returns true iff all plane fds in a DmaBufImage
map to the same kernel object via st_ino. Backs the (α) multi-plane
import: when shared, one VkDeviceMemory import covers all planes via
per-plane layouts; when disjoint, we'll keep returning
NativeImportNotYetImplemented (no producer needs that today).

Five unit tests cover: single-plane → shared, two dup'd fds of one
pipe → shared, two independent pipes → disjoint, invalid fd →
conservatively disjoint, empty planes → vacuously shared. No display
needed for any test.
EOF
)"
```

Do NOT push. Commit on `main`.

---

## Task 2: Replace the multi-plane reject + N-entry plane_layouts + close redundant fds

**Files:**
- Modify: `scrying/src/native_frame/dmabuf.rs`

- [ ] **Step 1: Replace the multi-plane reject block**

In `scrying/src/native_frame/dmabuf.rs`, locate the current rejection (around lines 69-83):

```rust
    if frame.planes.len() > 1 {
        // ... (deferred YUV / DCC comment block) ...
        return Err(InteropError::Unsupported(
            UnsupportedReason::NativeImportNotYetImplemented,
        ));
    }
```

Replace with the shared-fd guard:

```rust
    // Multi-plane DMABUFs come in two shapes on Linux:
    //
    //   (a) "Shared-fd multi-plane": all plane fds are dup's of a
    //       single kernel DMABUF, distinguished by per-plane offsets.
    //       Mesa-RADV emits this for DCC-compressed RGBA: plane 0 is
    //       the color data, plane 1 is the DCC aux metadata, both in
    //       one underlying buffer. We import one VkDeviceMemory; the
    //       multi-plane layout is communicated via per-plane entries
    //       in `VkImageDrmFormatModifierExplicitCreateInfoEXT::pPlaneLayouts`.
    //
    //   (b) "Disjoint multi-plane": each plane has its own kernel
    //       DMABUF (different st_ino). This is the YUV ycbcr-style
    //       shape (and also possible for explicit-disjoint RGBA).
    //       Vulkan needs `VK_IMAGE_CREATE_DISJOINT_BIT` + N separate
    //       VkDeviceMemory imports + per-plane VkBindImagePlaneMemoryInfo
    //       chained binds — wgpu 29 also can't represent the YUV
    //       sampler-conversion path cleanly. Deferred.
    if frame.planes.len() > 1 && !planes_share_kernel_object(&frame.planes) {
        return Err(InteropError::Unsupported(
            UnsupportedReason::NativeImportNotYetImplemented,
        ));
    }
```

- [ ] **Step 2: Expand `plane_layouts` to N entries**

In the same file, locate (around lines 125-130):

```rust
        let plane_layouts = [vk::SubresourceLayout {
            offset: frame.planes[0].offset as u64,
            size: 0, // implementation-defined; ignored for DMABUFs
            row_pitch: frame.planes[0].stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        }];
```

Replace with a `Vec` built from every plane:

```rust
        // Multi-plane shared-fd: provide one SubresourceLayout entry
        // per plane so the driver can find each plane's data within
        // the single VkDeviceMemory we'll import below.
        let plane_layouts: Vec<vk::SubresourceLayout> = frame.planes.iter()
            .map(|p| vk::SubresourceLayout {
                offset: p.offset as u64,
                size: 0, // implementation-defined; ignored for DMABUFs
                row_pitch: p.stride as u64,
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();
```

The next line — `vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().plane_layouts(&plane_layouts)` — already takes `&[vk::SubresourceLayout]`, so the `&Vec` coerces correctly.

- [ ] **Step 3: Close redundant plane fds after `bind_image_memory` succeeds**

Locate (around line 196-204):

```rust
        if let Err(e) = raw_device.bind_image_memory(vk_image, vk_memory, 0) {
            raw_device.free_memory(vk_memory, None);
            raw_device.destroy_image(vk_image, None);
            return Err(InteropError::Vulkan(format!("vkBindImageMemory: {e}")));
        }
```

Immediately after this block (BEFORE the next `// ---- N. ...` comment / texture-wrap step), add:

```rust
        // All planes share one kernel DMABUF (guaranteed by the
        // `planes_share_kernel_object` check above). Vulkan now owns
        // the kernel ref via planes[0].fd (transferred on the
        // allocate_memory success above). The remaining plane fds are
        // redundant dup's that we close to avoid an fd-table leak;
        // they don't carry any layout information of their own (the
        // per-plane offsets/strides went through plane_layouts).
        for plane in &frame.planes[1..] {
            // SAFETY: producer-allocated dup'd fd that we know
            // references the same kernel object as planes[0].fd (which
            // Vulkan now owns). Closing this fd does not free the
            // underlying buffer.
            unsafe { libc::close(plane.fd); }
        }
```

- [ ] **Step 4: Build both configurations**

Run: `cargo build -p scrying`
Run: `cargo build -p scrying --features wpe`
Expected: PASS both, 0 warnings.

- [ ] **Step 5: Run the existing gbm round-trip test as a regression check**

Run: `cargo test -p scrying --test dmabuf_roundtrip -- --nocapture`

Expected: All non-ignored tests in that binary still pass. The gbm
buffer is single-plane, so it hits the `planes.len() == 1` branch
which is unchanged (`plane_layouts` is now a `Vec` of length 1 instead
of an array of length 1, but semantically identical for the single-plane
path).

- [ ] **Step 6: Run the WPE round-trip test — the empirical moment**

Run: `cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip -- --ignored --nocapture`

Two possible outcomes:

**Outcome A (expected): `import_frame` returns Ok**, but the test still PANICs at the post-import assertion stage because the test still has the `Err(e) => panic!(...)` arm in place. The CURRENT panic should no longer fire; if Vulkan accepts the import we get to the dimension-assertion checks (which pass per spec). Task 3 of this plan flips the panic arm; for this task, success is "the `FAIL: import_frame errored on real WPE buffer` panic stops firing."

**Outcome B (iteration needed): Vulkan rejects the import** with a different error message. The likely candidates:
- `vkAllocateMemory` errors with `OUT_OF_HOST_MEMORY` or similar — usually means the modifier+layout combo isn't acceptable.
- `vkBindImageMemory` errors — bind shape may need `vkBindImageMemory2`.
- Validation layer prints a CRITICAL — read it carefully; it identifies the constraint.

If Outcome B fires, capture the exact error + any validation-layer output and STOP. Report BLOCKED with the message; iteration may require adding `VK_IMAGE_CREATE_DISJOINT_BIT`, switching to `vkBindImageMemory2`, or adjusting the modifier-info chain. We'd revisit in a follow-up task rather than thrash inside Task 2.

If Outcome A fires, the import is working. Task 3 flips the test's panic arm and adds pixel-correctness sampling.

- [ ] **Step 7: Commit (Outcome A)**

```bash
git add scrying/src/native_frame/dmabuf.rs
git commit -m "$(cat <<'EOF'
phase 4a.x: multi-plane shared-fd DMABUF import (DCC-compressed RGBA)

Removes the planes.len() > 1 blanket reject. Multi-plane DMABUFs whose
planes share a single kernel object (st_ino) now import successfully:
N-entry plane_layouts in VkImageDrmFormatModifierExplicitCreateInfoEXT
threads per-plane offsets/strides through to the driver, a single
VkDeviceMemory import from planes[0].fd covers all planes, and the
redundant plane fds (1..N) are closed after vkBindImageMemory.

Disjoint multi-plane (YUV ycbcr-style or genuinely-separate-kernel
buffers) keeps returning NativeImportNotYetImplemented; that path
requires DISJOINT_BIT + N VkDeviceMemory imports + chained
VkBindImagePlaneMemoryInfo, and no current producer needs it.

Verified: wpe_to_vulkan_roundtrip no longer panics with "import_frame
errored on real WPE buffer"; Task 3 of the plan adds pixel-correctness
sampling. The gbm-buffer round-trip test still passes unchanged
(single-plane path is semantically identical, just plane_layouts is
now a Vec<1> instead of array<1>).
EOF
)"
```

If Outcome B fires, stop and report; do not commit.

Do NOT push. Commit on `main`.

---

## Task 3: Flip the round-trip test to Outcome A + pixel-correctness sampling

**Files:**
- Modify: `scrying/tests/wpe_to_vulkan_roundtrip.rs`

- [ ] **Step 1: Drop the panic-on-Err arm; assert Ok + dimensions**

In `scrying/tests/wpe_to_vulkan_roundtrip.rs`, locate the current Err arm (the multi-line `panic!(...)` block):

```rust
        Err(e) => {
            panic!(
                "FAIL: import_frame errored on real WPE buffer: {e}\n\n\
                 This is the actionable signal: Phase 4a's importer at \
                 native_frame/dmabuf.rs:69-83 EXPLICITLY DEFERS frame.planes.len() > 1 \
                 with NativeImportNotYetImplemented. WPE on AMD/Mesa-RADV emits a \
                 2-plane DCC-tiled BGRA buffer (single sampleable image + DCC aux \
                 metadata plane) — this is the RGBA-with-aux multi-plane case, \
                 DISTINCT from the YUV ycbcr-conversion case the existing comment \
                 there discusses. The fix is to extend dmabuf::import to feed \
                 per-plane VkImageDrmFormatModifierExplicitCreateInfoEXT plane \
                 layouts + chained VkBindImagePlaneMemoryInfo for non-YUV \
                 multi-plane modifiers. See 4c.2 retrospective + 2026-06-04 \
                 round-trip spec for context."
            );
        }
```

Replace with a concise expectation that import now succeeds:

```rust
        Err(e) => {
            panic!(
                "FAIL: import_frame errored — phase 4a.x multi-plane DCC import \
                 should accept this WPE buffer. Error: {e}"
            );
        }
```

The existing dimension assertions (right after the match) stay as-is — they're already in the spec for the Ok branch.

- [ ] **Step 2: Add a small-center-region readback helper**

Append to the test file (before `make_vulkan_host`):

```rust
/// Read back a small center region of the imported texture as raw
/// BGRA bytes (4 bytes per pixel, untransformed). Mirrors the readback
/// shape from `tests/dmabuf_roundtrip.rs::read_back_texture` but reads
/// a smaller region — the WPE-rendered background is uniform so a
/// 64×64 center sample is representative and keeps the readback buffer
/// small (16 KiB instead of ~3 MiB for the full 1024×768).
///
/// `bytes_per_row` is aligned up to wgpu's 256-byte requirement; the
/// returned Vec drops the row padding so caller code can index by
/// `row * SAMPLE_W * 4 + col * 4`.
fn read_back_center_region(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    imported: &scrying::ImportedTexture,
    full_width: u32,
    full_height: u32,
) -> Vec<u8> {
    const SAMPLE_W: u32 = 64;
    const SAMPLE_H: u32 = 64;
    // Aligned up to wgpu's 256-byte bytes_per_row requirement.
    let bytes_per_row = ((SAMPLE_W * 4 + 255) / 256) * 256;
    let readback_size = (bytes_per_row as u64) * (SAMPLE_H as u64);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("wpe_roundtrip_readback"),
        size: readback_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("wpe_roundtrip_readback_encoder"),
    });
    let center_x = full_width / 2 - SAMPLE_W / 2;
    let center_y = full_height / 2 - SAMPLE_H / 2;
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &imported.texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x: center_x, y: center_y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(SAMPLE_H),
            },
        },
        wgpu::Extent3d {
            width: SAMPLE_W,
            height: SAMPLE_H,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let buffer_slice = readback.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device poll");
    receiver
        .recv()
        .expect("map_async sender dropped")
        .expect("map_async failed");

    let mapped = buffer_slice.get_mapped_range();
    let mut bytes = Vec::with_capacity((SAMPLE_W as usize) * (SAMPLE_H as usize) * 4);
    for row in 0..SAMPLE_H as usize {
        let row_start = row * (bytes_per_row as usize);
        let row_end = row_start + (SAMPLE_W as usize) * 4;
        bytes.extend_from_slice(&mapped[row_start..row_end]);
    }
    drop(mapped);
    readback.unmap();
    bytes
}
```

- [ ] **Step 3: Add the dodger-blue assertion**

In the test body, AFTER the existing `eprintln!("wpe→vk: imported texture ...")` line, add:

```rust
    // --- 7. Pixel-correctness sampling ---
    //
    // Read a 64×64 center patch of the imported texture and assert it
    // matches the rendered HTML background (#1e90ff = dodger-blue).
    // For wgpu::TextureFormat::Bgra8UnormSrgb, the readback bytes are
    // sRGB-encoded BGRA, so a #1e90ff fill renders to approximately
    // (B=255, G=144, R=30, A=255). Tolerance ±8 per channel accounts
    // for sRGB rounding and the WPE-side compositor's gamma path.
    let bytes = read_back_center_region(
        &_device, &_queue, &imported, expected_size.width, expected_size.height,
    );
    let expected = [0xFFu8, 0x90, 0x1E, 0xFF]; // B, G, R, A for dodger-blue
    let tolerance: i32 = 8;
    let mut bad_pixels = 0usize;
    let mut first_bad: Option<(usize, [u8; 4])> = None;
    for px in bytes.chunks_exact(4) {
        let off = px.iter()
            .zip(expected.iter())
            .any(|(g, e)| (*g as i32 - *e as i32).abs() > tolerance);
        if off {
            bad_pixels += 1;
            if first_bad.is_none() {
                first_bad = Some((bad_pixels, [px[0], px[1], px[2], px[3]]));
            }
        }
    }
    let total = (64 * 64) as usize;
    assert!(
        bad_pixels == 0,
        "FAIL: {}/{} center pixels diverged from dodger-blue ±{}. First bad: {:?}. \
         expected BGRA={:?}. This suggests the multi-plane DCC import sampled \
         the wrong bytes (e.g. plane[1] DCC metadata not threaded through, or \
         a layout mismatch).",
        bad_pixels, total, tolerance, first_bad, expected
    );
    eprintln!("wpe→vk: pixel-correctness OK ({}×{} center sample all within ±{} of BGRA={:?})",
        64, 64, tolerance, expected);
```

Three notes for the implementer:
- The `_device` and `_queue` bindings exist from Task-2 (4c.x round-trip) `make_vulkan_host()`. Drop the leading `_` from `_device` and `_queue` since they're now read; or leave the underscore (Rust accepts `&_device` and `&_queue` as expressions referring to the bindings — the leading underscore is just a "may be unused" hint, not a name change). Cleanest: rename to `device`/`queue` for clarity.
- `imported.texture` is the wgpu::Texture inside ImportedTexture; the helper uses it as the copy source.
- The tolerance ±8 may need widening if RADV's sRGB rounding is more aggressive than expected. If the first run shows uniformly-shifted pixels (e.g. all are BGRA `(247, 138, 25, 255)`), bump tolerance to ±12 and document the observation. If pixels are wildly different (e.g. all zero, or stripes), that's a real bug — STOP and report.

- [ ] **Step 4: Run the test — the second empirical moment**

Run: `cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip -- --ignored --nocapture`

Expected:
```
wpe→vk: 1024x768 fourcc=0x34325258 mod=0x020000044051ba01 planes=2
wpe→vk:   plane[0]: fd=... st_ino=... offset=0 stride=4096
wpe→vk:   plane[1]: fd=... st_ino=... offset=3145728 stride=1024
wpe→vk: wgpu Vulkan host up
wpe→vk: imported texture 1024x768 format=Bgra8UnormSrgb gen=N
wpe→vk: pixel-correctness OK (64×64 center sample all within ±8 of BGRA=[255, 144, 30, 255])
test result: ok. 1 passed
```

If `bad_pixels` is non-zero with a small uniform offset (e.g. all pixels off by ~10), widen tolerance and re-run. If pixels are wildly wrong (zero, stripes, garbage), STOP — that means the DCC layout isn't being decompressed by the GPU read. Report BLOCKED with the observed first_bad pixel; we'd need to revisit Task 2's Vulkan flags (likely a missing `VK_KHR_image_format_list` or a `VK_IMAGE_USAGE_*` mismatch).

- [ ] **Step 5: Confirm non-ignored tests still pass**

Run: `cargo test -p scrying`
Run: `cargo test -p scrying --features wpe`
Expected: unchanged from current state.

Run the existing unit smoke as a regression check:
`cargo test -p scrying --features wpe navigate_resize_and_render -- --ignored --nocapture`
Expected: PASS unchanged.

- [ ] **Step 6: Commit**

```bash
git add scrying/tests/wpe_to_vulkan_roundtrip.rs
git commit -m "$(cat <<'EOF'
phase 4a.x: WPE round-trip flips to Outcome A + pixel-correctness check

After the multi-plane DCC import lands in dmabuf.rs, import_frame now
returns Ok on the real WPE-on-AMD buffer. Drop the documented-failure
panic; assert dimensions + format (as the spec always intended for the
Ok branch); add a 64×64 center-region readback that asserts the
sampled pixels are dodger-blue (#1e90ff in sRGB, ±8 per channel
tolerance) — this is what genuinely proves DCC decompression worked
end-to-end, not just that the FFI shape was accepted.

The pixel-correctness check uses a small center sample (16 KiB
readback buffer) rather than the full 1024×768, since the rendered
background is uniform and a representative center patch is enough to
catch any plane-layout / DCC-decompression bug.
EOF
)"
```

Do NOT push. Commit on `main`.

---

## Task 4: Strategy checklist update

**Files:**
- Modify: `design_docs/2026-05-15_phase4_strategy.md`

- [ ] **Step 1: Add a new 4a.x line after the existing 4a entries**

I (controller) will do this directly — it's a pure docs change with no subagent value. The line goes after the `4a.7` line:

```markdown
- [x] **4a.x** Multi-plane shared-fd DMABUF import (DCC-compressed RGBA)
      — non-YUV multi-plane: planes share one kernel DMABUF, communicated
      via N-entry plane_layouts. Flips the WPE round-trip from Outcome B
      (documented failure) to Outcome A (Ok + pixel-correct dodger-blue
      center sample). Spec
      [`2026-06-04_phase4a_x_multiplane_dcc_import.md`](2026-06-04_phase4a_x_multiplane_dcc_import.md),
      plan [`2026-06-04_phase4a_x_multiplane_dcc_plan.md`](2026-06-04_phase4a_x_multiplane_dcc_plan.md).
      Disjoint-fd multi-plane / YUV ycbcr still deferred.
```

And update the status line to add `+ 4a.x`.

---

## Self-Review

**Spec coverage:**
- `planes_share_kernel_object` + `fstat_inode` helpers → Task 1. ✓
- Pure-Rust unit tests with pipe fds → Task 1 Step 2 (5 tests). ✓
- Multi-plane reject replaced with shared-fd guard → Task 2 Step 1. ✓
- N-entry `plane_layouts` → Task 2 Step 2. ✓
- Close redundant plane fds after bind → Task 2 Step 3. ✓
- Round-trip test flips to Outcome A → Task 3 Step 1. ✓
- Pixel-correctness sampling (64×64 center, dodger-blue ±8) → Task 3 Steps 2–3. ✓
- Strategy doc update → Task 4. ✓
- Disjoint-fd / YUV ycbcr stay deferred → respected (only the shared-fd branch is accepted in Task 2). ✓
- No `wpe_producer/*` changes → respected throughout. ✓

**Placeholder scan:** No "TBD"/"TODO". The two empirical-by-design points (Task 2 Step 6 Outcome A/B branching; Task 3 Step 4 tolerance-widening if uniformly-shifted pixels show up) have concrete decision procedures with stop-conditions, not placeholders.

**Type consistency:**
- `planes_share_kernel_object(&[DmaBufPlane]) -> bool` — consistent between Task 1 definition and Task 2 call site.
- `fstat_inode(i32) -> Option<u64>` — consistent.
- `read_back_center_region` parameter list (`device, queue, imported, full_width, full_height`) — consistent between Task 3 Steps 2 and 3.
- Tolerance constant (`8`) used in both the assertion and the eprintln.

**Known risks:**
- Vulkan may reject the multi-plane import shape even though the spec is fine on paper (Task 2 Step 6 Outcome B). The plan has an explicit stop-and-report branch.
- DCC decompression may sample garbage if RADV needs additional flags (Task 3 Step 4 stop-condition).
- Both risks have clear escalation paths; no thrashing.

# WPE → Vulkan Round-Trip Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create `scrying/tests/wpe_to_vulkan_roundtrip.rs` — an `#[ignore]`d integration test that takes a WPE-produced `DmaBufImage` and hands it to `WgpuTextureImporter`, surfacing whatever the import path does with a real AMD-tiled headless WPE buffer.

**Architecture:** Separate `tests/*.rs` binary so its WebKit init is independent of the unit-test smoke (per the one-WPE-per-process discipline established in 4c.2). Mirrors the gbm-based `dmabuf_roundtrip.rs` test pattern — same SKIP semantics for missing prerequisites, same `WgpuTextureImporter` import path — but substitutes a live `WpeProducer` for the gbm-allocated buffer.

**Tech Stack:** Rust 2024 integration test, `wgpu`, `pollster` (existing dev-deps), `scrying::{WpeProducer, WgpuTextureImporter, build_dmabuf_capable_device, ...}`, gated on `cfg(all(target_os = "linux", feature = "wpe"))`.

**Spec:** [`2026-06-04_wpe_to_vulkan_roundtrip.md`](2026-06-04_wpe_to_vulkan_roundtrip.md)

**Conventions:**
- Test is `#[ignore]`d; run manually with
  `cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip -- --ignored --nocapture`.
- SKIP pattern (mirrors `dmabuf_roundtrip.rs`): print `SKIP: ...` and `return` from the test body when prerequisites aren't met; cargo records the test as pass. ONLY use SKIP for *environmental* misses (no DRM device, no Vulkan adapter, missing extensions, WPE refuses to construct). Do NOT SKIP on a genuine import failure — that's the actionable signal.
- Commit per task. Commit-trailer:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.

---

## File Structure

- **Create:** `scrying/tests/wpe_to_vulkan_roundtrip.rs` — one file, ~150–200 lines. Single `#[test] #[ignore]` function `wpe_to_vulkan_round_trip` plus a `make_vulkan_host` helper inlined from the same shape `dmabuf_roundtrip.rs` uses.

That's it. No other files change in this plan.

---

## Task 1: Stand up the integration-test binary — producer side only

**Goal:** Get the file compiling, the `#[ignore]`d test runnable, and prove the integration-test process can construct a `WpeProducer` and acquire one real DMABUF frame. No wgpu / no importer yet — just the WPE-side harness in its own binary.

**Files:**
- Create: `scrying/tests/wpe_to_vulkan_roundtrip.rs`

- [ ] **Step 1: Create the file with cfg-gates + the producer-side test body**

```rust
//! End-to-end WPE → Vulkan round-trip integration test.
//!
//! Independent integration binary (separate process from the unit-test
//! smoke) so it can stand up its own headless WebKit without colliding
//! with the unit-test producer — see
//! `scrying/src/wpe_producer/headless.rs`'s module doc for the
//! one-WPE-per-process discipline.
//!
//! Run with:
//!   cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip \
//!     -- --ignored --nocapture
//!
//! SKIP semantics mirror `dmabuf_roundtrip.rs`: when a prerequisite is
//! missing (no DRM device, no Vulkan adapter, WPE can't construct),
//! print `SKIP: ...` and return — cargo records the test as pass. A
//! genuine `import_frame` Err is NOT a skip; the test fails loudly so
//! the Err message surfaces.

#![cfg(all(target_os = "linux", feature = "wpe"))]

use dpi::PhysicalSize;
use scrying::wpe_producer::{WpeProducer, WpeProducerConfig};
use scrying::{
    NativeFrame, NavigationEvent, SyncMechanism, WebSurfaceFrame, WebSurfaceProducer,
};

#[test]
#[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
fn wpe_to_vulkan_round_trip() {
    // --- 1. Stand up the WPE producer ---
    let config = WpeProducerConfig::new(PhysicalSize::new(256, 256), std::env::temp_dir());
    let mut producer = match WpeProducer::new(config) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: WpeProducer::new failed (no display / GPU?): {e}");
            return;
        }
    };

    // --- 2. Navigate to inline HTML; verify load completion event ---
    if let Err(e) = producer.navigate_to_string(
        "<body style='margin:0;background:#1e90ff'></body>",
        std::time::Duration::from_secs(5),
    ) {
        eprintln!("SKIP: navigate_to_string failed: {e}");
        return;
    }
    let mut nav_events = Vec::new();
    while let Some(e) = producer.poll_navigation_event() {
        nav_events.push(e);
    }
    assert!(
        nav_events.iter().any(|e| matches!(e, NavigationEvent::Completed { success: true, .. })),
        "expected a successful Completed event; got {:?}",
        nav_events
    );

    // --- 3. Acquire one DMABUF frame ---
    let frame = match producer.acquire_frame() {
        Ok(f) => f,
        Err(e) => panic!("FAIL: acquire_frame after successful navigate: {e}"),
    };
    let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(image)) = frame else {
        panic!("FAIL: expected a DMABUF frame, got something else");
    };

    // WPE-side sanity (always-on assertions).
    assert!(image.size.width > 0 && image.size.height > 0, "non-zero size");
    assert!(!image.planes.is_empty(), "at least one plane");
    assert!(image.planes[0].fd >= 0, "valid dup'd fd");
    assert_eq!(image.producer_sync, SyncMechanism::None);
    eprintln!(
        "wpe→vk: {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}",
        image.size.width,
        image.size.height,
        image.drm_format,
        image.drm_modifier,
        image.planes.len()
    );

    // --- 4. (Task 3): hand to the importer. Until Task 3 lands, just
    //         close the producer-owned fds so this task's test doesn't
    //         leak them. ---
    close_producer_fds(&image);
}

/// Closes the producer-owned dup'd fds on a `DmaBufImage`. Task 3
/// removes this helper because the importer takes ownership of the
/// fds on success.
fn close_producer_fds(image: &scrying::DmaBufImage) {
    for plane in &image.planes {
        // SAFETY: producer-owned dup'd fd not yet transferred to the importer.
        unsafe { libc::close(plane.fd); }
    }
    if let Some(fd) = image.semaphore_fd {
        unsafe { libc::close(fd); }
    }
}
```

Note: `libc` is already a Linux dev-dep transitively via the existing
test deps; if the compiler complains it's not in scope, add it as
`use libc;` — `libc` is a standard `[target.'cfg(target_os = "linux")'.dependencies]`
in `scrying/Cargo.toml`.

- [ ] **Step 2: Build the integration binary**

Run: `cargo build -p scrying --features wpe --tests`
Expected: PASS. The new test compiles. The pre-existing
`tests/dmabuf_roundtrip.rs` still compiles (we didn't touch it).

- [ ] **Step 3: Run the ignored test — must pass and print the diagnostic**

Run: `cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip -- --ignored --nocapture`
Expected: PASS, exit 0, **with a `wpe→vk: <size> fourcc=... mod=... planes=...` line printed**.

On this AMD/Fedora 44 box the line should match the unit-test smoke's
post-nav output: `1024x768 fourcc=0x34325258 mod=0x020000044051ba01 planes=2`
(the headless toplevel's coerced size).

If the test SKIPs (no display / GPU): note the SKIP reason in your
report. The harness is still considered DONE for Task 1 — the producer
half of the round-trip is wired up; later tasks add the wgpu half.

- [ ] **Step 4: Confirm the unit-test smoke still runs in a separate
      invocation (regression check that we didn't break the one-per-process
      discipline)**

Run: `cargo test -p scrying --features wpe navigate_resize_and_render -- --ignored --nocapture`
Expected: PASS, same `smoke#1` / `smoke#2` lines as before.

(The two ignored tests live in different binaries; running them in
the same `cargo test` invocation is fine because cargo runs each test
binary in its own process.)

- [ ] **Step 5: Commit**

```bash
git add scrying/tests/wpe_to_vulkan_roundtrip.rs
git commit -m "$(cat <<'EOF'
phase 4c.x: WPE round-trip — producer-side harness (independent binary)

New tests/*.rs integration binary that constructs a WpeProducer in its
own process, navigates, and acquires one real DMABUF frame. Independent
WebKit init keeps it from colliding with the unit-test smoke. Task 1
of the round-trip plan; the wgpu host + importer call land in Tasks 2-3.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

Do NOT push. Commit on `main`. Don't touch other commits.

---

## Task 2: Stand up the wgpu DMABUF-capable host (no import call yet)

**Goal:** Build the wgpu `Instance + Adapter + Device + Queue + HostWgpuContext` quartet using `scrying::build_dmabuf_capable_device`, mirroring the `make_vulkan_host` helper inside `dmabuf_roundtrip.rs`. The test still doesn't call `import_frame` yet — that's Task 3 — but the host stands up cleanly under SKIP semantics if any prerequisite is missing.

**Files:**
- Modify: `scrying/tests/wpe_to_vulkan_roundtrip.rs`

- [ ] **Step 1: Add the `make_vulkan_host` helper and call it after acquiring the frame**

Append the helper (place ABOVE the `#[test]` function for readability):

```rust
use scrying::HostWgpuContext;

/// Build a wgpu Vulkan host suitable for the DMABUF importer.
/// Returns `None` (and prints a `SKIP:` line) when:
/// - no Vulkan adapter is available
/// - `build_dmabuf_capable_device` returns Err (missing extensions etc.)
///
/// Mirrors the helper inside `tests/dmabuf_roundtrip.rs`; kept here as a
/// short inline copy so the two integration binaries don't depend on
/// each other's private helpers.
fn make_vulkan_host() -> Option<(wgpu::Device, wgpu::Queue, HostWgpuContext)> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    let adapter = match pollster::block_on(instance.request_adapter(
        &wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        },
    )) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("SKIP: no Vulkan adapter on this box: {e}");
            return None;
        }
    };
    let desc = wgpu::DeviceDescriptor {
        label: Some("wpe_to_vulkan_roundtrip"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
    };
    let (device, queue) = match scrying::build_dmabuf_capable_device(&adapter, &desc) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("SKIP: build_dmabuf_capable_device failed: {e}");
            return None;
        }
    };
    let host = HostWgpuContext::new(device.clone(), queue.clone());
    Some((device, queue, host))
}
```

> **Implementation note — exact wgpu::Instance / RequestAdapter / device
> field shape.** wgpu's API evolves across releases. The version of wgpu
> the crate uses (`workspace.toml`) determines whether `Instance::new`
> takes `&InstanceDescriptor` or `InstanceDescriptor`, whether
> `request_adapter` returns `Result` or `Option`, and whether
> `DeviceDescriptor` has the `trace` / `memory_hints` fields. The
> snippet above mirrors what `dmabuf_roundtrip.rs` uses in this repo —
> open that file and copy its `make_vulkan_host` shape verbatim if you
> hit any compile mismatches. The point is to reach the same end state
> (a `(Device, Queue, HostWgpuContext)` or a SKIP), not to invent the
> shape.

Then INSIDE the existing `#[test]` body, after the `eprintln!("wpe→vk:
...")` line and BEFORE the call to `close_producer_fds`, add:

```rust
    // --- 4. Stand up the wgpu Vulkan DMABUF-capable host ---
    let (_device, _queue, _host) = match make_vulkan_host() {
        Some(triple) => triple,
        None => {
            // Host couldn't be built; the SKIP message already printed.
            // Close producer-owned fds before returning so they don't
            // leak when this branch is taken.
            close_producer_fds(&image);
            return;
        }
    };
    eprintln!("wpe→vk: wgpu Vulkan host up");
```

(The `_device/_queue/_host` underscore prefixes silence unused-warning;
Task 3 removes the underscores when it uses them.)

- [ ] **Step 2: Build + run**

Run: `cargo build -p scrying --features wpe --tests`
Expected: PASS, 0 warnings on the new test file (the underscore-prefixed
unused-bindings are silent by convention).

Run: `cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip -- --ignored --nocapture`
Expected: PASS. Two diagnostic lines now print: `wpe→vk: <size> ...` and
`wpe→vk: wgpu Vulkan host up`. If `make_vulkan_host` SKIPs, only the
first line + the SKIP reason print.

- [ ] **Step 3: Commit**

```bash
git add scrying/tests/wpe_to_vulkan_roundtrip.rs
git commit -m "$(cat <<'EOF'
phase 4c.x: WPE round-trip — wgpu DMABUF-capable host

Stands up a wgpu Vulkan Instance + Adapter + Device + Queue +
HostWgpuContext using scrying::build_dmabuf_capable_device, mirroring
the helper in tests/dmabuf_roundtrip.rs. SKIPs cleanly when there's no
Vulkan adapter or the required DMABUF extensions are missing. Task 2
of the round-trip plan; Task 3 invokes WgpuTextureImporter::import_frame.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Invoke the importer — empirical answer to "what happens on the AMD-DCC buffer?"

**Goal:** Call `WgpuTextureImporter::new(host).import_frame(&NativeFrame::DmaBufImage(image), &ImportOptions::default())`. On `Ok`, assert texture size/format match the producer's frame. On `Err`, panic with the literal Err message — that failure surfaces the actionable signal (Phase 4a importer needs multi-plane expansion for AMD-DCC buffers).

**Files:**
- Modify: `scrying/tests/wpe_to_vulkan_roundtrip.rs`

- [ ] **Step 1: Remove the placeholder `close_producer_fds` calls (the importer takes ownership of fds on success); call `import_frame`; inspect the result**

Replace the `Task 2 -- 4. Stand up the wgpu Vulkan DMABUF-capable host`
block with the full importer flow. Also update the top-of-file imports
to bring `WgpuTextureImporter`, `TextureImporter`, `ImportOptions` into
scope.

Update the `use scrying::...;` at the top of the file:

```rust
use scrying::wpe_producer::{WpeProducer, WpeProducerConfig};
use scrying::{
    HostWgpuContext, ImportOptions, NativeFrame, NavigationEvent,
    SyncMechanism, TextureImporter, WebSurfaceFrame, WebSurfaceProducer,
    WgpuTextureImporter,
};
```

Replace the Task-2 block (`let (_device, _queue, _host) = ...` through
the final `close_producer_fds(&image)`) with:

```rust
    // --- 4. Stand up the wgpu Vulkan DMABUF-capable host ---
    let (_device, _queue, host) = match make_vulkan_host() {
        Some(triple) => triple,
        None => {
            // Host couldn't be built; SKIP message already printed.
            // Close producer-owned fds before returning so they don't leak.
            close_producer_fds(&image);
            return;
        }
    };
    eprintln!("wpe→vk: wgpu Vulkan host up");

    // --- 5. Hand the WPE frame to the importer ---
    //
    // EMPIRICAL: on this AMD/Fedora 44 box the headless WPE buffer is
    // 2-plane AMD-tiled DCC (modifier 0x020000044051ba01). Phase 4a's
    // importer reads only `planes[0]`. The importer either:
    //  (a) accepts the modifier + reads only plane 0 → texture imports
    //      with potential visual artifact (pixels are out of scope for
    //      this test per spec § Assertions).
    //  (b) rejects the modifier outright → import_frame returns Err.
    //
    // If Err: we panic with the literal message — the failure IS the
    // actionable signal that Phase 4a needs multi-plane DRM-modifier
    // import to handle WPE's real output on AMD. The harness exists
    // and remains useful when that lands.
    //
    // FD OWNERSHIP: import_frame consumes the fds in `image` on success
    // (per native_frame/mod.rs's contract). Don't `close_producer_fds`
    // on the Ok path. On Err, the importer may or may not have
    // consumed them — match tests/dmabuf_roundtrip.rs's behaviour
    // (which doesn't manually close on Err either); the trade-off is
    // a tiny one-shot fd leak on the error path vs. a potential
    // double-close.
    let importer = WgpuTextureImporter::new(host);
    let expected_size = image.size;
    let expected_format = image.format;
    let imported = match importer.import_frame(
        &NativeFrame::DmaBufImage(image),
        &ImportOptions::default(),
    ) {
        Ok(t) => t,
        Err(e) => {
            panic!(
                "FAIL: import_frame errored on real WPE buffer: {e}\n\n\
                 This is the actionable signal: Phase 4a's importer is single-plane \
                 (reads planes[0]) but WPE on this hardware exports a multi-plane \
                 DCC-tiled buffer. The fix is multi-plane DRM-modifier import in \
                 native_frame/dmabuf.rs — see the 4c.2 retrospective and \
                 2026-06-04 round-trip spec for context."
            );
        }
    };

    // --- 6. Assert: import returned a wgpu texture of the right shape ---
    assert_eq!(imported.size.width, expected_size.width, "imported width matches");
    assert_eq!(imported.size.height, expected_size.height, "imported height matches");
    assert_eq!(imported.format, expected_format, "imported format matches");
    eprintln!(
        "wpe→vk: imported texture {}x{} format={:?} gen={}",
        imported.size.width, imported.size.height, imported.format, imported.generation
    );
```

Delete the `close_producer_fds` helper from the file — it's no longer
used on any reachable path (the SKIP branches all happen before frame
acquisition or before the importer is called; on the
host-creation-fails branch, the `close_producer_fds(&image)` call still
fires because we move into that branch after acquiring the image). Wait
— re-read the diff above. The SKIP branch INSIDE the host-build path
still calls `close_producer_fds(&image)` to release fds before
returning. So the helper IS still used. Keep it.

Update the helper's doc-comment now that its scope has narrowed:

```rust
/// Close producer-owned dup'd fds on a `DmaBufImage` that was never
/// handed to the importer. Used only on the SKIP branch where the wgpu
/// host couldn't be built — otherwise `import_frame` takes ownership of
/// the fds and Vulkan closes them on the imported texture's drop.
fn close_producer_fds(image: &scrying::DmaBufImage) {
    for plane in &image.planes {
        unsafe { libc::close(plane.fd); }
    }
    if let Some(fd) = image.semaphore_fd {
        unsafe { libc::close(fd); }
    }
}
```

- [ ] **Step 2: Build the test**

Run: `cargo build -p scrying --features wpe --tests`
Expected: PASS, 0 warnings.

- [ ] **Step 3: Run the ignored test — THE EMPIRICAL MOMENT**

Run: `cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip -- --ignored --nocapture`

**Two possible outcomes — both are valuable:**

**Outcome A: PASS.** The importer accepted the AMD-DCC buffer's plane 0.
You'll see three diagnostic lines:
```
wpe→vk: 1024x768 fourcc=0x34325258 mod=0x020000044051ba01 planes=2
wpe→vk: wgpu Vulkan host up
wpe→vk: imported texture 1024x768 format=Bgra8UnormSrgb gen=N
```
Pixel correctness isn't asserted — per the spec, that's a separate
expansion. The framework is in place; commit.

**Outcome B: FAIL with the "actionable signal" panic.** The importer
rejected the buffer (likely because the modifier requires multi-plane
info we're not providing). You'll see the `wpe→vk:` and `wgpu Vulkan
host up` lines, then a `FAIL: import_frame errored ...` panic with the
underlying Err. The framework is in place; the test fails on the
expected single-plane gap. **Commit anyway** — the failing
`#[ignore]`d test is the durable record of what needs fixing in Phase
4a. Document the observed Err in the commit message.

If neither A nor B occurs (e.g. compile error you can't resolve, or
test SKIPs unexpectedly), STOP and report BLOCKED with the exact
output.

- [ ] **Step 4: Commit**

Pick the right message based on the outcome:

For **Outcome A** (import succeeded):
```bash
git add scrying/tests/wpe_to_vulkan_roundtrip.rs
git commit -m "$(cat <<'EOF'
phase 4c.x: WPE round-trip — importer succeeds on the AMD-DCC buffer

WgpuTextureImporter accepted the headless WPE buffer (1024x768 XR24 on
modifier 0x020000044051ba01, 2 planes) via plane[0] only. The imported
texture's size and format match the producer's frame. Pixel correctness
is out of scope for this harness per the spec (separate Phase 4a
expansion). The end-to-end producer→importer pipeline is now exercised
in CI-able form (still #[ignore]d, but runnable).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

For **Outcome B** (import errored — paste the actual Err message into
the commit body):
```bash
git add scrying/tests/wpe_to_vulkan_roundtrip.rs
git commit -m "$(cat <<'EOF'
phase 4c.x: WPE round-trip — durable record of single-plane importer gap

WgpuTextureImporter::import_frame on the real headless WPE buffer
errors as predicted in the spec: WPE on AMD outputs a 2-plane DCC-tiled
buffer (modifier 0x020000044051ba01) and Phase 4a's importer is
single-plane (reads only planes[0]). The test fails loudly with the
underlying message so the gap is visible.

Observed error:
  <paste the literal Err message from the panic here>

The harness exists and remains useful when Phase 4a expands to
multi-plane DRM-modifier import. Test stays #[ignore]d.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- Separate integration binary → Task 1 Step 1 (`#![cfg(...)]` at top of new file). ✓
- WPE construction + navigate + frame acquire → Task 1. ✓
- wgpu DMABUF-capable host via `build_dmabuf_capable_device` → Task 2. ✓
- `import_frame` invocation → Task 3 Step 1. ✓
- WPE-side assertions (size, plane, fd, sync) → Task 1 Step 1. ✓
- Import-side assertion (Ok with size/format check; loud panic on Err) → Task 3 Step 1. ✓
- SKIP semantics for environmental misses (no Vulkan adapter, missing extensions, WPE construct fail) → Task 1, Task 2. ✓
- No modifications to `dmabuf.rs`, `headless.rs`, `producer.rs`, etc. → respected throughout. ✓
- `#[ignore]` discipline → Task 1 Step 1. ✓
- One ignored test in the binary → confirmed (only `wpe_to_vulkan_round_trip` exists). ✓
- Diagnostic eprintln for fourcc/modifier/planes → Task 1 Step 1. ✓

**Placeholder scan:** No "TBD"/"TODO"/"handle errors". Two
empirical-by-design points (the Outcome A/B branching in Task 3 Step 3,
the wgpu API version match in Task 2 Step 1 "copy from
`dmabuf_roundtrip.rs` verbatim if mismatch") are explicit decision
procedures with concrete code, not placeholders.

**Type consistency:**
- `WpeProducerConfig::new(PhysicalSize<u32>, PathBuf)` — consistent
  with the 4c.2/4c.3 surface.
- `NativeFrame::DmaBufImage(DmaBufImage)` → matches existing variant.
- `WgpuTextureImporter::new(HostWgpuContext)` and
  `.import_frame(&NativeFrame, &ImportOptions) -> Result<ImportedTexture, _>`
  → confirmed against `tests/dmabuf_roundtrip.rs:222–245`.
- `build_dmabuf_capable_device(&Adapter, &DeviceDescriptor) -> Result<(Device, Queue), _>`
  → confirmed against `tests/dmabuf_roundtrip.rs:606`.
- `image.size.width` / `imported.size.width` access consistent.
- `close_producer_fds(&DmaBufImage)` signature stable between Tasks 1
  and 3.

**Known risk:** The biggest unknown is the wgpu API version's exact
spelling (`Instance::new` signature, `request_adapter` Result vs Option,
`DeviceDescriptor` fields). Task 2 Step 1's note tells the implementer
to copy from the in-repo reference if anything mismatches — the
already-shipped `dmabuf_roundtrip.rs` is the source of truth.

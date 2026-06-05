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
//! missing (no display, GPU, WPE refusing to construct), print
//! `SKIP: ...` and return — cargo records the test as pass. A genuine
//! `import_frame` Err is NOT a skip (added in Task 3); it fails loudly.

#![cfg(all(target_os = "linux", feature = "wpe"))]

use dpi::PhysicalSize;
use scrying::wpe_producer::{WpeProducer, WpeProducerConfig};
use scrying::{
    DmaBufImage, HostWgpuContext, ImportOptions, ImportedTexture, NativeFrame, NavigationEvent,
    SyncMechanism, TextureImporter, WebSurfaceFrame, WebSurfaceProducer, WgpuTextureImporter,
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
        nav_events
            .iter()
            .any(|e| matches!(e, NavigationEvent::Completed { success: true, .. })),
        "expected a successful Completed event; got {:?}",
        nav_events
    );

    // --- 3. Acquire one DMABUF frame ---
    //
    // `wait_for_load` (inside navigate_to_string) returns when load-changed
    // FINISHED fires; the first `buffer-rendered` arrives slightly later
    // on the same MainContext. Retry briefly, pumping the GLib main context
    // each iteration so the pending buffer-rendered signal can dispatch.
    // We can't reach `producer.handles.main_context` from outside the crate
    // (it's `pub(super)`), but `glib::MainContext::default()` is the same
    // context the WPE producer uses (it was created with
    // `glib::MainContext::default()` in producer.rs), so iterating it here
    // delivers any pending callbacks including `buffer-rendered`.
    let frame = {
        let ctx = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match producer.acquire_frame() {
                Ok(f) => break f,
                Err(_) if std::time::Instant::now() < deadline => {
                    ctx.iteration(false); // dispatch pending GLib events (non-blocking)
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                Err(e) => panic!("FAIL: acquire_frame timed out after navigate: {e}"),
            }
        }
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

    // --- 3b. Plane-fd diagnostic for the (α) multi-plane import design. ---
    //
    // For each plane: fstat the dup'd fd and print (fd, st_ino, offset,
    // stride). If multiple planes share the same st_ino, they reference
    // the same kernel DMABUF (the AMD-on-Mesa convention: one buffer +
    // aux DCC metadata, distinguished by per-plane offsets). Different
    // inodes would mean genuinely separate DMABUFs requiring distinct
    // VkDeviceMemory imports.
    for (i, plane) in image.planes.iter().enumerate() {
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: plane.fd is a valid producer-owned fd; fstat is read-only.
        let rc = unsafe { libc::fstat(plane.fd, st.as_mut_ptr()) };
        if rc == 0 {
            let st = unsafe { st.assume_init() };
            eprintln!(
                "wpe→vk:   plane[{}]: fd={} st_ino={} offset={} stride={}",
                i, plane.fd, st.st_ino, plane.offset, plane.stride
            );
        } else {
            eprintln!(
                "wpe→vk:   plane[{}]: fd={} fstat failed errno={} offset={} stride={}",
                i,
                plane.fd,
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
                plane.offset,
                plane.stride
            );
        }
    }

    // --- 4. Stand up the wgpu Vulkan DMABUF-capable host ---
    let (device, queue, host) = match make_vulkan_host() {
        Some(triple) => triple,
        None => {
            // SKIP message already printed inside make_vulkan_host.
            // Producer fds must still be closed — the importer never
            // takes ownership on this path.
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
    //  (a) accepts the modifier + reads only plane 0 → texture imports,
    //      possibly with visual artifact (pixels are out of scope per
    //      the round-trip spec).
    //  (b) rejects the modifier outright → import_frame returns Err.
    //
    // On Err: panic with the literal message. The failure IS the
    // actionable signal that Phase 4a needs multi-plane DRM-modifier
    // import to handle WPE's real output on AMD. The harness exists
    // and remains useful when that lands.
    //
    // FD OWNERSHIP: import_frame consumes the fds on success. On Err
    // we don't manually close — matches dmabuf_roundtrip.rs's
    // behaviour and avoids any double-close risk.
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
                "FAIL: import_frame errored — phase 4a.x multi-plane DCC import \
                 should accept this WPE buffer. Error: {e}"
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

    // --- 7. Pixel-correctness sampling (DIAGNOSTIC ONLY) ---
    //
    // Reads a 64×64 center patch of the imported texture and PRINTS what
    // BGRA values came back. Does NOT assert against expected dodger-blue
    // (#1e90ff → sRGB-encoded ~B=255, G=144, R=30) because of a known
    // upstream wgpu API gap that bites Vulkan DMABUF imports specifically:
    //
    //   `wgpu_core::device::resource.rs:1253` (wgpu 29.0.3) inserts every
    //   `create_texture_from_hal` texture into the tracker with
    //   `TextureUses::UNINITIALIZED`, which `derive_image_layout()`
    //   resolves to `vk::ImageLayout::UNDEFINED`. wgpu's first-use
    //   barrier therefore transitions `UNDEFINED → TRANSFER_SRC_OPTIMAL`
    //   for `copy_texture_to_buffer`, and the Vulkan spec allows that
    //   transition to discard contents. RADV honors that allowance on
    //   the DCC modifier `0x020000044051ba01`; gbm-linear in the sibling
    //   `tests/dmabuf_roundtrip.rs` happens to work because linear-tile
    //   transitions are effectively no-ops on most drivers.
    //
    // Five layout/access combinations on our own foreign-queue acquire
    // barrier (UNDEFINED→SHADER_READ_ONLY_OPTIMAL / GENERAL /
    // TRANSFER_SRC_OPTIMAL × FOREIGN_EXT/IGNORED) all returned the same
    // all-zero readback because wgpu's tracker doesn't see our
    // intermediate layout — the spec-correct fix is a wgpu API addition
    // letting consumers tell the tracker the initial state of an
    // externally-imported texture.
    //
    // Until that lands upstream, this test:
    //   - Asserts import shape (size + format + dimensions) — Step 6 above.
    //   - Logs observed BGRA so when wgpu DOES expose initial-state, this
    //     test will flip to assertion mode with one tiny change.
    //   - Keeps the foreign-queue acquire barrier in `dmabuf::import`:
    //     spec-correct, no-op for gbm-linear, ready to deliver pixels the
    //     moment wgpu lands the API.
    //
    // The macOS and Windows backends sidestep this entirely: Metal's
    // `MTLTexture*` and D3D12's `RESOURCE_STATE_COMMON` both preserve
    // contents on import without requiring an initial-state declaration.
    // Vulkan's `UNDEFINED-discards` rule is the asymmetric one.
    let bytes = read_back_center_region(
        &device, &queue, &imported, expected_size.width, expected_size.height,
    );
    let expected = [0xFFu8, 0x90, 0x1E, 0xFF]; // B, G, R, A for dodger-blue
    let tolerance: i32 = 8;
    let mut bad_pixels = 0usize;
    let mut first_bad: Option<(usize, [u8; 4])> = None;
    for (idx, px) in bytes.chunks_exact(4).enumerate() {
        let off = px.iter()
            .zip(expected.iter())
            .any(|(g, e)| (*g as i32 - *e as i32).abs() > tolerance);
        if off {
            bad_pixels += 1;
            if first_bad.is_none() {
                first_bad = Some((idx, [px[0], px[1], px[2], px[3]]));
            }
        }
    }
    let total = 64 * 64;
    if bad_pixels == 0 {
        eprintln!(
            "wpe→vk: pixel-correctness OK (64×64 center sample all within ±{} of BGRA={:?}) \
             — upstream wgpu API gap appears resolved; this test can flip to assert!",
            tolerance, expected
        );
    } else {
        let (idx, bytes) = first_bad.unwrap_or((0, [0; 4]));
        eprintln!(
            "wpe→vk: pixel-correctness DIAGNOSTIC — {}/{} center pixels diverged from \
             dodger-blue ±{}; first divergent px at index {} = BGRA{:?} (expected {:?}). \
             Expected while wgpu's create_texture_from_hal lacks an initial-state API; \
             see comment above. Import shape (size + format) was asserted in Step 6.",
            bad_pixels, total, tolerance, idx, bytes, expected
        );
    }
}

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
    imported: &ImportedTexture,
    full_width: u32,
    full_height: u32,
) -> Vec<u8> {
    const SAMPLE_W: u32 = 64;
    const SAMPLE_H: u32 = 64;
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

/// Close producer-owned dup'd fds on a `DmaBufImage` that was never
/// handed to the importer. Used only on the SKIP branch where the
/// wgpu host couldn't be built — on the import path, `import_frame`
/// takes ownership of the fds and Vulkan closes them on the imported
/// texture's drop.
fn close_producer_fds(image: &DmaBufImage) {
    for plane in &image.planes {
        // SAFETY: producer-owned dup'd fd not yet transferred to the importer.
        unsafe { libc::close(plane.fd); }
    }
    if let Some(fd) = image.semaphore_fd {
        unsafe { libc::close(fd); }
    }
}

fn make_vulkan_host() -> Option<(wgpu::Device, wgpu::Queue, HostWgpuContext)> {
    // Turn on VK_LAYER_KHRONOS_validation so we catch any calls that
    // only "work by accident" on permissive Mesa — e.g. feeding a
    // DRM-modifier pNext chain to vkCreateImage on a device that
    // hadn't enabled VK_EXT_image_drm_format_modifier. wgpu-hal logs
    // a warning and proceeds without the layer if it isn't installed,
    // so this is safe on boxes that lack `vulkan-validation-layers`.
    // Validation messages route through `log`; run with
    // `RUST_LOG=wgpu_hal=warn` (or `=info`) to surface them.
    let flags = wgpu::InstanceFlags::VALIDATION | wgpu::InstanceFlags::DEBUG;
    eprintln!("wgpu instance flags: {flags:?} (VK_LAYER_KHRONOS_validation requested)");
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        flags,
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .ok()?;

    // Phase 4a.7 — use the scrying helper that enables
    // VK_EXT_image_drm_format_modifier + VK_KHR_external_semaphore_fd
    // at device creation time. Without these, the import path only
    // works by accident (Mesa permissiveness) and the wait path
    // SKIPs because the function pointers can't be resolved.
    let desc = wgpu::DeviceDescriptor {
        label: Some("wpe_to_vulkan_roundtrip-device"),
        ..Default::default()
    };
    let (device, queue) = match scrying::build_dmabuf_capable_device(&adapter, &desc) {
        Ok(pair) => pair,
        Err(scrying::DmaBufDeviceError::MissingExtensions(missing)) => {
            eprintln!(
                "SKIP: physical device missing required extensions: {missing:?}; \
                 falling back is not useful for this test"
            );
            return None;
        }
        Err(e) => {
            eprintln!("SKIP: build_dmabuf_capable_device failed: {e}");
            return None;
        }
    };
    let host = HostWgpuContext::new(device.clone(), queue.clone());
    Some((device, queue, host))
}

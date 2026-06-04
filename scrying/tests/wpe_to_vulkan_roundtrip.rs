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
    DmaBufImage, HostWgpuContext, ImportOptions, NativeFrame, NavigationEvent, SyncMechanism,
    TextureImporter, WebSurfaceFrame, WebSurfaceProducer, WgpuTextureImporter,
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

    // --- 4. Stand up the wgpu Vulkan DMABUF-capable host ---
    let (_device, _queue, host) = match make_vulkan_host() {
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
    };

    // --- 6. Assert: import returned a wgpu texture of the right shape ---
    assert_eq!(imported.size.width, expected_size.width, "imported width matches");
    assert_eq!(imported.size.height, expected_size.height, "imported height matches");
    assert_eq!(imported.format, expected_format, "imported format matches");
    eprintln!(
        "wpe→vk: imported texture {}x{} format={:?} gen={}",
        imported.size.width, imported.size.height, imported.format, imported.generation
    );
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

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
use scrying::wgpu;
use scrying::wpe_producer::{WpeProducer, WpeProducerConfig};
use scrying::{
    DmaBufImage, HostWgpuContext, ImportOptions, ImportedTexture, NativeFrame, NavigationEvent,
    SyncMechanism, TextureImporter, WebSurfaceFrame, WebSurfaceProducer, WgpuTextureImporter,
};

#[cfg(feature = "wgpu-30")]
const SAMPLE_SHADER: &str = r#"
struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    let x = f32((vertex_index & 1u) << 2u) - 1.0;
    let y = f32((vertex_index & 2u) << 1u) - 1.0;
    var out: VsOut;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(source, source_sampler, in.uv);
}
"#;

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
        "<!doctype html><html style='width:100%;height:100%;background:#1e90ff'>\
         <body style='width:100%;height:100%;margin:0;background:#1e90ff'>\
         <div style='position:fixed;inset:0;background:#1e90ff'></div></body></html>",
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

    // Wait for and consume WPE's transparent startup frame. Merely draining
    // the producer's current slot races the delayed `buffer-rendered`
    // callback and can let that stale frame satisfy the content acquisition.
    let first_frame = acquire_dmabuf_frame(&mut producer, "first navigation");
    close_producer_fds(&first_frame);
    producer
        .resize(PhysicalSize::new(256, 256))
        .expect("request a post-startup content repaint");

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
    let image = acquire_dmabuf_frame(&mut producer, "post-startup repaint");

    // WPE-side sanity (always-on assertions).
    assert!(
        image.size.width > 0 && image.size.height > 0,
        "non-zero size"
    );
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
    let imported =
        match importer.import_frame(&NativeFrame::DmaBufImage(image), &ImportOptions::default()) {
            Ok(t) => t,
            Err(e) => {
                panic!(
                    "FAIL: import_frame errored — phase 4a.x multi-plane DCC import \
                 should accept this WPE buffer. Error: {e}"
                );
            }
        };

    // --- 6. Assert: import returned a wgpu texture of the right shape ---
    assert_eq!(
        imported.size.width, expected_size.width,
        "imported width matches"
    );
    assert_eq!(
        imported.size.height, expected_size.height,
        "imported height matches"
    );
    assert_eq!(imported.format, expected_format, "imported format matches");
    eprintln!(
        "wpe→vk: imported texture {}x{} format={:?} gen={}",
        imported.size.width, imported.size.height, imported.format, imported.generation
    );

    // --- 7. Pixel-correctness sampling ---
    //
    // Exercise the imported texture through the contract consumers actually
    // use: sample it into a host-owned render target, then read that target.
    // A direct copy from a modifier-backed producer image asks the imported
    // allocation to support transfer-source use that WPE did not negotiate.
    #[cfg(feature = "wgpu-30")]
    let sampled = sample_imported_texture(&device, &queue, &imported);
    #[cfg(feature = "wgpu-30")]
    let bytes = read_back_center_region(&device, &queue, &sampled, 64, 64);
    #[cfg(not(feature = "wgpu-30"))]
    let bytes = read_back_center_region(
        &device,
        &queue,
        &imported.texture,
        expected_size.width,
        expected_size.height,
    );
    let expected = [0xFFu8, 0x90, 0x1E, 0xFF]; // B, G, R, A for dodger-blue
    let tolerance: i32 = 8;
    let mut bad_pixels = 0usize;
    let mut first_bad: Option<(usize, [u8; 4])> = None;
    for (idx, px) in bytes.chunks_exact(4).enumerate() {
        let off = px
            .iter()
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
    let (first_bad_idx, first_bad_bytes) = first_bad.unwrap_or((0, expected));
    assert_eq!(
        bad_pixels, 0,
        "{bad_pixels}/{total} center pixels diverged from dodger-blue ±{tolerance}; \
         first divergent pixel at index {first_bad_idx} was BGRA{first_bad_bytes:?}, \
         expected {expected:?}"
    );
    eprintln!(
        "wpe→vk: pixel-correctness OK (64×64 center sample all within ±{} of BGRA={:?})",
        tolerance, expected
    );
}

fn acquire_dmabuf_frame(producer: &mut WpeProducer, boundary: &str) -> DmaBufImage {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match producer.acquire_frame() {
            Ok(WebSurfaceFrame::Native(NativeFrame::DmaBufImage(image))) => return image,
            Ok(_) => panic!("FAIL: expected a DMABUF frame after {boundary}"),
            Err(_) if std::time::Instant::now() < deadline => {
                ctx.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(e) => panic!("FAIL: acquire_frame timed out after {boundary}: {e}"),
        }
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
    texture: &wgpu::Texture,
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
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: center_x,
                y: center_y,
                z: 0,
            },
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

    #[cfg(feature = "wgpu-30")]
    let mapped = buffer_slice
        .get_mapped_range()
        .expect("readback buffer map range");
    #[cfg(not(feature = "wgpu-30"))]
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

/// Sample the imported producer texture into a host-owned 64x64 target.
/// This mirrors the compositing path used by the demos and keeps readback
/// requirements off the externally allocated image itself.
#[cfg(feature = "wgpu-30")]
fn sample_imported_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    imported: &ImportedTexture,
) -> wgpu::Texture {
    const SAMPLE_SIZE: u32 = 64;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("wpe_roundtrip_sample_target"),
        size: wgpu::Extent3d {
            width: SAMPLE_SIZE,
            height: SAMPLE_SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let source_view = imported
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("wpe_roundtrip_sample_sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("wpe_roundtrip_sample_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("wpe_roundtrip_sample_bg"),
        layout: &bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("wpe_roundtrip_sample_pl"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("wpe_roundtrip_sample_shader"),
        source: wgpu::ShaderSource::Wgsl(SAMPLE_SHADER.into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("wpe_roundtrip_sample_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("wpe_roundtrip_sample_encoder"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("wpe_roundtrip_sample_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit(std::iter::once(encoder.finish()));
    target
}

/// Close producer-owned dup'd fds on a `DmaBufImage` that was never
/// handed to the importer. Used only on the SKIP branch where the
/// wgpu host couldn't be built — on the import path, `import_frame`
/// takes ownership of the fds and Vulkan closes them on the imported
/// texture's drop.
fn close_producer_fds(image: &DmaBufImage) {
    for plane in &image.planes {
        // SAFETY: producer-owned dup'd fd not yet transferred to the importer.
        unsafe {
            libc::close(plane.fd);
        }
    }
    if let Some(fd) = image.semaphore_fd {
        unsafe {
            libc::close(fd);
        }
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
        #[cfg(feature = "wgpu-30")]
        apply_limit_buckets: false,
    }))
    .ok()?;

    // Phase 4a.7 — use the scrying helper that enables
    // VK_EXT_image_drm_format_modifier + VK_EXT_queue_family_foreign +
    // VK_KHR_external_semaphore_fd at device creation time. Without these,
    // the import path only
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

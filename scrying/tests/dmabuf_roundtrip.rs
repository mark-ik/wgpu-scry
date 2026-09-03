// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Phase 4a.4 round-trip integration test:
//!
//! 1. Open `/dev/dri/renderD128`.
//! 2. Allocate a 256×256 ARGB8888 DMABUF via libgbm with the LINEAR
//!    modifier (so the byte layout is predictable + the stride is
//!    just `width * 4`).
//! 3. Write a known checkerboard pattern into the BO via `write()`.
//! 4. Build a [`scrying::DmaBufImage`] around the DMABUF fd, stride,
//!    modifier.
//! 5. Stand up a wgpu Vulkan device on this host and feed the frame
//!    through [`scrying::WgpuTextureImporter::import_frame`].
//! 6. Copy the imported texture into a CPU-mapped readback buffer.
//! 7. Assert the readback bytes match the original pattern.
//!
//! Skipped (test exits PASS without asserting) when this box can't
//! satisfy the prerequisites:
//! - no readable render node (CI runners without a DRM device)
//! - wgpu can't acquire a Vulkan adapter
//! - the chosen Vulkan device is missing the required DMABUF
//!   extensions
//!
//! Phase 4a.6 sibling test (`dmabuf_import_with_signaled_semaphore`)
//! reuses the same fixture but additionally creates an exportable
//! binary `VkSemaphore` on the consumer's wgpu Vulkan device, signals
//! it via a signal-only `vkQueueSubmit`, exports the `OPAQUE_FD`, and
//! threads the fd through the [`scrying::DmaBufImage`] so
//! `import_frame` walks the Phase 4a.2 wait path. Since the semaphore
//! is already signaled when import sees it, the internal
//! `vkQueueWaitIdle` returns immediately — this validates the FFI
//! wiring (proc-addr lookup, semaphore import, wait-only submit,
//! drain, cleanup), not actual GPU-side ordering (which would need
//! a producer-driven render).
//!
//! Run with `cargo test --manifest-path scrying/Cargo.toml --test dmabuf_roundtrip -- --nocapture`
//! to see the diagnostic print statements.

#![cfg(target_os = "linux")]

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::mem;
use std::os::fd::IntoRawFd;

use ash::vk;
use dpi::PhysicalSize;
use scrying::wgpu;
use scrying::{
    DmaBufImage, DmaBufPlane, HostWgpuContext, ImportOptions, ImportedTexture, NativeFrame,
    SyncMechanism, TextureImporter, WgpuTextureImporter,
};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 256;
const DRM_FORMAT_ARGB8888: u32 = 0x34325241; // 'AR24'
/// linux/drm_fourcc.h `DRM_FORMAT_MOD_INVALID` — the producer didn't
/// negotiate an explicit modifier. Phase 4a.5 treats this as linear.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Mirror image of the import path's format choice: wgpu BGRA8 maps
/// to DRM_FORMAT_ARGB8888 on little-endian (bytes in memory: B G R A).
const WGPU_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// Shared producer-side state: a wgpu Vulkan host + a filled DMABUF
/// + its layout. Both round-trip tests want the same fixture before
/// diverging on how the `DmaBufImage` is composed.
struct ProducerFixture {
    device: wgpu::Device,
    queue: wgpu::Queue,
    host: HostWgpuContext,
    pattern: Vec<u8>,
    dmabuf_fd: i32,
    stride: u32,
    offset: u32,
    drm_modifier: u64,
    // Keep the gbm BufferObject alive for the duration of the test.
    // Dropping it before the importer transfers fd ownership to the
    // driver is technically fine (DMABUF fds hold their own ref to
    // the underlying buffer), but holding it matches what a real
    // producer would do.
    _bo: gbm::BufferObject<()>,
}

fn checkerboard_pattern() -> Vec<u8> {
    let mut bytes = vec![0u8; (WIDTH as usize) * (HEIGHT as usize) * 4];
    for y in 0..HEIGHT as usize {
        for x in 0..WIDTH as usize {
            let idx = (y * WIDTH as usize + x) * 4;
            let on_white = ((x / 16) + (y / 16)) % 2 == 0;
            // Two distinguishable colors: scrying-navy (#0F172A) and
            // scrying-yellow (#FACC15), written in BGRA byte order.
            if on_white {
                // Yellow: R=0xFA G=0xCC B=0x15 A=0xFF
                bytes[idx + 0] = 0x15; // B
                bytes[idx + 1] = 0xCC; // G
                bytes[idx + 2] = 0xFA; // R
                bytes[idx + 3] = 0xFF; // A
            } else {
                // Navy:   R=0x0F G=0x17 B=0x2A A=0xFF
                bytes[idx + 0] = 0x2A; // B
                bytes[idx + 1] = 0x17; // G
                bytes[idx + 2] = 0x0F; // R
                bytes[idx + 3] = 0xFF; // A
            }
        }
    }
    bytes
}

/// Open a render node, allocate + fill a DMABUF, and stand up a
/// wgpu Vulkan host on this box. Returns `None` (and prints a
/// `SKIP:` line) when any prerequisite isn't met — callers should
/// then early-return so cargo records the test as a pass without
/// running the assertion phase.
fn setup_producer() -> Option<ProducerFixture> {
    let render_node_path = "/dev/dri/renderD128";
    let render_node = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(render_node_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("SKIP: cannot open {render_node_path}: {e}");
            return None;
        }
    };

    let gbm_device = match gbm::Device::new(render_node) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: gbm::Device::new failed: {e}");
            return None;
        }
    };

    // LINEAR modifier so the on-disk byte layout matches our writes
    // and stride == width * 4 with high probability.
    let modifier = gbm::Modifier::Linear;
    let bo: gbm::BufferObject<()> = match gbm_device.create_buffer_object_with_modifiers::<()>(
        WIDTH,
        HEIGHT,
        gbm::Format::Argb8888,
        std::iter::once(modifier),
    ) {
        Ok(bo) => bo,
        Err(e) => {
            eprintln!("SKIP: gbm BO allocation failed: {e}");
            return None;
        }
    };

    let pattern = checkerboard_pattern();
    let stride = bo.stride();
    eprintln!("gbm BO: {WIDTH}x{HEIGHT} stride={stride} modifier={modifier:?}");
    let mut bo = bo;
    let stride_usize = stride as usize;
    let row_bytes = (WIDTH as usize) * 4;
    let map_result = bo.map_mut(0, 0, WIDTH, HEIGHT, |mapped| {
        let dst = mapped.buffer_mut();
        for y in 0..HEIGHT as usize {
            let dst_row = &mut dst[y * stride_usize..y * stride_usize + row_bytes];
            let src_row = &pattern[y * row_bytes..(y + 1) * row_bytes];
            dst_row.copy_from_slice(src_row);
        }
    });
    if let Err(e) = map_result {
        eprintln!("SKIP: gbm_bo_map failed: {e}");
        return None;
    }

    let dmabuf_fd = match bo.fd() {
        Ok(fd) => fd.into_raw_fd(),
        Err(e) => {
            eprintln!("SKIP: gbm_bo_get_fd failed: {e:?}");
            return None;
        }
    };
    let drm_modifier: u64 = u64::from(bo.modifier());
    let offset = bo.offset(0);

    let (device, queue, host) = match make_vulkan_host() {
        Some(h) => h,
        None => {
            eprintln!("SKIP: no Vulkan wgpu adapter available");
            return None;
        }
    };

    Some(ProducerFixture {
        device,
        queue,
        host,
        pattern,
        dmabuf_fd,
        stride,
        offset,
        drm_modifier,
        _bo: bo,
    })
}

#[test]
fn dmabuf_import_round_trip() {
    let Some(fixture) = setup_producer() else {
        return;
    };

    let frame = DmaBufImage {
        size: PhysicalSize::new(WIDTH, HEIGHT),
        format: WGPU_FORMAT,
        drm_format: DRM_FORMAT_ARGB8888,
        drm_modifier: fixture.drm_modifier,
        planes: vec![DmaBufPlane {
            fd: fixture.dmabuf_fd,
            offset: fixture.offset,
            stride: fixture.stride,
        }],
        generation: 1,
        producer_sync: SyncMechanism::None,
        semaphore_fd: None,
    };

    let importer = WgpuTextureImporter::new(fixture.host);
    let imported =
        match importer.import_frame(&NativeFrame::DmaBufImage(frame), &ImportOptions::default()) {
            Ok(t) => t,
            Err(e) => {
                panic!("FAIL: import_frame errored: {e}");
            }
        };
    assert_eq!(imported.size.width, WIDTH);
    assert_eq!(imported.size.height, HEIGHT);
    assert_eq!(imported.format, WGPU_FORMAT);
    eprintln!(
        "imported texture: {}x{} format={:?} gen={}",
        imported.size.width, imported.size.height, imported.format, imported.generation
    );

    let readback_bytes = read_back_texture(&fixture.device, &fixture.queue, &imported);
    assert_pixels_match(&fixture.pattern, &readback_bytes);
}

/// Phase 4a.6: extends the round-trip with an explicit external
/// semaphore so the importer walks `wait_on_producer_semaphore`.
///
/// The semaphore is created on the consumer's wgpu Vulkan device,
/// signaled via a signal-only `vkQueueSubmit`, drained with
/// `vkQueueWaitIdle`, then exported as `OPAQUE_FD`. The exported fd
/// represents a payload that's already in the signaled state, so the
/// importer's wait completes immediately — this is enough to exercise
/// the wiring without standing up a separate producer device.
///
/// SKIPs cleanly when the consumer's wgpu Vulkan device doesn't
/// expose `vkGetSemaphoreFdKHR` (i.e. `VK_KHR_external_semaphore_fd`
/// wasn't enabled at device creation). wgpu-hal 29 doesn't enable
/// that extension by default; some Mesa drivers return the function
/// pointer anyway under permissive loader behaviour, but strict
/// validation would reject. A follow-up task tracks adding device
/// creation through a wgpu-hal escape hatch with the extension
/// explicitly enabled.
#[test]
fn dmabuf_import_with_signaled_semaphore() {
    let Some(fixture) = setup_producer() else {
        return;
    };

    let semaphore_fd = match create_and_signal_exportable_semaphore(&fixture.device, &fixture.queue)
    {
        Ok(fd) => fd,
        Err(SemaphoreSetupError::Skip(msg)) => {
            eprintln!("SKIP: {msg}");
            return;
        }
        Err(SemaphoreSetupError::Fail(msg)) => {
            panic!("FAIL: {msg}");
        }
    };
    eprintln!("exported producer semaphore fd={semaphore_fd}");

    let frame = DmaBufImage {
        size: PhysicalSize::new(WIDTH, HEIGHT),
        format: WGPU_FORMAT,
        drm_format: DRM_FORMAT_ARGB8888,
        drm_modifier: fixture.drm_modifier,
        planes: vec![DmaBufPlane {
            fd: fixture.dmabuf_fd,
            offset: fixture.offset,
            stride: fixture.stride,
        }],
        generation: 2,
        producer_sync: SyncMechanism::ExplicitExternalSemaphore,
        semaphore_fd: Some(semaphore_fd),
    };

    let importer = WgpuTextureImporter::new(fixture.host);
    let imported =
        match importer.import_frame(&NativeFrame::DmaBufImage(frame), &ImportOptions::default()) {
            Ok(t) => t,
            Err(e) => {
                panic!("FAIL: import_frame (with signaled semaphore) errored: {e}");
            }
        };
    assert_eq!(imported.size.width, WIDTH);
    assert_eq!(imported.size.height, HEIGHT);
    assert_eq!(imported.format, WGPU_FORMAT);
    eprintln!(
        "imported texture (semaphore path): {}x{} format={:?} gen={}",
        imported.size.width, imported.size.height, imported.format, imported.generation
    );

    let readback_bytes = read_back_texture(&fixture.device, &fixture.queue, &imported);
    assert_pixels_match(&fixture.pattern, &readback_bytes);
}

/// Phase 4a.5: a producer that couldn't negotiate an explicit DRM
/// modifier hands us `DRM_FORMAT_MOD_INVALID`. The fixture's BO is
/// genuinely linear (allocated with the LINEAR modifier), so the
/// importer's INVALID→LINEAR substitution should round-trip
/// bit-for-bit. This is the common WebKit fallback case when EGL/
/// Vulkan modifier negotiation isn't available.
#[test]
fn dmabuf_import_implicit_modifier() {
    let Some(fixture) = setup_producer() else {
        return;
    };

    let frame = DmaBufImage {
        size: PhysicalSize::new(WIDTH, HEIGHT),
        format: WGPU_FORMAT,
        drm_format: DRM_FORMAT_ARGB8888,
        drm_modifier: DRM_FORMAT_MOD_INVALID,
        planes: vec![DmaBufPlane {
            fd: fixture.dmabuf_fd,
            offset: fixture.offset,
            stride: fixture.stride,
        }],
        generation: 3,
        producer_sync: SyncMechanism::None,
        semaphore_fd: None,
    };

    let importer = WgpuTextureImporter::new(fixture.host);
    let imported =
        match importer.import_frame(&NativeFrame::DmaBufImage(frame), &ImportOptions::default()) {
            Ok(t) => t,
            Err(e) => {
                panic!("FAIL: import_frame (implicit modifier) errored: {e}");
            }
        };
    eprintln!(
        "imported texture (implicit modifier): {}x{} format={:?} gen={}",
        imported.size.width, imported.size.height, imported.format, imported.generation
    );

    let readback_bytes = read_back_texture(&fixture.device, &fixture.queue, &imported);
    assert_pixels_match(&fixture.pattern, &readback_bytes);
}

enum SemaphoreSetupError {
    /// Driver / wgpu combination doesn't support the export path —
    /// report as SKIP so cargo records the test as a pass.
    Skip(String),
    /// Something we expected to work blew up — fail the test.
    Fail(String),
}

type PfnGetSemaphoreFd =
    unsafe extern "system" fn(vk::Device, *const c_void, *mut i32) -> vk::Result;

/// Create an exportable binary semaphore on `device`'s Vulkan handle,
/// signal it via a signal-only `vkQueueSubmit` on `queue`'s Vulkan
/// handle, drain to ensure the signal has completed, then export the
/// payload as `OPAQUE_FD`. Returns the fd ready to be embedded in a
/// `DmaBufImage`.
fn create_and_signal_exportable_semaphore(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> Result<i32, SemaphoreSetupError> {
    use wgpu::wgc::api::Vulkan;

    unsafe {
        let hal_device = device.as_hal::<Vulkan>().ok_or_else(|| {
            SemaphoreSetupError::Fail("device.as_hal::<Vulkan>() returned None".into())
        })?;
        let raw_device: &ash::Device = hal_device.raw_device();

        // Walk wgpu-hal's InstanceShared to reach the ash::Instance
        // wgpu created at adapter time. vkGetDeviceProcAddr is the
        // documented bootstrap for device-level extension functions
        // — and it requires a real vk::Instance handle per Vulkan
        // 1.2+ (a NULL instance returns null for everything except
        // the four global commands). Bypassing wgpu-hal's instance
        // is what the previous ash::Entry::load() attempt got wrong.
        let ash_instance: &ash::Instance = hal_device.shared_instance().raw_instance();

        let raw_get_fd =
            ash_instance.get_device_proc_addr(raw_device.handle(), c"vkGetSemaphoreFdKHR".as_ptr());
        let get_semaphore_fd: PfnGetSemaphoreFd = match raw_get_fd {
            Some(p) => mem::transmute_copy(&p),
            None => {
                return Err(SemaphoreSetupError::Skip(
                    "vkGetDeviceProcAddr(vkGetSemaphoreFdKHR) returned null — \
                     VK_KHR_external_semaphore_fd not enabled on the wgpu Vulkan device"
                        .into(),
                ));
            }
        };

        let mut export_info = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
        let vk_semaphore = raw_device
            .create_semaphore(&create_info, None)
            .map_err(|e| {
                SemaphoreSetupError::Skip(format!(
                    "vkCreateSemaphore(exportable): {e} — likely the device wasn't \
                 created with VK_KHR_external_semaphore[_fd] enabled"
                ))
            })?;

        // Signal-only submit.
        let queue_guard = queue.as_hal::<Vulkan>().ok_or_else(|| {
            raw_device.destroy_semaphore(vk_semaphore, None);
            SemaphoreSetupError::Fail("queue.as_hal::<Vulkan>() returned None".into())
        })?;
        let vk_queue = queue_guard.as_raw();

        let signal_semaphores = [vk_semaphore];
        let submit = vk::SubmitInfo::default().signal_semaphores(&signal_semaphores);
        if let Err(e) = raw_device.queue_submit(vk_queue, &[submit], vk::Fence::null()) {
            raw_device.destroy_semaphore(vk_semaphore, None);
            return Err(SemaphoreSetupError::Fail(format!(
                "queue_submit(signal): {e}"
            )));
        }

        // Drain so the signal is observable before we export.
        if let Err(e) = raw_device.queue_wait_idle(vk_queue) {
            raw_device.destroy_semaphore(vk_semaphore, None);
            return Err(SemaphoreSetupError::Fail(format!("queue_wait_idle: {e}")));
        }
        drop(queue_guard);

        // Export OPAQUE_FD. The fd is a self-contained payload
        // reference — the importer's vkImportSemaphoreFdKHR transfers
        // ownership of the fd to the driver on success.
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(vk_semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        let mut fd: i32 = -1;
        let result = get_semaphore_fd(
            raw_device.handle(),
            &info as *const _ as *const c_void,
            &mut fd,
        );
        // Per the spec, the original VkSemaphore can be destroyed
        // after a successful export — the fd outlives it.
        raw_device.destroy_semaphore(vk_semaphore, None);
        if result != vk::Result::SUCCESS {
            return Err(SemaphoreSetupError::Skip(format!(
                "vkGetSemaphoreFdKHR returned {result:?}"
            )));
        }
        if fd < 0 {
            return Err(SemaphoreSetupError::Fail(format!(
                "vkGetSemaphoreFdKHR returned invalid fd {fd}"
            )));
        }

        Ok(fd)
    }
}

fn read_back_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    imported: &ImportedTexture,
) -> Vec<u8> {
    let bytes_per_row = align_up(WIDTH * 4, 256);
    let readback_size = (bytes_per_row as u64) * (HEIGHT as u64);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("scrying-dmabuf-roundtrip-readback"),
        size: readback_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("scrying-dmabuf-roundtrip-encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &imported.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(HEIGHT),
            },
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
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
    let mut bytes = Vec::with_capacity((WIDTH as usize) * (HEIGHT as usize) * 4);
    for row in 0..HEIGHT as usize {
        let row_start = row * (bytes_per_row as usize);
        let row_end = row_start + (WIDTH as usize) * 4;
        bytes.extend_from_slice(&mapped[row_start..row_end]);
    }
    drop(mapped);
    readback.unmap();
    bytes
}

fn assert_pixels_match(want: &[u8], got: &[u8]) {
    let mut mismatches = 0usize;
    let mut first_mismatch: Option<(usize, [u8; 4], [u8; 4])> = None;
    for i in 0..want.len() / 4 {
        let want_px = [
            want[i * 4],
            want[i * 4 + 1],
            want[i * 4 + 2],
            want[i * 4 + 3],
        ];
        let got_px = [got[i * 4], got[i * 4 + 1], got[i * 4 + 2], got[i * 4 + 3]];
        if want_px != got_px {
            mismatches += 1;
            if first_mismatch.is_none() {
                first_mismatch = Some((i, want_px, got_px));
            }
        }
    }

    if mismatches == 0 {
        eprintln!("PASS: {} pixels matched", want.len() / 4);
    } else {
        let (idx, want_px, got_px) = first_mismatch.unwrap();
        let x = idx % WIDTH as usize;
        let y = idx / WIDTH as usize;
        panic!(
            "FAIL: {mismatches} / {} pixels differ; first mismatch at ({x}, {y}): expected {want_px:02x?}, got {got_px:02x?}",
            want.len() / 4
        );
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
    // VK_EXT_image_drm_format_modifier + VK_KHR_external_semaphore_fd
    // at device creation time. Without these, the import path only
    // works by accident (Mesa permissiveness) and the wait path
    // SKIPs because the function pointers can't be resolved.
    let desc = wgpu::DeviceDescriptor {
        label: Some("scrying-dmabuf-roundtrip-device"),
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

fn align_up(value: u32, alignment: u32) -> u32 {
    (value + alignment - 1) & !(alignment - 1)
}

//! Linux DMABUF → wgpu Vulkan texture import.
//!
//! Phase 4a slice 1 — single-plane DMABUF import through wgpu-hal's
//! Vulkan escape hatch. Uses three Vulkan extensions, all of which
//! Mesa + the Renoir / Intel / NVIDIA-current Linux drivers support
//! out of the box:
//!
//! - `VK_KHR_external_memory_fd` — imports the DMABUF file descriptor
//!   as a `VkDeviceMemory` backing.
//! - `VK_EXT_image_drm_format_modifier` — creates a `VkImage` whose
//!   tiling matches the DMABUF's DRM format modifier so the consumer's
//!   sampling pipeline understands the producer's memory layout.
//! - `VK_KHR_external_memory` — the umbrella extension implied by the
//!   above two.
//!
//! The function-pointer side is intentionally minimal — `vkAllocateMemory`
//! accepts a chained `VkImportMemoryFdInfoKHR` to do the fd import in
//! the standard allocation path, so we don't load any KHR loader
//! structs. `vkGetMemoryFdPropertiesKHR` (which would refine the memory
//! type selection) is also skipped — we pick from the image's
//! `memory_type_bits` directly and rely on `vkAllocateMemory` to
//! reject the choice if it's fd-incompatible.
//!
//! Multi-plane DMABUFs (NV12 / P010 / YUV420) and the
//! `DRM_FORMAT_MOD_INVALID` implicit-modifier path are deferred to
//! Phase 4a.5. The Phase 4a.2 `VK_KHR_external_semaphore_fd` import for
//! the producer-sync semaphore is wired here: when `frame.semaphore_fd`
//! is `Some`, the importer loads `vkImportSemaphoreFdKHR` via
//! `vkGetDeviceProcAddr` (no ash high-level wrapper needed), imports
//! the semaphore, submits a wait-only `vkQueueSubmit` directly on the
//! consumer queue's `vk::Queue` (reached via
//! `wgpu::Queue::as_hal::<Vulkan>().as_raw()`), drains via
//! `vkQueueWaitIdle`, and destroys the semaphore. The CPU-side drain
//! is pessimistic — a future iteration can defer cleanup via a fence
//! ring so the consumer can issue work concurrently with the
//! producer's render.

#![cfg(target_os = "linux")]

use std::ffi::{CStr, c_void};
use std::mem;

use ash::vk;
use wgpu::wgc::api::Vulkan;

use super::{
    DmaBufImage, DmaBufPlane, HostWgpuContext, ImportedTexture, InteropError, SyncMechanism,
    UnsupportedReason,
};

/// linux/drm_fourcc.h `DRM_FORMAT_MOD_LINEAR` — row-major, no tiling.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// linux/drm_fourcc.h `DRM_FORMAT_MOD_INVALID` = `fourcc_mod_code(NONE,
/// (1<<56)-1)`. Signals "no explicit modifier negotiated".
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

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

pub(super) fn import(
    frame: &DmaBufImage,
    host: &HostWgpuContext,
) -> Result<ImportedTexture, InteropError> {
    if host.backend != super::InteropBackend::Vulkan {
        return Err(InteropError::BackendMismatch {
            expected: "Vulkan",
            actual: "non-Vulkan",
        });
    }
    if frame.planes.is_empty() {
        return Err(InteropError::InvalidFrame("DmaBufImage has no planes"));
    }
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

    let vk_format = vk_format_from_dmabuf(frame).ok_or_else(|| {
        InteropError::Vulkan(format!(
            "unsupported DRM fourcc {:#010x} (wgpu format {:?})",
            frame.drm_format, frame.format
        ))
    })?;

    // Phase 4a.5 — implicit-modifier handling. A producer that
    // couldn't negotiate an explicit DRM modifier hands us
    // `DRM_FORMAT_MOD_INVALID`. We can't feed that to
    // `ImageDrmFormatModifierExplicitCreateInfoEXT` (it isn't a real
    // modifier — the driver produces an undefined layout and reads
    // back garbage). Substitute `DRM_FORMAT_MOD_LINEAR`, which lets us
    // keep the explicit-modifier path (and its stride control) while
    // treating the buffer as the linear layout that implicit-modifier
    // producers — CPU producers and simple GBM allocations, including
    // WebKit's fallback when EGL/Vulkan modifier negotiation isn't
    // available — actually emit. GPU-tiled implicit buffers can't be
    // imported this way; the producer must negotiate an explicit
    // modifier (a fundamental DMABUF interop constraint, not a
    // limitation we can paper over here).
    let effective_modifier = if frame.drm_modifier == DRM_FORMAT_MOD_INVALID {
        DRM_FORMAT_MOD_LINEAR
    } else {
        frame.drm_modifier
    };

    let width = frame.size.width;
    let height = frame.size.height;

    let texture = unsafe {
        let hal_device = host
            .device
            .as_hal::<Vulkan>()
            .ok_or(InteropError::BackendMismatch {
                expected: "Vulkan",
                actual: "non-Vulkan",
            })?;
        let raw_device: &ash::Device = hal_device.raw_device();

        // ---- 1. Build VkImage with DMABUF + DRM-modifier chain ----
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

        let mut drm_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(effective_modifier)
            .plane_layouts(&plane_layouts);

        let mut external_memory_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_memory_info)
            .push_next(&mut drm_modifier_info);

        let vk_image = raw_device
            .create_image(&image_info, None)
            .map_err(|e| InteropError::Vulkan(format!("vkCreateImage: {e}")))?;

        // ---- 2. Memory requirements + memory-type selection ----
        let mem_reqs = raw_device.get_image_memory_requirements(vk_image);
        let memory_type_index = first_set_bit(mem_reqs.memory_type_bits).ok_or_else(|| {
            raw_device.destroy_image(vk_image, None);
            InteropError::Vulkan(
                "vkGetImageMemoryRequirements reported no compatible memory types".into(),
            )
        })?;

        // ---- 3. Allocate VkDeviceMemory importing the DMABUF fd ----
        // Per the Vulkan spec, ownership of `fd` transfers to the
        // driver on success — we must NOT close it ourselves
        // afterwards. On failure, ownership stays with the caller,
        // but the `DmaBufImage` is consumed by the producer's frame
        // path already, so we don't try to recover the fd.
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(frame.planes[0].fd);
        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(vk_image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut dedicated_info)
            .push_next(&mut import_info);

        let vk_memory = match raw_device.allocate_memory(&alloc_info, None) {
            Ok(m) => m,
            Err(e) => {
                raw_device.destroy_image(vk_image, None);
                return Err(InteropError::Vulkan(format!("vkAllocateMemory: {e}")));
            }
        };

        if let Err(e) = raw_device.bind_image_memory(vk_image, vk_memory, 0) {
            raw_device.free_memory(vk_memory, None);
            raw_device.destroy_image(vk_image, None);
            return Err(InteropError::Vulkan(format!("vkBindImageMemory: {e}")));
        }

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
            // underlying buffer. (Already inside the outer `unsafe`
            // block opened at the top of the import body.)
            libc::close(plane.fd);
        }

        // ---- 3b. Foreign-queue ownership-acquire barrier ----
        //
        // Per the Vulkan spec for VK_EXT_image_drm_format_modifier /
        // external-handle-type imports, the imported VkImage's contents
        // are owned by `VK_QUEUE_FAMILY_FOREIGN_EXT` (the producer —
        // Mesa-RADV's compositor / WPE). Without an explicit ownership
        // acquire to *our* queue family, wgpu's first-use layout
        // transition (UNDEFINED → whatever) is allowed to discard the
        // producer's content per the "transition from UNDEFINED may
        // discard" rule, and RADV strictly enforces this — the imported
        // texture samples as all-zero.
        //
        // The fix: record a one-shot UNDEFINED → SHADER_READ_ONLY_OPTIMAL
        // transition with src=FOREIGN, dst=our_family. This both
        // (a) acquires ownership from the producer (preserving content)
        // and (b) advances the layout off UNDEFINED so wgpu's first-use
        // barrier transitions from a defined layout (preserving content).
        //
        // gbm-linear DMABUFs happen to work without this on most drivers
        // because linear-tiled transitions are effectively no-ops; we
        // still emit the barrier unconditionally — it's correct for both
        // shapes per the spec.
        let queue_family_index = hal_device.queue_family_index();
        let vk_queue = hal_device.raw_queue();

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::TRANSIENT);
        let cmd_pool = raw_device.create_command_pool(&pool_info, None).map_err(|e| {
            raw_device.free_memory(vk_memory, None);
            raw_device.destroy_image(vk_image, None);
            InteropError::Vulkan(format!("vkCreateCommandPool (acquire barrier): {e}"))
        })?;

        let cmd_alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cmd_buf = match raw_device.allocate_command_buffers(&cmd_alloc_info) {
            Ok(v) => v[0],
            Err(e) => {
                raw_device.destroy_command_pool(cmd_pool, None);
                raw_device.free_memory(vk_memory, None);
                raw_device.destroy_image(vk_image, None);
                return Err(InteropError::Vulkan(format!(
                    "vkAllocateCommandBuffers (acquire barrier): {e}"
                )));
            }
        };

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        if let Err(e) = raw_device.begin_command_buffer(cmd_buf, &begin_info) {
            raw_device.destroy_command_pool(cmd_pool, None);
            raw_device.free_memory(vk_memory, None);
            raw_device.destroy_image(vk_image, None);
            return Err(InteropError::Vulkan(format!(
                "vkBeginCommandBuffer (acquire barrier): {e}"
            )));
        }

        let acquire_barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(queue_family_index)
            .image(vk_image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        raw_device.cmd_pipeline_barrier(
            cmd_buf,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[acquire_barrier],
        );

        if let Err(e) = raw_device.end_command_buffer(cmd_buf) {
            raw_device.destroy_command_pool(cmd_pool, None);
            raw_device.free_memory(vk_memory, None);
            raw_device.destroy_image(vk_image, None);
            return Err(InteropError::Vulkan(format!(
                "vkEndCommandBuffer (acquire barrier): {e}"
            )));
        }

        let cmd_buffers = [cmd_buf];
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmd_buffers);
        if let Err(e) = raw_device.queue_submit(vk_queue, &[submit_info], vk::Fence::null()) {
            raw_device.destroy_command_pool(cmd_pool, None);
            raw_device.free_memory(vk_memory, None);
            raw_device.destroy_image(vk_image, None);
            return Err(InteropError::Vulkan(format!(
                "vkQueueSubmit (acquire barrier): {e}"
            )));
        }
        if let Err(e) = raw_device.queue_wait_idle(vk_queue) {
            raw_device.destroy_command_pool(cmd_pool, None);
            raw_device.free_memory(vk_memory, None);
            raw_device.destroy_image(vk_image, None);
            return Err(InteropError::Vulkan(format!(
                "vkQueueWaitIdle (acquire barrier): {e}"
            )));
        }

        raw_device.destroy_command_pool(cmd_pool, None);

        // ---- 4. Wrap as wgpu_hal::vulkan::Texture ----
        let hal_descriptor = wgpu_hal::TextureDescriptor {
            label: Some("scrying-dmabuf-import"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: frame.format,
            usage: wgpu_types::TextureUses::RESOURCE | wgpu_types::TextureUses::COPY_SRC,
            memory_flags: wgpu_hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        let hal_texture = hal_device.texture_from_raw(
            vk_image,
            &hal_descriptor,
            None,
            // `TextureMemory::Dedicated` hands the VkDeviceMemory to
            // wgpu-hal which frees it when the wgpu::Texture drops.
            // The DMABUF fd already transferred ownership to Vulkan,
            // so the fd cleanup happens automatically too.
            wgpu_hal::vulkan::TextureMemory::Dedicated(vk_memory),
        );

        // ---- 5. Wrap as wgpu::Texture ----
        let wgpu_texture = host.device.create_texture_from_hal::<Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("scrying-dmabuf-import"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                format: frame.format,
                dimension: wgpu::TextureDimension::D2,
                mip_level_count: 1,
                sample_count: 1,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
        );

        // ---- 6. Optional: producer-sync semaphore wait ----
        if let (Some(semaphore_fd), SyncMechanism::ExplicitExternalSemaphore) =
            (frame.semaphore_fd, frame.producer_sync)
        {
            let ash_instance = hal_device.shared_instance().raw_instance();
            if let Err(e) =
                wait_on_producer_semaphore(host, ash_instance, raw_device, semaphore_fd)
            {
                // The texture is valid; the wait failed. Surface the
                // error so callers can decide whether the data is
                // safe to sample (probably not).
                return Err(e);
            }
        }

        wgpu_texture
    };

    Ok(ImportedTexture {
        texture,
        format: frame.format,
        size: frame.size,
        generation: frame.generation,
        consumer_sync: frame.producer_sync,
    })
}

/// Import the producer's binary `VkSemaphore` from the supplied
/// opaque fd, inject a wait-only `vkQueueSubmit` on the consumer's
/// `vk::Queue`, drain via `vkQueueWaitIdle`, then destroy the
/// semaphore.
///
/// Thread-safety: `wgpu::Queue::as_hal` returns a Deref guard that
/// does **not** lock the underlying queue — Vulkan queues are
/// externally synchronized, so calling `vkQueueSubmit` from this
/// path while wgpu submits from another thread is undefined
/// behaviour. The Phase 4a.2 contract is single-threaded import:
/// callers must ensure no other wgpu queue work is in flight on
/// this queue during `import_frame`. A future iteration that adds
/// a wgpu-hal `Queue::add_wait_semaphore` upstream would lift this
/// restriction.
unsafe fn wait_on_producer_semaphore(
    host: &HostWgpuContext,
    ash_instance: &ash::Instance,
    raw_device: &ash::Device,
    semaphore_fd: i32,
) -> Result<(), InteropError> {
    // Resolve `vkImportSemaphoreFdKHR` via the live wgpu-hal
    // `ash::Instance` (reachable through
    // `hal_device.shared_instance().raw_instance()`). Per Vulkan 1.2+
    // `vkGetInstanceProcAddr(VK_NULL_HANDLE, …)` only returns the
    // four global commands — going through the entry alone would
    // return null. The ash `Instance` wraps the real `VkInstance`
    // wgpu created so `get_device_proc_addr` resolves correctly.
    let raw_proc = unsafe {
        ash_instance
            .get_device_proc_addr(raw_device.handle(), c"vkImportSemaphoreFdKHR".as_ptr())
    };
    if raw_proc.is_none() {
        return Err(InteropError::Vulkan(
            "vkGetDeviceProcAddr(vkImportSemaphoreFdKHR) returned null; \
             VK_KHR_external_semaphore_fd not enabled on this device"
                .into(),
        ));
    }
    type PfnImportSemaphoreFd = unsafe extern "system" fn(vk::Device, *const c_void) -> vk::Result;
    let import_fd: PfnImportSemaphoreFd = unsafe { mem::transmute_copy(&raw_proc) };

    // Create an empty VkSemaphore.
    let create_info = vk::SemaphoreCreateInfo::default();
    let vk_semaphore = unsafe {
        raw_device
            .create_semaphore(&create_info, None)
            .map_err(|e| InteropError::Vulkan(format!("vkCreateSemaphore: {e}")))?
    };

    // Import the producer's fd into the semaphore. Per the Vulkan
    // spec, OPAQUE_FD imports transfer ownership of the fd to the
    // driver on success.
    let import_info = vk::ImportSemaphoreFdInfoKHR::default()
        .semaphore(vk_semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
        .fd(semaphore_fd);
    let result = unsafe { import_fd(raw_device.handle(), &import_info as *const _ as *const _) };
    if result != vk::Result::SUCCESS {
        unsafe { raw_device.destroy_semaphore(vk_semaphore, None) };
        return Err(InteropError::Vulkan(format!(
            "vkImportSemaphoreFdKHR failed: {result:?}"
        )));
    }

    // Reach the underlying vk::Queue via wgpu's HAL escape. The
    // returned guard is a Deref proxy; the raw handle is valid for
    // direct vkQueueSubmit calls as long as we don't race against
    // other wgpu work (see the function-level note above).
    let queue_guard =
        unsafe { host.queue.as_hal::<Vulkan>() }.ok_or(InteropError::BackendMismatch {
            expected: "Vulkan",
            actual: "non-Vulkan",
        })?;
    let vk_queue = queue_guard.as_raw();

    let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
    let wait_semaphores = [vk_semaphore];
    let submit_info = vk::SubmitInfo::default()
        .wait_semaphores(&wait_semaphores)
        .wait_dst_stage_mask(&wait_stages);

    let submit_result =
        unsafe { raw_device.queue_submit(vk_queue, &[submit_info], vk::Fence::null()) };
    if let Err(e) = submit_result {
        unsafe { raw_device.destroy_semaphore(vk_semaphore, None) };
        return Err(InteropError::Vulkan(format!(
            "vkQueueSubmit(wait_semaphores=[producer]): {e}"
        )));
    }

    // CPU-side drain so we can destroy the semaphore safely. Future
    // iterations can swap this for a fence-tracked deferred cleanup
    // so the consumer keeps running concurrently with the producer.
    let drain_result = unsafe { raw_device.queue_wait_idle(vk_queue) };
    unsafe { raw_device.destroy_semaphore(vk_semaphore, None) };
    if let Err(e) = drain_result {
        return Err(InteropError::Vulkan(format!("vkQueueWaitIdle: {e}")));
    }

    Ok(())
}

/// Translate DRM fourcc (the `drm_format` field) to a Vulkan format.
/// Only the BGRA/RGBA single-plane cases that WebKit's offscreen
/// DMABUF renderer actually produces today. Returns `None` for
/// anything we haven't validated.
fn vk_format_from_dmabuf(frame: &DmaBufImage) -> Option<vk::Format> {
    // linux/drm_fourcc.h — little-endian byte-order so the bytes in
    // memory are reversed vs. the human-readable fourcc.
    const DRM_FORMAT_ARGB8888: u32 = 0x34325241; // 'AR24' (BGRA bytes-in-memory)
    const DRM_FORMAT_ABGR8888: u32 = 0x34324241; // 'AB24' (RGBA bytes-in-memory)
    const DRM_FORMAT_XRGB8888: u32 = 0x34325258; // 'XR24'
    const DRM_FORMAT_XBGR8888: u32 = 0x34324258; // 'XB24'

    match frame.drm_format {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Some(vk::Format::B8G8R8A8_UNORM),
        DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => Some(vk::Format::R8G8B8A8_UNORM),
        _ => None,
    }
}

/// Lowest set bit in the memory-type-bits mask, or `None` if
/// `mask == 0`.
fn first_set_bit(mask: u32) -> Option<u32> {
    if mask == 0 {
        None
    } else {
        Some(mask.trailing_zeros())
    }
}

/// Capability probe surfaced by [`crate::WebSurfaceCapabilities::probe`]
/// (and any caller that wants to short-circuit before constructing a
/// `DmaBufImage`).
///
/// Returns `Ok(())` if the host's wgpu device has the Vulkan
/// extensions [`import`] needs at runtime. Returns
/// [`super::UnsupportedReason`] otherwise so callers can downgrade
/// `imported_texture` in the capability struct.
///
/// Cheap to call once at host setup; not designed for per-frame use
/// (dlopens libvulkan via `ash::Entry::load`).
pub(crate) fn probe_dmabuf_extensions(
    host: &super::HostWgpuContext,
) -> Result<(), super::UnsupportedReason> {
    if host.backend != super::InteropBackend::Vulkan {
        return Err(super::UnsupportedReason::HostBackendMismatch);
    }

    let hal_device = unsafe { host.device.as_hal::<Vulkan>() }
        .ok_or(super::UnsupportedReason::HostBackendMismatch)?;
    let raw_device: &ash::Device = hal_device.raw_device();
    let ash_instance: &ash::Instance = hal_device.shared_instance().raw_instance();

    // Signature functions of the device extensions the import path
    // depends on. `vkImportMemoryFdKHR` itself isn't probed directly
    // — `vkAllocateMemory` accepts the import-fd chain struct so the
    // function pointer is never resolved separately; but
    // `vkGetMemoryFdPropertiesKHR` is the marker we can query for
    // VK_KHR_external_memory_fd availability.
    let required = [
        c"vkGetMemoryFdPropertiesKHR", // VK_KHR_external_memory_fd
        c"vkGetImageDrmFormatModifierPropertiesEXT", // VK_EXT_image_drm_format_modifier
    ];
    for name in required {
        let ptr = unsafe { ash_instance.get_device_proc_addr(raw_device.handle(), name.as_ptr()) };
        if ptr.is_none() {
            return Err(super::UnsupportedReason::NativeImportNotYetImplemented);
        }
    }

    Ok(())
}

/// Errors that can occur while building a DMABUF-capable wgpu device
/// via [`build_dmabuf_capable_device`].
#[derive(Debug, thiserror::Error)]
pub enum DmaBufDeviceError {
    #[error("adapter backend is not Vulkan — DMABUF import is Linux/Vulkan-only")]
    NotVulkanBackend,
    #[error("physical device is missing required extensions: {0:?}")]
    MissingExtensions(Vec<&'static str>),
    #[error("vkEnumerateDeviceExtensionProperties failed: {0}")]
    ExtensionEnumeration(String),
    #[error("wgpu-hal Adapter::open_with_callback failed: {0}")]
    HalOpen(String),
    #[error("wgpu Adapter::create_device_from_hal failed: {0}")]
    DeviceCreation(#[from] wgpu::RequestDeviceError),
}

/// Build a [`wgpu::Device`] + [`wgpu::Queue`] from `adapter` with the
/// Vulkan device extensions [`import`] needs at runtime enabled:
///
/// - `VK_EXT_image_drm_format_modifier` — required by the
///   `DRM_FORMAT_MODIFIER_EXT` tiling path on
///   [`vk::ImageCreateInfo`].
/// - `VK_KHR_external_semaphore_fd` — required by
///   [`wait_on_producer_semaphore`] when
///   `SyncMechanism::ExplicitExternalSemaphore` frames arrive.
///
/// `VK_KHR_external_memory_fd` is enabled by wgpu-hal already when
/// the adapter exposes it; we don't push it again to avoid
/// `VK_ERROR_VALIDATION_FAILED` from duplicate names.
///
/// Use this in place of [`wgpu::Adapter::request_device`] when you
/// want [`WgpuTextureImporter`](crate::WgpuTextureImporter) to
/// actually be able to import producer DMABUFs.
///
/// Returns [`DmaBufDeviceError::MissingExtensions`] when the
/// physical device doesn't expose one or both of the required
/// extensions — typically rare on modern Linux hardware (RADV / ANV /
/// the NVIDIA proprietary driver all expose them on contemporary
/// versions), but worth surfacing so callers can fall back to
/// `request_device` + `CpuSnapshot`.
pub fn build_dmabuf_capable_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> Result<(wgpu::Device, wgpu::Queue), DmaBufDeviceError> {
    const REQUIRED: &[&CStr] = &[
        vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME,
        vk::KHR_EXTERNAL_SEMAPHORE_FD_NAME,
    ];

    let hal_adapter = unsafe { adapter.as_hal::<Vulkan>() }
        .ok_or(DmaBufDeviceError::NotVulkanBackend)?;

    // Probe `vkEnumerateDeviceExtensionProperties` so we can return a
    // structured "you're missing X" error instead of letting
    // vkCreateDevice die with `ERROR_EXTENSION_NOT_PRESENT` and
    // `hal_usage_error` panicking.
    let pd = hal_adapter.raw_physical_device();
    let ash_instance = hal_adapter.shared_instance().raw_instance();
    let supported = unsafe { ash_instance.enumerate_device_extension_properties(pd) }
        .map_err(|e| DmaBufDeviceError::ExtensionEnumeration(format!("{e}")))?;

    let missing: Vec<&'static str> = REQUIRED
        .iter()
        .filter(|&&needle| {
            !supported.iter().any(|prop| {
                let name = prop.extension_name_as_c_str().unwrap_or_default();
                name == needle
            })
        })
        .map(|&n| n.to_str().unwrap_or("<invalid-utf8-ext-name>"))
        .collect();
    if !missing.is_empty() {
        return Err(DmaBufDeviceError::MissingExtensions(missing));
    }

    // Walk wgpu-hal's `open_with_callback` so we get to mutate the
    // extensions vec wgpu-hal built. Only push names that aren't
    // already there — Vulkan rejects duplicates in
    // `vkCreateDevice`.
    let hal_open = unsafe {
        hal_adapter.open_with_callback(
            desc.required_features,
            &desc.required_limits,
            &desc.memory_hints,
            Some(Box::new(|args| {
                for &name in REQUIRED {
                    if !args.extensions.iter().any(|e| *e == name) {
                        args.extensions.push(name);
                    }
                }
            })),
        )
    }
    .map_err(|e| DmaBufDeviceError::HalOpen(format!("{e:?}")))?;

    drop(hal_adapter);

    let (device, queue) =
        unsafe { adapter.create_device_from_hal::<Vulkan>(hal_open, desc) }?;
    Ok((device, queue))
}

#[cfg(test)]
mod plane_share_tests {
    use super::*;

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

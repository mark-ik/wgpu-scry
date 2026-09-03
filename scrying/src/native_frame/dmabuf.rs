// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Scry's WPE/DMABUF policy around Graft's Vulkan import boundary.
//!
//! Scry owns producer synchronization and the explicit fallback from WPE's
//! invalid modifier sentinel to linear. Graft owns fd lifetime, DRM image
//! creation, memory import, shared-fd multi-plane handling, and foreign-queue
//! acquisition.

#![cfg(target_os = "linux")]

use std::ffi::{CStr, c_void};
use std::mem;

use ash::vk;
use wgpu::wgc::api::Vulkan;

use super::{
    DmaBufImage, HostWgpuContext, ImportedTexture, InteropBackend, InteropError, SyncMechanism,
    UnsupportedReason,
};

const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

struct OwnedFds(Vec<i32>);

impl OwnedFds {
    fn new(fds: impl IntoIterator<Item = i32>) -> Self {
        let mut owned = Vec::new();
        for fd in fds {
            if fd >= 0 && !owned.contains(&fd) {
                owned.push(fd);
            }
        }
        Self(owned)
    }

    fn disarm(&mut self) {
        self.0.clear();
    }
}

impl Drop for OwnedFds {
    fn drop(&mut self) {
        for fd in self.0.drain(..) {
            // SAFETY: this guard exists only while Scry owns the descriptor.
            unsafe { libc::close(fd) };
        }
    }
}

pub(super) fn import(
    frame: &DmaBufImage,
    host: &HostWgpuContext,
) -> Result<ImportedTexture, InteropError> {
    let mut plane_fds = OwnedFds::new(frame.planes.iter().map(|plane| plane.fd));
    let mut semaphore_fd = OwnedFds::new(frame.semaphore_fd);

    if host.backend != InteropBackend::Vulkan {
        return Err(InteropError::BackendMismatch {
            expected: "Vulkan",
            actual: "non-Vulkan",
        });
    }
    if frame.planes.is_empty() {
        return Err(InteropError::InvalidFrame("DmaBufImage has no planes"));
    }

    // Explicit producer completion must precede Graft's ownership-acquire
    // submit. The helper owns the fd after this handoff and closes it on every
    // pre-import error; Vulkan owns it after a successful semaphore import.
    if let (Some(fd), SyncMechanism::ExplicitExternalSemaphore) =
        (frame.semaphore_fd, frame.producer_sync)
    {
        semaphore_fd.disarm();
        let hal_device =
            unsafe { host.device.as_hal::<Vulkan>() }.ok_or(InteropError::BackendMismatch {
                expected: "Vulkan",
                actual: "non-Vulkan",
            })?;
        unsafe {
            wait_on_producer_semaphore(
                host,
                hal_device.shared_instance().raw_instance(),
                hal_device.raw_device(),
                fd,
            )?;
        }
    }

    let effective_modifier = if frame.drm_modifier == DRM_FORMAT_MOD_INVALID {
        DRM_FORMAT_MOD_LINEAR
    } else {
        frame.drm_modifier
    };
    let graft_host = grafting::HostWgpuContext::new(host.device.clone(), host.queue.clone());
    let graft_frame = grafting::vulkan_dmabuf::VulkanDmaBufImport {
        size: frame.size,
        format: frame.format,
        drm_format: frame.drm_format,
        drm_modifier: effective_modifier,
        planes: frame
            .planes
            .iter()
            .map(|plane| grafting::vulkan_dmabuf::VulkanDmaBufPlane {
                fd: plane.fd,
                offset: u64::from(plane.offset),
                stride: u64::from(plane.stride),
            })
            .collect(),
        queue_ownership: grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
    };

    // Graft owns every plane descriptor from this point, including validation
    // failures before Vulkan accepts the external memory.
    plane_fds.disarm();
    let texture = grafting::vulkan_dmabuf::import_dmabuf(graft_frame, &graft_host)
        .map_err(map_graft_error)?;

    Ok(ImportedTexture {
        texture,
        format: frame.format,
        size: frame.size,
        generation: frame.generation,
        consumer_sync: frame.producer_sync,
    })
}

fn map_graft_error(error: grafting::InteropError) -> InteropError {
    match error {
        grafting::InteropError::BackendMismatch { expected, actual } => {
            InteropError::BackendMismatch { expected, actual }
        }
        grafting::InteropError::InvalidFrame(message) => InteropError::InvalidFrame(message),
        grafting::InteropError::Vulkan(message) => InteropError::Vulkan(message),
        grafting::InteropError::Unsupported(_) => {
            InteropError::Unsupported(UnsupportedReason::NativeImportNotYetImplemented)
        }
        other => InteropError::Vulkan(other.to_string()),
    }
}

/// Import and drain the producer's binary Vulkan semaphore before touching
/// the associated image. The caller must serialize this direct submit against
/// other work on the same wgpu queue.
unsafe fn wait_on_producer_semaphore(
    host: &HostWgpuContext,
    ash_instance: &ash::Instance,
    raw_device: &ash::Device,
    semaphore_fd: i32,
) -> Result<(), InteropError> {
    let mut owned_fd = OwnedFds::new([semaphore_fd]);
    let raw_proc = unsafe {
        ash_instance.get_device_proc_addr(raw_device.handle(), c"vkImportSemaphoreFdKHR".as_ptr())
    };
    if raw_proc.is_none() {
        return Err(InteropError::Vulkan(
            "vkGetDeviceProcAddr(vkImportSemaphoreFdKHR) returned null; \
             VK_KHR_external_semaphore_fd is not enabled"
                .into(),
        ));
    }
    type PfnImportSemaphoreFd = unsafe extern "system" fn(vk::Device, *const c_void) -> vk::Result;
    let import_fd: PfnImportSemaphoreFd = unsafe { mem::transmute_copy(&raw_proc) };

    let vk_semaphore = unsafe {
        raw_device
            .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            .map_err(|error| InteropError::Vulkan(format!("vkCreateSemaphore: {error}")))?
    };
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
    owned_fd.disarm();

    let queue = unsafe { host.queue.as_hal::<Vulkan>() }.ok_or(InteropError::BackendMismatch {
        expected: "Vulkan",
        actual: "non-Vulkan",
    })?;
    let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
    let wait_semaphores = [vk_semaphore];
    let submit = vk::SubmitInfo::default()
        .wait_semaphores(&wait_semaphores)
        .wait_dst_stage_mask(&wait_stages);
    if let Err(error) =
        unsafe { raw_device.queue_submit(queue.as_raw(), &[submit], vk::Fence::null()) }
    {
        unsafe { raw_device.destroy_semaphore(vk_semaphore, None) };
        return Err(InteropError::Vulkan(format!(
            "vkQueueSubmit (producer semaphore): {error}"
        )));
    }

    let drain = unsafe { raw_device.queue_wait_idle(queue.as_raw()) };
    unsafe { raw_device.destroy_semaphore(vk_semaphore, None) };
    drain.map_err(|error| InteropError::Vulkan(format!("vkQueueWaitIdle: {error}")))
}

/// Probe the Vulkan functions used by the wgpu 30 content-preserving import.
pub(crate) fn probe_dmabuf_extensions(host: &HostWgpuContext) -> Result<(), UnsupportedReason> {
    if host.backend != InteropBackend::Vulkan {
        return Err(UnsupportedReason::HostBackendMismatch);
    }
    #[cfg(not(feature = "wgpu-30"))]
    {
        let _ = host;
        return Err(UnsupportedReason::NativeImportNotYetImplemented);
    }

    #[cfg(feature = "wgpu-30")]
    {
        let hal_device = unsafe { host.device.as_hal::<Vulkan>() }
            .ok_or(UnsupportedReason::HostBackendMismatch)?;
        if !hal_device
            .enabled_device_extensions()
            .contains(&vk::EXT_QUEUE_FAMILY_FOREIGN_NAME)
        {
            return Err(UnsupportedReason::NativeImportNotYetImplemented);
        }
        let raw_device = hal_device.raw_device();
        let ash_instance = hal_device.shared_instance().raw_instance();
        for name in [
            c"vkGetMemoryFdPropertiesKHR",
            c"vkGetImageDrmFormatModifierPropertiesEXT",
        ] {
            let pointer =
                unsafe { ash_instance.get_device_proc_addr(raw_device.handle(), name.as_ptr()) };
            if pointer.is_none() {
                return Err(UnsupportedReason::NativeImportNotYetImplemented);
            }
        }
        Ok(())
    }
}

/// Errors from constructing a DMABUF-capable wgpu device.
#[derive(Debug, thiserror::Error)]
pub enum DmaBufDeviceError {
    #[error("adapter backend is not Vulkan; DMABUF import is Linux/Vulkan-only")]
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

/// Build a wgpu device with DRM-modifier, foreign-queue, and
/// external-semaphore support.
pub fn build_dmabuf_capable_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> Result<(wgpu::Device, wgpu::Queue), DmaBufDeviceError> {
    const REQUIRED: &[&CStr] = &[
        vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME,
        vk::EXT_QUEUE_FAMILY_FOREIGN_NAME,
        vk::KHR_EXTERNAL_SEMAPHORE_FD_NAME,
    ];

    let hal_adapter =
        unsafe { adapter.as_hal::<Vulkan>() }.ok_or(DmaBufDeviceError::NotVulkanBackend)?;
    let physical_device = hal_adapter.raw_physical_device();
    let ash_instance = hal_adapter.shared_instance().raw_instance();
    let supported = unsafe { ash_instance.enumerate_device_extension_properties(physical_device) }
        .map_err(|error| DmaBufDeviceError::ExtensionEnumeration(error.to_string()))?;
    let missing: Vec<&'static str> = REQUIRED
        .iter()
        .filter(|&&required| {
            !supported
                .iter()
                .any(|property| property.extension_name_as_c_str().unwrap_or_default() == required)
        })
        .map(|name| name.to_str().unwrap_or("<invalid-utf8-extension-name>"))
        .collect();
    if !missing.is_empty() {
        return Err(DmaBufDeviceError::MissingExtensions(missing));
    }

    #[cfg(any(feature = "wgpu-29", feature = "wgpu-30"))]
    let open = unsafe {
        hal_adapter.open_with_callback(
            desc.required_features,
            &desc.required_limits,
            &desc.memory_hints,
            Some(Box::new(|args| {
                for &name in REQUIRED {
                    if !args.extensions.contains(&name) {
                        args.extensions.push(name);
                    }
                }
            })),
        )
    }
    .map_err(|error| DmaBufDeviceError::HalOpen(format!("{error:?}")))?;
    #[cfg(not(any(feature = "wgpu-29", feature = "wgpu-30")))]
    let open = unsafe {
        hal_adapter.open_with_callback(
            desc.required_features,
            &desc.memory_hints,
            Some(Box::new(|args| {
                for &name in REQUIRED {
                    if !args.extensions.contains(&name) {
                        args.extensions.push(name);
                    }
                }
            })),
        )
    }
    .map_err(|error| DmaBufDeviceError::HalOpen(format!("{error:?}")))?;
    drop(hal_adapter);

    let (device, queue) = unsafe { adapter.create_device_from_hal::<Vulkan>(open, desc) }?;
    Ok((device, queue))
}

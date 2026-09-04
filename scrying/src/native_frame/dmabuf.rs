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
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

use ash::vk;
use wgpu::wgc::api::Vulkan;

use super::{
    DmaBufImage, HostWgpuContext, ImportedTexture, InteropBackend, InteropError, SyncMechanism,
    UnsupportedReason,
};

const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

pub(super) fn import(
    frame: DmaBufImage,
    host: &HostWgpuContext,
) -> Result<ImportedTexture, InteropError> {
    let size = frame.size;
    let format = frame.format;
    let generation = frame.generation;
    let producer_sync = frame.producer_sync;
    let semaphore_fd = frame.semaphore_fd();

    if host.backend != InteropBackend::Vulkan {
        return Err(InteropError::BackendMismatch {
            expected: "Vulkan",
            actual: "non-Vulkan",
        });
    }
    if frame.planes().is_empty() {
        return Err(InteropError::InvalidFrame("DmaBufImage has no planes"));
    }

    // Explicit producer completion must precede Graft's ownership-acquire
    // submit. The helper owns the fd after this handoff and closes it on every
    // pre-import error; Vulkan owns it after a successful semaphore import.
    if let (Some(fd), SyncMechanism::ExplicitExternalSemaphore) = (semaphore_fd, producer_sync) {
        unsafe { wait_on_producer_semaphore(host, fd)? };
    }

    let effective_modifier = if frame.drm_modifier == DRM_FORMAT_MOD_INVALID {
        DRM_FORMAT_MOD_LINEAR
    } else {
        frame.drm_modifier
    };
    let (graft_frame, _semaphore_owner) = frame.into_graft_import(effective_modifier)?;
    let graft_host = grafting::HostWgpuContext::new(host.device.clone(), host.queue.clone());
    let texture = grafting::vulkan_dmabuf::import_dmabuf(graft_frame, &graft_host)
        .map_err(map_graft_error)?;

    Ok(ImportedTexture {
        texture,
        format,
        size,
        generation,
        consumer_sync: producer_sync,
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
    semaphore_fd: i32,
) -> Result<(), InteropError> {
    let duplicated = unsafe { libc::dup(semaphore_fd) };
    if duplicated < 0 {
        return Err(InteropError::Vulkan(
            "dup producer semaphore fd failed".into(),
        ));
    }
    let duplicated = unsafe { OwnedFd::from_raw_fd(duplicated) };
    let hal_device =
        unsafe { host.device.as_hal::<Vulkan>() }.ok_or(InteropError::BackendMismatch {
            expected: "Vulkan",
            actual: "non-Vulkan",
        })?;
    let ash_instance = hal_device.shared_instance().raw_instance();
    let raw_device = hal_device.raw_device();
    let raw_proc = unsafe {
        ash_instance.get_device_proc_addr(raw_device.handle(), c"vkImportSemaphoreFdKHR".as_ptr())
    };
    let Some(raw_proc) = raw_proc else {
        return Err(InteropError::Vulkan(
            "vkGetDeviceProcAddr(vkImportSemaphoreFdKHR) returned null; \
             VK_KHR_external_semaphore_fd is not enabled"
                .into(),
        ));
    };
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
        .fd(duplicated.as_raw_fd());
    let result = unsafe { import_fd(raw_device.handle(), &import_info as *const _ as *const _) };
    if result != vk::Result::SUCCESS {
        unsafe { raw_device.destroy_semaphore(vk_semaphore, None) };
        return Err(InteropError::Vulkan(format!(
            "vkImportSemaphoreFdKHR failed: {result:?}"
        )));
    }
    let _ = duplicated.into_raw_fd();

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

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::native_frame::DmaBufPlane;
    use std::os::fd::{FromRawFd, OwnedFd};

    fn pipe_fd() -> i32 {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { libc::close(fds[1]) };
        fds[0]
    }

    fn fd_open(fd: i32) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    fn owned(fd: i32) -> OwnedFd {
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[test]
    fn repeated_plane_index_uses_one_owner_and_closes_on_drop() {
        let fd = pipe_fd();
        let image = DmaBufImage::from_owned_buffers(
            dpi::PhysicalSize::new(4, 4),
            wgpu::TextureFormat::Bgra8Unorm,
            0,
            0,
            vec![owned(fd)],
            vec![DmaBufPlane::new(0, 0, 16), DmaBufPlane::new(0, 16, 16)],
            1,
            SyncMechanism::None,
            None,
        )
        .expect("shared buffer index");
        let (import, _) = image.into_graft_import(0).expect("shared plane table");
        drop(import);
        assert!(!fd_open(fd), "shared buffer fd must close exactly once");
    }

    #[test]
    fn distinct_dup_fds_keep_distinct_owners() {
        let fd = pipe_fd();
        let dup_fd = unsafe { libc::dup(fd) };
        assert!(dup_fd >= 0);
        let image = DmaBufImage::from_owned_buffers(
            dpi::PhysicalSize::new(4, 4),
            wgpu::TextureFormat::Bgra8Unorm,
            0,
            0,
            vec![owned(fd), owned(dup_fd)],
            vec![DmaBufPlane::new(0, 0, 16), DmaBufPlane::new(1, 16, 16)],
            1,
            SyncMechanism::None,
            None,
        )
        .expect("distinct owned buffers");
        let (import, _) = image.into_graft_import(0).expect("distinct plane table");
        drop(import);
        assert!(!fd_open(fd), "first dup fd must close on drop");
        assert!(!fd_open(dup_fd), "second dup fd must close on drop");
    }

    #[test]
    fn validation_failure_closes_valid_descriptors() {
        let fd = pipe_fd();
        let result = DmaBufImage::from_owned_buffers(
            dpi::PhysicalSize::new(4, 4),
            wgpu::TextureFormat::Bgra8Unorm,
            0,
            0,
            vec![owned(fd)],
            vec![DmaBufPlane::new(1, 0, 16)],
            1,
            SyncMechanism::None,
            None,
        );
        assert!(result.is_err());
        assert!(!fd_open(fd), "validation failure must close owned fds");
    }

    #[test]
    fn raw_constructor_error_closes_all_valid_descriptors() {
        let first = pipe_fd();
        let second = pipe_fd();
        let result = unsafe {
            DmaBufImage::from_owned_raw_buffers(
                dpi::PhysicalSize::new(4, 4),
                wgpu::TextureFormat::Bgra8Unorm,
                0,
                0,
                vec![first, -1, second],
                vec![DmaBufPlane::new(0, 0, 16)],
                1,
                SyncMechanism::None,
                None,
            )
        };
        assert!(result.is_err());
        assert!(!fd_open(first));
        assert!(!fd_open(second));
    }
}

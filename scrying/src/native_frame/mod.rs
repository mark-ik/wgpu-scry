// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Native-frame import: import platform-native GPU texture handles
//! (D3D12 NT-handle today, IOSurface and DMABUF eventually) into wgpu
//! textures owned by the host device.
//!
//! Derived structurally from the per-platform `rendering_context/` shape
//! in the [Slint Servo embedding example][1] and adapted to take native
//! handles directly (no surfman GL FBO bridge).
//!
//! [1]: https://github.com/slint-ui/slint/tree/master/examples/servo

mod error;
mod sync;

#[cfg(target_os = "linux")]
pub(crate) mod dmabuf;

#[cfg(target_os = "windows")]
mod sync_dx12;

#[cfg(target_os = "macos")]
mod metal;

#[cfg(target_os = "macos")]
mod sync_metal;

use dpi::PhysicalSize;

pub use error::{InteropError, UnsupportedReason};
pub use sync::{
    ExplicitExternalSemaphoreSynchronizer, ImplicitOnlySynchronizer, InteropSynchronizer,
    NoopSynchronizer, SyncMechanism,
};

#[cfg(target_os = "windows")]
pub use sync_dx12::Dx12FenceSynchronizer;

#[cfg(target_os = "macos")]
pub use sync_metal::MetalSharedEventSynchronizer;

/// The wgpu graphics backend in use on the host device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InteropBackend {
    Vulkan,
    Metal,
    Dx12,
    Unknown,
}

/// Discriminant for [`NativeFrame`] variants without carrying frame data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NativeFrameKind {
    Dx12SharedTexture,
    /// MTLTexture reference (Apple platforms). The producer creates the
    /// MTLTexture itself — typically by bridging a `CVPixelBuffer` /
    /// `IOSurfaceRef` from ScreenCaptureKit through
    /// `[MTLDevice newTextureWithDescriptor:iosurface:plane:]` — and
    /// hands the resulting `*mut MTLTexture` to the importer.
    MetalTextureRef,
    /// Linux WPE DMABUF frame. The producer exports one or more DMABUF
    /// plane file descriptors plus a DRM format/modifier and optional
    /// external semaphore fd for explicit Vulkan ordering.
    DmaBufImage,
}

/// Whether a particular interop capability is available on this device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityStatus {
    Supported,
    Unsupported(UnsupportedReason),
}

/// The set of [`NativeFrameKind`]s a producer can emit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProducerCapabilities {
    pub supported_frames: Vec<NativeFrameKind>,
}

/// Wraps a `wgpu::Device` and `wgpu::Queue` together with the detected
/// backend.
#[derive(Clone, Debug)]
pub struct HostWgpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub backend: InteropBackend,
}

impl HostWgpuContext {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self {
            backend: detect_backend(&device),
            device,
            queue,
        }
    }
}

/// Options that control how [`WgpuTextureImporter`] processes each frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImportOptions {
    /// Currently unused; reserved for future use (e.g. CPU-fallback gates).
    pub allow_copy_fallback: bool,
}

/// A successfully imported wgpu texture, ready for use in a render pipeline.
#[derive(Debug)]
pub struct ImportedTexture {
    pub texture: wgpu::Texture,
    pub format: wgpu::TextureFormat,
    pub size: PhysicalSize<u32>,
    pub generation: u64,
    pub consumer_sync: SyncMechanism,
}

/// A frame backed by a D3D12 resource shared via a DXGI NT handle.
///
/// Obtain the handle by calling `IDXGIResource1::CreateSharedHandle` on
/// your `ID3D12Resource` (or the equivalent on a D3D11 producer). The
/// importer opens its own D3D12 reference via
/// `ID3D12Device::OpenSharedHandle`; the caller is responsible for closing
/// its copy of the handle after constructing this struct.
#[derive(Clone, Copy, Debug)]
pub struct Dx12SharedTexture {
    pub size: PhysicalSize<u32>,
    pub format: wgpu::TextureFormat,
    pub generation: u64,
    pub producer_sync: SyncMechanism,
    /// Fence value the producer signalled at on its `ID3D11Fence` /
    /// `ID3D12Fence` (opened from
    /// [`Dx12FenceSynchronizer::shared_handle`]). The synchronizer waits
    /// for this value on the wgpu D3D12 queue before the next consumer
    /// submit.
    ///
    /// Only meaningful when `producer_sync == SyncMechanism::ExplicitFence`.
    /// `0` for the keyed-mutex path; the synchronizer treats `0` as "no
    /// wait recorded for this frame".
    pub fence_value: u64,
    /// NT `HANDLE` from `IDXGIResource1::CreateSharedHandle`. Windows only.
    #[cfg(target_os = "windows")]
    pub handle: *mut std::ffi::c_void,
}

/// A frame backed by an `MTLTexture` from a Metal producer.
///
/// The producer is responsible for creating the `MTLTexture` (typically
/// by bridging an `IOSurfaceRef` from ScreenCaptureKit's
/// `CMSampleBuffer` through
/// `[MTLDevice newTextureWithDescriptor:iosurface:plane:]`) and ensuring
/// the texture remains valid for the duration of the import call.
/// Ownership is **not** transferred; the importer wraps the texture by
/// retaining it via the Metal API and does not release the producer's
/// reference.
///
/// The producer should use the **host's** `MTLDevice` (acquired via
/// `wgpu::Device::as_hal::<Metal>().raw_device()`) so the resulting
/// texture is usable on the host's wgpu queue without cross-device
/// migration.
#[derive(Debug)]
pub struct MetalTextureRef {
    pub size: PhysicalSize<u32>,
    pub format: wgpu::TextureFormat,
    pub generation: u64,
    pub producer_sync: SyncMechanism,
    /// Raw `MTLTexture *` pointer. Must be non-null. Apple platforms only.
    #[cfg(target_os = "macos")]
    pub raw_metal_texture: *mut std::ffi::c_void,
    /// `MTLSharedEvent` value the producer signals at after the
    /// per-frame Metal blit completes. Consumers that opt in to
    /// explicit synchronization (`producer_sync ==
    /// SyncMechanism::ExplicitMetalEvent`) wait for this value via
    /// `MTLCommandBuffer::encodeWaitForEvent:value:` on their own
    /// queue before sampling the texture, against the
    /// `MTLSharedEvent` exposed via
    /// [`crate::WkWebViewProducer::metal_shared_event`].
    ///
    /// Only meaningful when `producer_sync ==
    /// SyncMechanism::ExplicitMetalEvent`. `0` for the implicit-
    /// IOSurface-coherence path; the synchronizer treats `0` as
    /// "no wait recorded for this frame," matching the
    /// [`Dx12SharedTexture::fence_value`] convention.
    pub signal_value: u64,
}

/// One plane of a Linux DMABUF image.
///
/// File descriptor ownership is transferred with the frame. The eventual
/// Vulkan importer must duplicate or consume the fd, then close it after
/// `vkImportMemoryFdKHR` / image creation has taken ownership as required by
/// the Vulkan external-memory contract.
#[derive(Clone, Copy, Debug)]
pub struct DmaBufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

/// A Linux WPE frame exported as DMABUF planes.
#[derive(Clone, Debug)]
pub struct DmaBufImage {
    pub size: PhysicalSize<u32>,
    pub format: wgpu::TextureFormat,
    pub drm_format: u32,
    pub drm_modifier: u64,
    pub planes: Vec<DmaBufPlane>,
    pub generation: u64,
    pub producer_sync: SyncMechanism,
    /// Optional opaque fd for a Vulkan external semaphore signalled by the
    /// producer when the frame is ready. Ownership transfers with the frame.
    pub semaphore_fd: Option<i32>,
}

/// A native frame from a producer, ready to be imported by a
/// [`TextureImporter`].
#[non_exhaustive]
pub enum NativeFrame {
    Dx12SharedTexture(Dx12SharedTexture),
    MetalTextureRef(MetalTextureRef),
    DmaBufImage(DmaBufImage),
}

impl NativeFrame {
    pub fn kind(&self) -> NativeFrameKind {
        match self {
            NativeFrame::Dx12SharedTexture(_) => NativeFrameKind::Dx12SharedTexture,
            NativeFrame::MetalTextureRef(_) => NativeFrameKind::MetalTextureRef,
            NativeFrame::DmaBufImage(_) => NativeFrameKind::DmaBufImage,
        }
    }

    pub fn producer_sync(&self) -> SyncMechanism {
        match self {
            NativeFrame::Dx12SharedTexture(frame) => frame.producer_sync,
            NativeFrame::MetalTextureRef(frame) => frame.producer_sync,
            NativeFrame::DmaBufImage(frame) => frame.producer_sync,
        }
    }
}

/// Imports a [`NativeFrame`] into a `wgpu::Texture`.
pub trait TextureImporter {
    fn import_frame(
        &self,
        frame: &NativeFrame,
        options: &ImportOptions,
    ) -> Result<ImportedTexture, InteropError>;
}

/// Main entry point. Create one per wgpu device, reuse across frames.
pub struct WgpuTextureImporter {
    host: HostWgpuContext,
    synchronizer: Box<dyn InteropSynchronizer>,
}

impl WgpuTextureImporter {
    /// Default importer.
    ///
    /// - **macOS**: [`MetalSharedEventSynchronizer`] — accepts
    ///   both `SyncMechanism::None` (legacy) and
    ///   `SyncMechanism::ExplicitMetalEvent` (the macOS WKWebView
    ///   producer's per-frame `MTLSharedEvent` signal).
    ///   Consumer-side wait insertion is currently a no-op
    ///   because IOSurface coherence already covers correctness
    ///   on Apple silicon, but the synchronizer accepts the
    ///   advertised mechanism so the producer's
    ///   `MetalTextureRef::producer_sync ==
    ///   SyncMechanism::ExplicitMetalEvent` doesn't hit the
    ///   strict-rejection path on import.
    /// - **Other platforms**: [`ImplicitOnlySynchronizer`].
    pub fn new(host: HostWgpuContext) -> Self {
        #[cfg(target_os = "macos")]
        let synchronizer: Box<dyn InteropSynchronizer> = Box::new(MetalSharedEventSynchronizer);
        #[cfg(target_os = "linux")]
        let synchronizer: Box<dyn InteropSynchronizer> =
            Box::new(ExplicitExternalSemaphoreSynchronizer);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let synchronizer: Box<dyn InteropSynchronizer> = Box::new(ImplicitOnlySynchronizer);
        Self { host, synchronizer }
    }

    /// Importer with a custom [`InteropSynchronizer`].
    pub fn with_synchronizer(
        host: HostWgpuContext,
        synchronizer: Box<dyn InteropSynchronizer>,
    ) -> Self {
        Self { host, synchronizer }
    }

    pub fn host(&self) -> &HostWgpuContext {
        &self.host
    }
}

impl TextureImporter for WgpuTextureImporter {
    fn import_frame(
        &self,
        frame: &NativeFrame,
        _options: &ImportOptions,
    ) -> Result<ImportedTexture, InteropError> {
        self.synchronizer
            .producer_complete(frame, frame.producer_sync())?;

        let imported = match frame {
            NativeFrame::Dx12SharedTexture(frame) => import_dx12_shared_texture(frame, &self.host),
            NativeFrame::MetalTextureRef(frame) => import_metal_texture_ref(frame, &self.host),
            NativeFrame::DmaBufImage(frame) => import_dmabuf_image(frame, &self.host),
        }?;

        self.synchronizer
            .consumer_ready(&imported, imported.consumer_sync)?;
        Ok(imported)
    }
}

fn import_dmabuf_image(
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] frame: &DmaBufImage,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] host: &HostWgpuContext,
) -> Result<ImportedTexture, InteropError> {
    #[cfg(target_os = "linux")]
    {
        return dmabuf::import(frame, host);
    }

    #[cfg(not(target_os = "linux"))]
    Err(InteropError::Unsupported(
        UnsupportedReason::HostBackendMismatch,
    ))
}

fn import_dx12_shared_texture(
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))] frame: &Dx12SharedTexture,
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))] host: &HostWgpuContext,
) -> Result<ImportedTexture, InteropError> {
    #[cfg(target_os = "windows")]
    {
        if host.backend != InteropBackend::Dx12 {
            return Err(InteropError::BackendMismatch {
                expected: "Dx12",
                actual: "non-Dx12",
            });
        }

        // Delegate the generic OpenSharedHandle -> wgpu import to grafting (the
        // shared interop core). scrying's synchronizer still wraps this call in
        // `import_frame` (producer_complete / consumer_ready), so the explicit
        // fence / semaphore waits are unchanged; grafting's low-level import
        // ignores the frame's sync fields.
        let g_host = grafting::HostWgpuContext::new(host.device.clone(), host.queue.clone());
        let g_frame = grafting::Dx12SharedTexture {
            handle: frame.handle,
            size: frame.size,
            format: frame.format,
            generation: frame.generation,
            producer_sync: grafting::SyncMechanism::ImplicitGlFlush,
            fence_value: frame.fence_value,
        };
        let texture = grafting::import_dx12_shared_texture(&g_frame, &g_host)
            .map_err(|e| InteropError::Dx12(e.to_string()))?;

        return Ok(ImportedTexture {
            texture,
            format: frame.format,
            size: frame.size,
            generation: frame.generation,
            consumer_sync: frame.producer_sync,
        });
    }

    #[cfg(not(target_os = "windows"))]
    Err(InteropError::Unsupported(
        UnsupportedReason::HostBackendMismatch,
    ))
}

fn import_metal_texture_ref(
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] frame: &MetalTextureRef,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] host: &HostWgpuContext,
) -> Result<ImportedTexture, InteropError> {
    #[cfg(target_os = "macos")]
    {
        metal::import(frame, host)
    }

    #[cfg(not(target_os = "macos"))]
    Err(InteropError::Unsupported(
        UnsupportedReason::HostBackendMismatch,
    ))
}

fn detect_backend(device: &wgpu::Device) -> InteropBackend {
    unsafe {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "windows"))]
        if device.as_hal::<wgpu::wgc::api::Vulkan>().is_some() {
            return InteropBackend::Vulkan;
        }

        #[cfg(target_vendor = "apple")]
        if device.as_hal::<wgpu::wgc::api::Metal>().is_some() {
            return InteropBackend::Metal;
        }

        #[cfg(target_os = "windows")]
        if device.as_hal::<wgpu::wgc::api::Dx12>().is_some() {
            return InteropBackend::Dx12;
        }
    }

    InteropBackend::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_synchronizer_accepts_none() {
        assert!(ImplicitOnlySynchronizer::validate(SyncMechanism::None).is_ok());
    }

    #[test]
    fn implicit_synchronizer_rejects_explicit_fence() {
        assert!(matches!(
            ImplicitOnlySynchronizer::validate(SyncMechanism::ExplicitFence),
            Err(InteropError::UnsupportedSynchronization(
                SyncMechanism::ExplicitFence
            ))
        ));
    }

    #[test]
    fn dmabuf_frame_reports_kind_and_sync() {
        let frame = NativeFrame::DmaBufImage(DmaBufImage {
            size: PhysicalSize::new(16, 16),
            format: wgpu::TextureFormat::Bgra8Unorm,
            drm_format: 0x34325241,
            drm_modifier: 0,
            planes: vec![DmaBufPlane {
                fd: -1,
                offset: 0,
                stride: 64,
            }],
            generation: 1,
            producer_sync: SyncMechanism::ExplicitExternalSemaphore,
            semaphore_fd: Some(-1),
        });

        assert_eq!(frame.kind(), NativeFrameKind::DmaBufImage);
        assert_eq!(
            frame.producer_sync(),
            SyncMechanism::ExplicitExternalSemaphore
        );
    }
}

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
    /// The operation exists, but the backend drops or cannot represent part
    /// of the requested contract. The detail is deliberately public so a
    /// host can choose a fallback without probing by trial and error.
    Partial(&'static str),
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
/// the safe constructor takes ownership of the exported handle and closes it
/// when the frame's RAII custody is dropped. The importer opens its own D3D12
/// reference via `ID3D12Device::OpenSharedHandle`.
#[derive(Debug)]
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
    /// Shared RAII custody of the NT handle. Windows only.
    #[cfg(target_os = "windows")]
    pub(crate) resource: grafting::Dx12SharedResource,
}

#[cfg(target_os = "windows")]
pub use grafting::Dx12SharedResource;

impl Dx12SharedTexture {
    #[cfg(target_os = "windows")]
    pub(crate) fn from_resource(
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        generation: u64,
        producer_sync: SyncMechanism,
        fence_value: u64,
        resource: grafting::Dx12SharedResource,
    ) -> Self {
        Self {
            size,
            format,
            generation,
            producer_sync,
            fence_value,
            resource,
        }
    }

    #[cfg(target_os = "windows")]
    pub fn from_owned_handle(
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        generation: u64,
        producer_sync: SyncMechanism,
        fence_value: u64,
        handle: std::os::windows::io::OwnedHandle,
    ) -> Self {
        Self::from_resource(
            size,
            format,
            generation,
            producer_sync,
            fence_value,
            grafting::Dx12SharedResource::from_owned_handle(handle),
        )
    }

    /// Construct a frame from a raw handle whose ownership is transferred.
    ///
    /// # Safety
    ///
    /// `handle` must be a valid, non-null NT handle with one close obligation
    /// owned by the caller. Scry takes that obligation and closes it when the
    /// frame's RAII custody is dropped.
    #[cfg(target_os = "windows")]
    pub unsafe fn from_raw_owned_handle(
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        generation: u64,
        producer_sync: SyncMechanism,
        fence_value: u64,
        handle: *mut std::ffi::c_void,
    ) -> Option<Self> {
        if handle.is_null() {
            return None;
        }
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        Some(Self::from_owned_handle(
            size,
            format,
            generation,
            producer_sync,
            fence_value,
            handle,
        ))
    }
}

/// A frame backed by an `MTLTexture` from a Metal producer.
///
/// The producer is responsible for creating the `MTLTexture` (typically
/// by bridging an `IOSurfaceRef` from ScreenCaptureKit's
/// `CMSampleBuffer` through
/// `[MTLDevice newTextureWithDescriptor:iosurface:plane:]`). The safe
/// constructor takes a retained Objective-C reference, and the frame releases
/// that retain when dropped. Producers that keep using the same texture must
/// retain their own reference as well.
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
    /// Retained `MTLTexture` custody. Apple platforms only.
    #[cfg(target_os = "macos")]
    pub(crate) raw_metal_texture:
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLTexture>>,
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

#[cfg(target_os = "macos")]
impl MetalTextureRef {
    pub fn from_retained(
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        generation: u64,
        producer_sync: SyncMechanism,
        signal_value: u64,
        raw_metal_texture: objc2::rc::Retained<
            objc2::runtime::ProtocolObject<dyn objc2_metal::MTLTexture>,
        >,
    ) -> Self {
        Self {
            size,
            format,
            generation,
            producer_sync,
            raw_metal_texture,
            signal_value,
        }
    }

    /// Construct a frame from a raw `MTLTexture *` with one transferred retain.
    ///
    /// # Safety
    ///
    /// `raw_metal_texture` must be a non-null pointer with a retain owned by
    /// the caller. Scry takes that retain and releases it when the frame is
    /// dropped.
    pub unsafe fn from_raw_retained(
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        generation: u64,
        producer_sync: SyncMechanism,
        signal_value: u64,
        raw_metal_texture: *mut std::ffi::c_void,
    ) -> Option<Self> {
        let raw_metal_texture = unsafe {
            objc2::rc::Retained::from_raw(
                raw_metal_texture
                    .cast::<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLTexture>>(),
            )
        }?;
        Some(Self::from_retained(
            size,
            format,
            generation,
            producer_sync,
            signal_value,
            raw_metal_texture,
        ))
    }
}

/// Copyable metadata for one plane of a Linux DMABUF image.
///
/// `buffer_index` selects an owned descriptor in the containing
/// [`DmaBufImage`]. A standalone plane never owns a descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaBufPlane {
    buffer_index: usize,
    offset: u32,
    stride: u32,
}

impl DmaBufPlane {
    pub fn new(buffer_index: usize, offset: u32, stride: u32) -> Self {
        Self {
            buffer_index,
            offset,
            stride,
        }
    }

    pub fn buffer_index(&self) -> usize {
        self.buffer_index
    }

    pub fn offset(&self) -> u32 {
        self.offset
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }
}

/// A Linux WPE frame exported as DMABUF planes.
#[derive(Debug)]
pub struct DmaBufImage {
    pub size: PhysicalSize<u32>,
    pub format: wgpu::TextureFormat,
    pub drm_format: u32,
    pub drm_modifier: u64,
    planes: Vec<DmaBufPlane>,
    pub generation: u64,
    pub producer_sync: SyncMechanism,
    #[cfg(target_os = "linux")]
    buffers: Vec<std::os::fd::OwnedFd>,
    #[cfg(target_os = "linux")]
    semaphore: Option<std::os::fd::OwnedFd>,
}

impl DmaBufImage {
    pub fn planes(&self) -> &[DmaBufPlane] {
        &self.planes
    }
}

#[cfg(target_os = "linux")]
impl DmaBufImage {
    /// Build a frame from an owned descriptor table and copyable plane
    /// metadata. Every supplied descriptor is closed if validation fails.
    pub fn from_owned_buffers(
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        drm_format: u32,
        drm_modifier: u64,
        buffers: Vec<std::os::fd::OwnedFd>,
        planes: Vec<DmaBufPlane>,
        generation: u64,
        producer_sync: SyncMechanism,
        semaphore: Option<std::os::fd::OwnedFd>,
    ) -> Result<Self, InteropError> {
        if size.width == 0 || size.height == 0 {
            return Err(InteropError::InvalidFrame("DMABUF image has zero size"));
        }
        if planes.is_empty() {
            return Err(InteropError::InvalidFrame("DMABUF image has no planes"));
        }
        if buffers.is_empty() {
            return Err(InteropError::InvalidFrame(
                "DMABUF image has no descriptor buffers",
            ));
        }
        if planes
            .iter()
            .any(|plane| plane.buffer_index >= buffers.len())
        {
            return Err(InteropError::InvalidFrame(
                "DMABUF plane buffer index is out of range",
            ));
        }
        Ok(Self {
            size,
            format,
            drm_format,
            drm_modifier,
            planes,
            generation,
            producer_sync,
            buffers,
            semaphore,
        })
    }

    /// Build a frame by transferring owned raw descriptors.
    ///
    /// # Safety
    ///
    /// Every non-negative descriptor must be uniquely owned by the caller.
    /// This function takes all descriptors immediately and closes every valid
    /// one if validation fails. Repeating a numeric descriptor is invalid.
    pub unsafe fn from_owned_raw_buffers(
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        drm_format: u32,
        drm_modifier: u64,
        raw_buffers: Vec<i32>,
        planes: Vec<DmaBufPlane>,
        generation: u64,
        producer_sync: SyncMechanism,
        semaphore_fd: Option<i32>,
    ) -> Result<Self, InteropError> {
        use std::os::fd::FromRawFd;

        let mut custody = RawFdCustody::new(&raw_buffers, semaphore_fd);
        if raw_buffers.iter().any(|fd| *fd < 0) || semaphore_fd.is_some_and(|fd| fd < 0) {
            return Err(InteropError::InvalidFrame(
                "owned raw descriptors must be non-negative",
            ));
        }
        let mut unique = raw_buffers.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() != raw_buffers.len() || semaphore_fd.is_some_and(|fd| unique.contains(&fd))
        {
            return Err(InteropError::InvalidFrame(
                "owned raw descriptors must not alias",
            ));
        }
        let buffers = raw_buffers
            .into_iter()
            .map(|fd| {
                custody.disarm(fd);
                unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
            })
            .collect();
        let semaphore = semaphore_fd.map(|fd| {
            custody.disarm(fd);
            unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
        });
        Self::from_owned_buffers(
            size,
            format,
            drm_format,
            drm_modifier,
            buffers,
            planes,
            generation,
            producer_sync,
            semaphore,
        )
    }

    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }

    /// Borrow one descriptor from the owned buffer table. The returned
    /// descriptor cannot outlive the frame and must not be closed by callers.
    pub fn buffer_fd(&self, buffer_index: usize) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.buffers.get(buffer_index).map(|fd| fd.as_fd())
    }

    /// Borrow the semaphore descriptor for diagnostics. The frame retains
    /// ownership and callers must not close it.
    pub fn semaphore_fd(&self) -> Option<i32> {
        use std::os::fd::AsRawFd;
        self.semaphore.as_ref().map(|fd| fd.as_raw_fd())
    }

    pub(crate) fn set_generation(&mut self, generation: u64) {
        self.generation = generation;
    }

    pub(crate) fn set_producer_sync(&mut self, producer_sync: SyncMechanism) {
        self.producer_sync = producer_sync;
    }

    pub(crate) fn into_graft_import(
        self,
        drm_modifier: u64,
    ) -> Result<
        (
            grafting::vulkan_dmabuf::VulkanDmaBufImport,
            Option<std::os::fd::OwnedFd>,
        ),
        InteropError,
    > {
        let Self {
            size,
            format,
            drm_format,
            drm_modifier: _original_drm_modifier,
            planes,
            semaphore,
            buffers,
            ..
        } = self;
        let graft_planes = planes
            .iter()
            .map(|plane| grafting::vulkan_dmabuf::VulkanDmaBufPlane {
                buffer_index: plane.buffer_index,
                offset: u64::from(plane.offset),
                stride: u64::from(plane.stride),
            })
            .collect();
        let frame = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
            size,
            format,
            drm_format,
            drm_modifier,
            buffers,
            graft_planes,
            grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
        )
        .map_err(|error| InteropError::Vulkan(error.to_string()))?;
        Ok((frame, semaphore))
    }
}

#[cfg(target_os = "linux")]
struct RawFdCustody {
    raw_fds: Vec<i32>,
}

#[cfg(target_os = "linux")]
impl RawFdCustody {
    fn new(raw_buffers: &[i32], semaphore_fd: Option<i32>) -> Self {
        let mut raw_fds = raw_buffers
            .iter()
            .copied()
            .chain(semaphore_fd)
            .filter(|fd| *fd >= 0)
            .collect::<Vec<_>>();
        raw_fds.sort_unstable();
        raw_fds.dedup();
        Self { raw_fds }
    }

    fn disarm(&mut self, raw_fd: i32) {
        if let Some(index) = self
            .raw_fds
            .iter()
            .position(|candidate| *candidate == raw_fd)
        {
            self.raw_fds.swap_remove(index);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for RawFdCustody {
    fn drop(&mut self) {
        for raw_fd in &self.raw_fds {
            unsafe { libc::close(*raw_fd) };
        }
    }
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
        frame: NativeFrame,
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
        frame: NativeFrame,
        _options: &ImportOptions,
    ) -> Result<ImportedTexture, InteropError> {
        let producer_sync = frame.producer_sync();
        self.synchronizer.producer_complete(&frame, producer_sync)?;

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
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] frame: DmaBufImage,
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
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))] frame: Dx12SharedTexture,
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
        // shared interop core). The frame owns a shared RAII custody token, and
        // the Graft frame consumes a clone of that token for the import.
        let producer_sync = match frame.producer_sync {
            SyncMechanism::ExplicitFence => grafting::SyncMechanism::ExplicitFence,
            SyncMechanism::None => grafting::SyncMechanism::None,
            other => return Err(InteropError::UnsupportedSynchronization(other)),
        };
        let metadata = grafting::FrameMetadata {
            size: frame.size,
            format: frame.format,
            generation: frame.generation,
            producer_sync,
        };
        let g_host = grafting::HostWgpuContext::new(host.device.clone(), host.queue.clone());
        let g_frame =
            grafting::Dx12SharedTexture::new(metadata, frame.resource.clone(), frame.fence_value);
        let texture = grafting::import_dx12_shared_texture(g_frame, &g_host)
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
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] frame: MetalTextureRef,
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
    #[cfg(target_os = "linux")]
    fn dmabuf_frame_reports_kind_and_sync() {
        use std::os::fd::FromRawFd;

        let _fd_lock = crate::lock_fd_table();
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { libc::close(fds[1]) };
        let buffer = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };
        let frame = NativeFrame::DmaBufImage(
            DmaBufImage::from_owned_buffers(
                PhysicalSize::new(16, 16),
                wgpu::TextureFormat::Bgra8Unorm,
                0x34325241,
                0,
                vec![buffer],
                vec![DmaBufPlane::new(0, 0, 64)],
                1,
                SyncMechanism::ExplicitExternalSemaphore,
                None,
            )
            .expect("valid owned DMABUF frame"),
        );

        assert_eq!(frame.kind(), NativeFrameKind::DmaBufImage);
        assert_eq!(
            frame.producer_sync(),
            SyncMechanism::ExplicitExternalSemaphore
        );
    }
}

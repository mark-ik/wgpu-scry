// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Windows WebView2 capture planning.
//!
//! The target path is WebView2 `ICoreWebView2CompositionController` plus
//! `Windows.Graphics.Capture`. Capture frames arrive as D3D11 textures; the
//! adapter must bridge them into a D3D12 shared texture before handing them to
//! `wgpu-native-texture-interop`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::os::windows::io::FromRawHandle;

use crate::native_frame::{Dx12SharedTexture, NativeFrame, SyncMechanism};
use dpi::PhysicalSize;
use windows::Win32::{
    Foundation::{HANDLE, HMODULE, HWND},
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED,
            D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
            D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice,
            ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence,
            ID3D11Texture2D,
        },
        Dxgi::{
            Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC},
            DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIDevice, IDXGIResource1,
        },
    },
    System::WinRT::{
        Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
        Graphics::Capture::IGraphicsCaptureItemInterop,
    },
};
use windows::{
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    UI::Composition::Visual,
    core::{Interface, PCWSTR},
};

use crate::{WebSurfaceError, WebSurfaceFrame};

/// Metadata for a captured WebView2 frame before it has been converted into a
/// `NativeFrame::Dx12SharedTexture`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebView2D3D11CaptureFrame {
    pub size: PhysicalSize<u32>,
    pub format: wgpu::TextureFormat,
    pub generation: u64,
    /// Raw `ID3D11Texture2D *`. The capture owner retains lifetime.
    pub raw_d3d11_texture: *mut std::ffi::c_void,
}

/// Owns a D3D11 device that can allocate NT-handle-shareable textures.
///
/// This is not the final WebView2 capture producer. It is the reusable helper
/// the producer needs once it receives `Direct3D11CaptureFrame.Surface` from
/// `Windows.Graphics.Capture`: either export a compatible capture texture
/// directly or copy the capture texture into a texture allocated here.
///
/// Two cross-API sync paths are supported:
///
/// - **Keyed-mutex (default)** — the destination textures are allocated with
///   `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`; per-frame copies bracket
///   `CopyResource` with `AcquireSync`/`ReleaseSync` and a CPU-side
///   `D3D11_QUERY_EVENT` spin to drain the GPU before handoff. This is the
///   path you get from [`new_hardware`](Self::new_hardware).
/// - **Explicit fence** — a `D3D12_FENCE_FLAG_SHARED` fence created on the
///   consumer's wgpu D3D12 device is opened on the producer's D3D11 device
///   via `ID3D11Device5::OpenSharedFence`; per-frame copies signal the
///   fence after `CopyResource` and the consumer's
///   `Dx12FenceSynchronizer::producer_complete` queues
///   `ID3D12CommandQueue::Wait` before its next submit. This is the path
///   you get from [`new_hardware_with_fence`](Self::new_hardware_with_fence).
///
/// Fence mode drops the keyed-mutex flag from the destination texture
/// allocation and removes the producer-side CPU spin.
#[derive(Clone, Debug)]
pub struct D3D11SharedTextureFactory {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    fence: Option<Arc<FenceMode>>,
}

#[derive(Debug)]
struct FenceMode {
    #[allow(dead_code)]
    device5: ID3D11Device5,
    context4: ID3D11DeviceContext4,
    signaler: D3D11FenceSignaler,
}

#[derive(Debug)]
struct D3D11FenceSignaler {
    fence: ID3D11Fence,
    next_value: AtomicU64,
}

impl D3D11FenceSignaler {
    fn open_from_shared_handle(
        device5: &ID3D11Device5,
        shared_handle: *mut std::ffi::c_void,
    ) -> Result<Self, WebSurfaceError> {
        if shared_handle.is_null() {
            return Err(WebSurfaceError::Platform(
                "fence shared handle was null".to_string(),
            ));
        }
        let mut fence: Option<ID3D11Fence> = None;
        unsafe {
            device5
                .OpenSharedFence(HANDLE(shared_handle), &mut fence)
                .map_err(|error| {
                    WebSurfaceError::Platform(format!(
                        "ID3D11Device5::OpenSharedFence failed: {error}"
                    ))
                })?;
        }
        let fence = fence.ok_or_else(|| {
            WebSurfaceError::Platform(
                "ID3D11Device5::OpenSharedFence returned null fence".to_string(),
            )
        })?;
        Ok(Self {
            fence,
            next_value: AtomicU64::new(0),
        })
    }

    fn signal(&self, ctx4: &ID3D11DeviceContext4) -> Result<u64, WebSurfaceError> {
        let value = self.next_value.fetch_add(1, Ordering::SeqCst) + 1;
        unsafe {
            ctx4.Signal(&self.fence, value).map_err(|error| {
                WebSurfaceError::Platform(format!(
                    "ID3D11DeviceContext4::Signal({value}) failed: {error}"
                ))
            })?;
        }
        Ok(value)
    }
}

impl D3D11SharedTextureFactory {
    pub fn new_hardware() -> Result<Self, WebSurfaceError> {
        let (device, context) = create_d3d11_hardware_device()?;
        Ok(Self {
            device,
            context,
            fence: None,
        })
    }

    /// Same as [`new_hardware`](Self::new_hardware) but enables explicit-fence
    /// mode using the consumer-supplied shared fence handle (typically from
    /// `Dx12FenceSynchronizer::shared_handle`). Per-frame copies will signal
    /// the fence after `CopyResource` instead of acquiring/releasing a keyed
    /// mutex and CPU-spinning.
    pub fn new_hardware_with_fence(
        fence_shared_handle: *mut std::ffi::c_void,
    ) -> Result<Self, WebSurfaceError> {
        let (device, context) = create_d3d11_hardware_device()?;
        let device5 = device.cast::<ID3D11Device5>().map_err(|error| {
            WebSurfaceError::Platform(format!(
                "ID3D11Device cast to ID3D11Device5 failed (Windows 10 1809+ required): {error}"
            ))
        })?;
        let context4 = context.cast::<ID3D11DeviceContext4>().map_err(|error| {
            WebSurfaceError::Platform(format!(
                "ID3D11DeviceContext cast to ID3D11DeviceContext4 failed: {error}"
            ))
        })?;
        let signaler = D3D11FenceSignaler::open_from_shared_handle(&device5, fence_shared_handle)?;
        Ok(Self {
            device,
            context,
            fence: Some(Arc::new(FenceMode {
                device5,
                context4,
                signaler,
            })),
        })
    }

    /// Returns the [`SyncMechanism`] this factory is configured for. Producers
    /// stamp this onto frames so the consumer's synchronizer knows whether to
    /// expect a fence wait.
    pub fn sync_mechanism(&self) -> SyncMechanism {
        if self.fence.is_some() {
            SyncMechanism::ExplicitFence
        } else {
            SyncMechanism::None
        }
    }

    pub fn create_shared_texture_frame(
        &self,
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        generation: u64,
    ) -> Result<WebView2DxgiSharedHandleFrame, WebSurfaceError> {
        Ok(self
            .create_shared_texture(size, format, generation)?
            .shared_frame)
    }

    pub(crate) fn create_shared_texture(
        &self,
        size: PhysicalSize<u32>,
        format: wgpu::TextureFormat,
        generation: u64,
    ) -> Result<D3D11SharedTexture, WebSurfaceError> {
        let dxgi_format = dxgi_format_for_wgpu(format)?;
        // D3D11_RESOURCE_MISC_SHARED_NTHANDLE requires either SHARED or
        // SHARED_KEYEDMUTEX alongside it — NT-handle sharing isn't valid
        // on its own. Fence mode pairs NTHANDLE with the bare SHARED
        // bit (ordering comes from the explicit fence wait); keyed-mutex
        // mode pairs it with SHARED_KEYEDMUTEX so producer/consumer
        // serialize through AcquireSync/ReleaseSync.
        let mut misc_flags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32;
        if self.fence.is_none() {
            misc_flags |= D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32;
        } else {
            misc_flags |= D3D11_RESOURCE_MISC_SHARED.0 as u32;
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.width,
            Height: size.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: dxgi_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc_flags,
        };

        let mut texture = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(
            |error| WebSurfaceError::Platform(format!("CreateTexture2D failed: {error}")),
        )?;

        let texture = texture.ok_or_else(|| {
            WebSurfaceError::Platform("CreateTexture2D returned no texture".to_string())
        })?;

        let shared_frame = shared_handle_from_texture(&texture, size, format, generation)?;
        Ok(D3D11SharedTexture {
            texture,
            shared_frame,
        })
    }

    pub fn create_winrt_direct3d_device(&self) -> Result<IDirect3DDevice, WebSurfaceError> {
        let dxgi_device = self.device.cast::<IDXGIDevice>().map_err(|error| {
            WebSurfaceError::Platform(format!("ID3D11Device cast to IDXGIDevice failed: {error}"))
        })?;
        let inspectable =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }.map_err(|error| {
                WebSurfaceError::Platform(format!(
                    "CreateDirect3D11DeviceFromDXGIDevice failed: {error}"
                ))
            })?;
        inspectable.cast::<IDirect3DDevice>().map_err(|error| {
            WebSurfaceError::Platform(format!("IDirect3DDevice cast failed: {error}"))
        })
    }

    pub fn copy_capture_into_shared_frame(
        &self,
        capture: WebView2D3D11CaptureFrame,
    ) -> Result<WebView2DxgiSharedHandleFrame, WebSurfaceError> {
        let target =
            self.create_shared_texture(capture.size, capture.format, capture.generation)?;
        let fence_value = self.copy_capture_into_existing_target(&target.texture, capture)?;
        Ok(WebView2DxgiSharedHandleFrame {
            producer_sync: self.sync_mechanism(),
            fence_value,
            ..target.shared_frame
        })
    }

    /// Copy the captured D3D11 texture into the destination shared texture and
    /// synchronize per this factory's mode.
    ///
    /// - **Keyed-mutex mode**: `AcquireSync(0)` → `CopyResource` →
    ///   `flush_and_wait_for_gpu` (CPU spin on `D3D11_QUERY_EVENT`) →
    ///   `ReleaseSync(0)`. Returns `0` (no fence).
    /// - **Fence mode**: `CopyResource` → `Signal(fence, value)` on the
    ///   immediate context (no CPU spin; the wgpu D3D12 consumer waits via
    ///   `Dx12FenceSynchronizer`). Returns the signalled fence value.
    pub(crate) fn copy_capture_into_existing_target(
        &self,
        target: &ID3D11Texture2D,
        capture: WebView2D3D11CaptureFrame,
    ) -> Result<u64, WebSurfaceError> {
        match self.fence.as_ref() {
            Some(fence_mode) => self.copy_with_fence(target, capture, fence_mode),
            None => self.copy_with_keyed_mutex(target, capture).map(|()| 0),
        }
    }

    fn copy_with_keyed_mutex(
        &self,
        target: &ID3D11Texture2D,
        capture: WebView2D3D11CaptureFrame,
    ) -> Result<(), WebSurfaceError> {
        let target_mutex = target
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex>()
            .map_err(|error| {
                WebSurfaceError::Platform(format!(
                    "destination texture cast to IDXGIKeyedMutex failed: {error}"
                ))
            })?;
        // Bound the keyed-mutex acquire so that if the consumer (or anything
        // else) is somehow holding key 0, we surface a clean error instead
        // of wedging the producer thread forever.
        const ACQUIRE_TIMEOUT_MS: u32 = 500;
        let acquire_hr = unsafe { target_mutex.AcquireSync(0, ACQUIRE_TIMEOUT_MS) };
        if let Err(error) = acquire_hr {
            return Err(WebSurfaceError::Platform(format!(
                "AcquireSync(0, {ACQUIRE_TIMEOUT_MS}ms) on shared dest failed/timed out: {error}"
            )));
        }

        let copy_result = with_borrowed_d3d11_texture(capture.raw_d3d11_texture, |source| {
            unsafe {
                self.context.CopyResource(target, source);
            }
            Ok(())
        });

        // Wait for the GPU to finish the copy before releasing the keyed mutex
        // and handing the shared NT handle to the D3D12 consumer.
        let sync_result = self.flush_and_wait_for_gpu();

        let release_result = unsafe { target_mutex.ReleaseSync(0) }.map_err(|error| {
            WebSurfaceError::Platform(format!("ReleaseSync(0) on shared dest failed: {error}"))
        });

        copy_result?;
        sync_result?;
        release_result?;
        Ok(())
    }

    fn copy_with_fence(
        &self,
        target: &ID3D11Texture2D,
        capture: WebView2D3D11CaptureFrame,
        fence_mode: &FenceMode,
    ) -> Result<u64, WebSurfaceError> {
        with_borrowed_d3d11_texture(capture.raw_d3d11_texture, |source| {
            unsafe {
                self.context.CopyResource(target, source);
            }
            Ok(())
        })?;
        // Order matters: CopyResource queues the copy, Signal queues a fence
        // signal after it, Flush commits the batch to the GPU. The signal
        // becomes visible to the consumer's `ID3D12CommandQueue::Wait` only
        // after the GPU drains the copy.
        let value = fence_mode.signaler.signal(&fence_mode.context4)?;
        unsafe { self.context.Flush() };
        Ok(value)
    }

    fn flush_and_wait_for_gpu(&self) -> Result<(), WebSurfaceError> {
        let mut query = None;
        unsafe {
            self.device
                .CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                )
                .map_err(|error| {
                    WebSurfaceError::Platform(format!(
                        "CreateQuery(D3D11_QUERY_EVENT) failed: {error}"
                    ))
                })?;
        }
        let query = query.ok_or_else(|| {
            WebSurfaceError::Platform("CreateQuery returned no query".to_string())
        })?;

        unsafe {
            self.context.End(&query);
            self.context.Flush();
        }

        // Spin until the GPU finishes everything queued up to End().
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let mut data: u32 = 0;
            let result = unsafe {
                self.context.GetData(
                    &query,
                    Some(&mut data as *mut _ as *mut std::ffi::c_void),
                    std::mem::size_of::<u32>() as u32,
                    0,
                )
            };
            if result.is_ok() {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(WebSurfaceError::Platform(
                    "D3D11 GPU sync (event query) timed out after 2s".to_string(),
                ));
            }
            std::thread::yield_now();
        }
    }
}

fn create_d3d11_hardware_device() -> Result<(ID3D11Device, ID3D11DeviceContext), WebSurfaceError> {
    let mut device = None;
    let mut context = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();
    let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
    }
    .map_err(|error| WebSurfaceError::Platform(format!("D3D11CreateDevice failed: {error}")))?;

    let device = device.ok_or_else(|| {
        WebSurfaceError::Platform("D3D11CreateDevice returned no device".to_string())
    })?;
    let context = context.ok_or_else(|| {
        WebSurfaceError::Platform("D3D11CreateDevice returned no immediate context".to_string())
    })?;
    Ok((device, context))
}

#[derive(Debug)]
pub(crate) struct D3D11SharedTexture {
    pub(crate) texture: ID3D11Texture2D,
    pub(crate) shared_frame: WebView2DxgiSharedHandleFrame,
}

/// Result of probing the Windows.Graphics.Capture side of the pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsCaptureProbe {
    pub session_supported: bool,
    pub winrt_d3d_device_created: bool,
    pub free_threaded_frame_pool_created: bool,
}

pub fn probe_graphics_capture_prerequisites() -> Result<GraphicsCaptureProbe, WebSurfaceError> {
    let session_supported = GraphicsCaptureSession::IsSupported().map_err(|error| {
        WebSurfaceError::Platform(format!(
            "GraphicsCaptureSession::IsSupported failed: {error}"
        ))
    })?;
    if !session_supported {
        return Ok(GraphicsCaptureProbe {
            session_supported,
            winrt_d3d_device_created: false,
            free_threaded_frame_pool_created: false,
        });
    }

    let factory = D3D11SharedTextureFactory::new_hardware()?;
    let device = factory.create_winrt_direct3d_device()?;
    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        1,
        SizeInt32 {
            Width: 64,
            Height: 64,
        },
    )
    .map_err(|error| {
        WebSurfaceError::Platform(format!(
            "Direct3D11CaptureFramePool::CreateFreeThreaded failed: {error}"
        ))
    })?;
    drop(frame_pool);

    Ok(GraphicsCaptureProbe {
        session_supported,
        winrt_d3d_device_created: true,
        free_threaded_frame_pool_created: true,
    })
}

#[derive(Debug)]
pub struct CapturedWindowFrame {
    pub shared_frame: WebView2DxgiSharedHandleFrame,
    pub content_size: PhysicalSize<u32>,
}

/// Capture one frame from a HWND using Windows.Graphics.Capture.
///
/// This is a stand-in for the WebView2 CompositionController visual path. It
/// proves the downstream frame-pool and D3D11 texture extraction machinery
/// before we substitute `GraphicsCaptureItem::CreateFromVisual`.
///
/// # Safety
///
/// `hwnd` must be a valid live window handle for the duration of the call.
pub unsafe fn capture_window_frame_once(
    hwnd: *mut std::ffi::c_void,
    timeout: std::time::Duration,
) -> Result<CapturedWindowFrame, WebSurfaceError> {
    if hwnd.is_null() {
        return Err(WebSurfaceError::Platform(
            "window capture HWND was null".to_string(),
        ));
    }

    let item = create_capture_item_for_hwnd(HWND(hwnd))?;
    capture_graphics_item_frame_once(&item, timeout)
}

/// Capture one frame from a Windows.UI.Composition visual.
///
/// This is the handoff shape expected from a WebView2 composition controller
/// once the host can obtain the webview visual.
///
/// # Safety
///
/// `visual` must be a valid live `Windows.UI.Composition.Visual *` for the
/// duration of the call.
pub unsafe fn capture_visual_frame_once(
    visual: *mut std::ffi::c_void,
    timeout: std::time::Duration,
) -> Result<CapturedWindowFrame, WebSurfaceError> {
    unsafe { capture_visual_frame_once_after_start(visual, timeout, || Ok(())) }
}

/// Capture one frame from a Windows.UI.Composition visual after running a hook
/// immediately after `GraphicsCaptureSession::StartCapture`.
///
/// This is a diagnostic helper for visual hosts that may need to invalidate or
/// repaint content after the capture session starts before a frame is emitted.
///
/// # Safety
///
/// `visual` must be a valid live `Windows.UI.Composition.Visual *` for the
/// duration of the call.
pub unsafe fn capture_visual_frame_once_after_start(
    visual: *mut std::ffi::c_void,
    timeout: std::time::Duration,
    after_start: impl FnOnce() -> Result<(), WebSurfaceError>,
) -> Result<CapturedWindowFrame, WebSurfaceError> {
    if visual.is_null() {
        return Err(WebSurfaceError::Platform(
            "composition visual pointer was null".to_string(),
        ));
    }

    with_borrowed_composition_visual(visual, |visual| {
        let item = GraphicsCaptureItem::CreateFromVisual(visual).map_err(|error| {
            WebSurfaceError::Platform(format!(
                "GraphicsCaptureItem::CreateFromVisual failed: {error}"
            ))
        })?;
        capture_graphics_item_frame_once_after_start(&item, timeout, after_start)
    })
}

/// Return the Windows.Graphics.Capture item size for a composition visual.
///
/// This diagnostic mirrors the first steps of `capture_visual_frame_once` so a
/// host can distinguish visual-to-item failures from frame-pool starvation.
///
/// # Safety
///
/// `visual` must be a valid live `Windows.UI.Composition.Visual *` for the
/// duration of the call.
pub unsafe fn capture_visual_item_size(
    visual: *mut std::ffi::c_void,
) -> Result<PhysicalSize<u32>, WebSurfaceError> {
    if visual.is_null() {
        return Err(WebSurfaceError::Platform(
            "composition visual pointer was null".to_string(),
        ));
    }

    with_borrowed_composition_visual(visual, |visual| {
        let item = GraphicsCaptureItem::CreateFromVisual(visual).map_err(|error| {
            WebSurfaceError::Platform(format!(
                "GraphicsCaptureItem::CreateFromVisual failed: {error}"
            ))
        })?;
        let item_size = item.Size().map_err(|error| {
            WebSurfaceError::Platform(format!("GraphicsCaptureItem::Size failed: {error}"))
        })?;
        if item_size.Width <= 0 || item_size.Height <= 0 {
            return Err(WebSurfaceError::Platform(format!(
                "GraphicsCaptureItem returned invalid size {}x{}",
                item_size.Width, item_size.Height
            )));
        }
        Ok(PhysicalSize::new(
            item_size.Width as u32,
            item_size.Height as u32,
        ))
    })
}

pub fn capture_graphics_item_frame_once(
    item: &GraphicsCaptureItem,
    timeout: std::time::Duration,
) -> Result<CapturedWindowFrame, WebSurfaceError> {
    capture_graphics_item_frame_once_after_start(item, timeout, || Ok(()))
}

fn capture_graphics_item_frame_once_after_start(
    item: &GraphicsCaptureItem,
    timeout: std::time::Duration,
    after_start: impl FnOnce() -> Result<(), WebSurfaceError>,
) -> Result<CapturedWindowFrame, WebSurfaceError> {
    let session_supported = GraphicsCaptureSession::IsSupported().map_err(|error| {
        WebSurfaceError::Platform(format!(
            "GraphicsCaptureSession::IsSupported failed: {error}"
        ))
    })?;
    if !session_supported {
        return Err(WebSurfaceError::Unsupported(
            "Windows.Graphics.Capture is not supported in this session",
        ));
    }

    let item_size = item.Size().map_err(|error| {
        WebSurfaceError::Platform(format!("GraphicsCaptureItem::Size failed: {error}"))
    })?;
    if item_size.Width <= 0 || item_size.Height <= 0 {
        return Err(WebSurfaceError::Platform(format!(
            "GraphicsCaptureItem returned invalid size {}x{}",
            item_size.Width, item_size.Height
        )));
    }

    let factory = D3D11SharedTextureFactory::new_hardware()?;
    let device = factory.create_winrt_direct3d_device()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        item_size,
    )
    .map_err(|error| {
        WebSurfaceError::Platform(format!(
            "Direct3D11CaptureFramePool::CreateFreeThreaded failed: {error}"
        ))
    })?;
    let session = pool.CreateCaptureSession(item).map_err(|error| {
        WebSurfaceError::Platform(format!("CreateCaptureSession failed: {error}"))
    })?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetIsBorderRequired(false);
    session
        .StartCapture()
        .map_err(|error| WebSurfaceError::Platform(format!("StartCapture failed: {error}")))?;
    after_start()?;

    let deadline = std::time::Instant::now() + timeout;
    let frame = loop {
        match pool.TryGetNextFrame() {
            Ok(frame) => break frame,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            Err(error) => {
                let _ = session.Close();
                let _ = pool.Close();
                return Err(WebSurfaceError::Platform(format!(
                    "TryGetNextFrame timed out after {timeout:?} for capture item {}x{}; last poll returned {error}",
                    item_size.Width, item_size.Height
                )));
            }
        }
    };

    let content_size = frame.ContentSize().map_err(|error| {
        WebSurfaceError::Platform(format!(
            "Direct3D11CaptureFrame::ContentSize failed: {error}"
        ))
    })?;
    let surface = frame.Surface().map_err(|error| {
        WebSurfaceError::Platform(format!("Direct3D11CaptureFrame::Surface failed: {error}"))
    })?;
    let access = surface
        .cast::<IDirect3DDxgiInterfaceAccess>()
        .map_err(|error| {
            WebSurfaceError::Platform(format!(
                "IDirect3DSurface cast to IDirect3DDxgiInterfaceAccess failed: {error}"
            ))
        })?;
    let texture = unsafe { access.GetInterface::<ID3D11Texture2D>() }.map_err(|error| {
        WebSurfaceError::Platform(format!(
            "IDirect3DDxgiInterfaceAccess::GetInterface<ID3D11Texture2D> failed: {error}"
        ))
    })?;

    let raw_texture = Interface::as_raw(&texture);
    let shared_frame = factory.copy_capture_into_shared_frame(WebView2D3D11CaptureFrame {
        size: PhysicalSize::new(content_size.Width as u32, content_size.Height as u32),
        format: wgpu::TextureFormat::Bgra8Unorm,
        generation: 1,
        raw_d3d11_texture: raw_texture,
    })?;

    let _ = frame.Close();
    let _ = session.Close();
    let _ = pool.Close();

    Ok(CapturedWindowFrame {
        shared_frame,
        content_size: PhysicalSize::new(content_size.Width as u32, content_size.Height as u32),
    })
}

fn create_capture_item_for_hwnd(hwnd: HWND) -> Result<GraphicsCaptureItem, WebSurfaceError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|error| {
            WebSurfaceError::Platform(format!(
                "GraphicsCaptureItem interop factory failed: {error}"
            ))
        })?;
    unsafe { interop.CreateForWindow::<GraphicsCaptureItem>(hwnd) }.map_err(|error| {
        WebSurfaceError::Platform(format!(
            "IGraphicsCaptureItemInterop::CreateForWindow failed: {error}"
        ))
    })
}

/// Result of converting a captured D3D11 frame into an importable D3D12 frame.
#[derive(Debug)]
pub struct WebView2Dx12SharedFrame {
    pub size: PhysicalSize<u32>,
    pub format: wgpu::TextureFormat,
    pub generation: u64,
    /// NT shared handle suitable for `ID3D12Device::OpenSharedHandle`.
    pub(crate) resource: grafting::Dx12SharedResource,
    /// Producer-side sync mechanism used for this frame.
    /// `SyncMechanism::None` for the keyed-mutex+CPU-spin path,
    /// `SyncMechanism::ExplicitFence` when the producer signalled a shared
    /// `ID3D11Fence` after `CopyResource`.
    pub producer_sync: SyncMechanism,
    /// Fence value the producer signalled at. Only meaningful when
    /// `producer_sync == SyncMechanism::ExplicitFence`; otherwise `0`.
    pub fence_value: u64,
}

impl WebView2Dx12SharedFrame {
    pub fn into_surface_frame(self) -> WebSurfaceFrame {
        WebSurfaceFrame::Native(NativeFrame::Dx12SharedTexture(
            Dx12SharedTexture::from_resource(
                self.size,
                self.format,
                self.generation,
                self.producer_sync,
                self.fence_value,
                self.resource,
            ),
        ))
    }
}

/// A capture frame that already has a DXGI/D3D shared handle.
///
/// This is the narrow handoff shape the WebView2 capture implementation should
/// try to reach after receiving a `Direct3D11CaptureFrame`. If the captured
/// `ID3D11Texture2D` can expose a handle that `ID3D12Device::OpenSharedHandle`
/// accepts, no CPU readback is needed.
#[derive(Debug)]
pub struct WebView2DxgiSharedHandleFrame {
    pub size: PhysicalSize<u32>,
    pub format: wgpu::TextureFormat,
    pub generation: u64,
    /// Non-owning raw handle view for diagnostics. The `resource` token owns
    /// the handle for the lifetime of this frame.
    pub shared_handle: *mut std::ffi::c_void,
    pub(crate) resource: grafting::Dx12SharedResource,
    /// Sync mechanism the consumer should use; carried through to the
    /// `Dx12SharedTexture` frame.
    pub producer_sync: SyncMechanism,
    /// Fence value the producer signalled at. `0` outside fence mode.
    pub fence_value: u64,
}

impl WebView2DxgiSharedHandleFrame {
    pub fn into_dx12_frame(self) -> WebView2Dx12SharedFrame {
        WebView2Dx12SharedFrame {
            size: self.size,
            format: self.format,
            generation: self.generation,
            resource: self.resource,
            producer_sync: self.producer_sync,
            fence_value: self.fence_value,
        }
    }

    pub fn into_surface_frame(self) -> WebSurfaceFrame {
        self.into_dx12_frame().into_surface_frame()
    }
}

pub fn export_capture_frame_shared_handle(
    frame: WebView2D3D11CaptureFrame,
) -> Result<WebView2DxgiSharedHandleFrame, WebSurfaceError> {
    with_borrowed_d3d11_texture(frame.raw_d3d11_texture, |texture| {
        shared_handle_from_texture(texture, frame.size, frame.format, frame.generation)
    })
}

/// Describes the Windows proof path without owning COM/WinRT objects yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebView2CompositionCapturePlan {
    pub requires_composition_controller: bool,
    pub requires_graphics_capture_item_from_visual: bool,
    pub capture_texture_api: &'static str,
    pub import_texture_kind: &'static str,
}

impl Default for WebView2CompositionCapturePlan {
    fn default() -> Self {
        Self {
            requires_composition_controller: true,
            requires_graphics_capture_item_from_visual: true,
            capture_texture_api: "Windows.Graphics.Capture.Direct3D11CaptureFrame.Surface",
            import_texture_kind: "NativeFrame::Dx12SharedTexture",
        }
    }
}

/// Placeholder bridge for the first hard proof point.
///
/// The implementation must prove either D3D11 shared-handle import into D3D12
/// or a D3D11On12 copy into a D3D12 shared resource before the adapter can
/// honestly advertise interactive `ImportedTexture` support.
pub trait D3D11ToDx12Bridge {
    fn bridge_frame(
        &self,
        frame: WebView2D3D11CaptureFrame,
    ) -> Result<WebView2Dx12SharedFrame, WebSurfaceError>;
}

/// Bridge implementation for capture paths that can already produce a
/// D3D12-openable DXGI shared handle.
#[derive(Clone, Debug, Default)]
pub struct DxgiSharedHandleBridge;

impl DxgiSharedHandleBridge {
    pub fn bridge_shared_handle(
        &self,
        frame: WebView2DxgiSharedHandleFrame,
    ) -> Result<WebView2Dx12SharedFrame, WebSurfaceError> {
        if frame.shared_handle.is_null() {
            return Err(WebSurfaceError::Platform(
                "WebView2 capture shared handle was null".to_string(),
            ));
        }
        Ok(frame.into_dx12_frame())
    }
}

impl D3D11ToDx12Bridge for DxgiSharedHandleBridge {
    fn bridge_frame(
        &self,
        frame: WebView2D3D11CaptureFrame,
    ) -> Result<WebView2Dx12SharedFrame, WebSurfaceError> {
        self.bridge_shared_handle(export_capture_frame_shared_handle(frame)?)
    }
}

#[derive(Clone, Debug, Default)]
pub struct UnsupportedD3D11ToDx12Bridge;

impl D3D11ToDx12Bridge for UnsupportedD3D11ToDx12Bridge {
    fn bridge_frame(
        &self,
        _frame: WebView2D3D11CaptureFrame,
    ) -> Result<WebView2Dx12SharedFrame, WebSurfaceError> {
        Err(WebSurfaceError::Unsupported(
            "D3D11 capture texture to D3D12 shared texture bridge is not implemented yet",
        ))
    }
}

fn dxgi_format_for_wgpu(
    format: wgpu::TextureFormat,
) -> Result<windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, WebSurfaceError> {
    match format {
        wgpu::TextureFormat::Rgba8Unorm => Ok(DXGI_FORMAT_R8G8B8A8_UNORM),
        wgpu::TextureFormat::Bgra8Unorm => Ok(DXGI_FORMAT_B8G8R8A8_UNORM),
        _ => Err(WebSurfaceError::Unsupported(
            "only Rgba8Unorm and Bgra8Unorm D3D11 capture textures are supported",
        )),
    }
}

fn with_borrowed_d3d11_texture<R>(
    raw: *mut std::ffi::c_void,
    f: impl FnOnce(&ID3D11Texture2D) -> Result<R, WebSurfaceError>,
) -> Result<R, WebSurfaceError> {
    if raw.is_null() {
        return Err(WebSurfaceError::Platform(
            "D3D11 capture texture pointer was null".to_string(),
        ));
    }

    unsafe { ID3D11Texture2D::from_raw_borrowed(&raw) }
        .ok_or_else(|| {
            WebSurfaceError::Platform("failed to borrow ID3D11Texture2D pointer".to_string())
        })
        .and_then(f)
}

fn with_borrowed_composition_visual<R>(
    raw: *mut std::ffi::c_void,
    f: impl FnOnce(&Visual) -> Result<R, WebSurfaceError>,
) -> Result<R, WebSurfaceError> {
    if raw.is_null() {
        return Err(WebSurfaceError::Platform(
            "composition visual pointer was null".to_string(),
        ));
    }

    unsafe { Visual::from_raw_borrowed(&raw) }
        .ok_or_else(|| {
            WebSurfaceError::Platform(
                "failed to borrow Windows.UI.Composition.Visual pointer".to_string(),
            )
        })
        .and_then(f)
}

fn shared_handle_from_texture(
    texture: &ID3D11Texture2D,
    size: PhysicalSize<u32>,
    format: wgpu::TextureFormat,
    generation: u64,
) -> Result<WebView2DxgiSharedHandleFrame, WebSurfaceError> {
    let resource = texture.cast::<IDXGIResource1>().map_err(|error| {
        WebSurfaceError::Platform(format!(
            "ID3D11Texture2D cast to IDXGIResource1 failed: {error}"
        ))
    })?;

    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
            PCWSTR::null(),
        )
    }
    .map_err(|error| {
        WebSurfaceError::Platform(format!(
            "IDXGIResource1::CreateSharedHandle failed: {error}"
        ))
    })?;

    let raw_handle = handle.0 as *mut std::ffi::c_void;
    let owned_handle = unsafe {
        std::os::windows::io::OwnedHandle::from_raw_handle(raw_handle)
    };
    let resource = grafting::Dx12SharedResource::from_owned_handle(owned_handle);
    Ok(WebView2DxgiSharedHandleFrame {
        size,
        format,
        generation,
        shared_handle: raw_handle,
        resource,
        producer_sync: SyncMechanism::None,
        fence_value: 0,
    })
}

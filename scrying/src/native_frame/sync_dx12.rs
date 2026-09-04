// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! D3D12 explicit fence synchronizer for cross-API texture handoff.
//!
//! Pairs a wgpu D3D12 consumer with a separate D3D11/D3D12 producer via a
//! `D3D12_FENCE_FLAG_SHARED` fence:
//!
//! 1. The synchronizer creates a shared fence on the wgpu D3D12 device and
//!    exports an NT handle via [`ID3D12Device::CreateSharedHandle`].
//! 2. The producer opens its own reference to the fence
//!    (`ID3D11Device5::OpenSharedFence` for D3D11 producers,
//!    `ID3D12Device::OpenSharedHandle` for D3D12 producers) using
//!    [`Dx12FenceSynchronizer::shared_handle`].
//! 3. Per frame, the producer signals the fence at the value carried in
//!    [`Dx12SharedTexture::fence_value`](super::Dx12SharedTexture::fence_value).
//! 4. `producer_complete` queues [`ID3D12CommandQueue::Wait`] on the wgpu
//!    D3D12 queue, gating any subsequent submit on the producer reaching
//!    that value.
//!
//! D3D12 queue waits are queue-level and persist across submits, so the
//! `producer_complete` injection point is correct even when multiple frames
//! are imported before a single render submit.

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::{
    Foundation::{CloseHandle, GENERIC_ALL, HANDLE},
    Graphics::Direct3D12::{
        D3D12_FENCE_FLAG_SHARED, ID3D12CommandQueue, ID3D12Device, ID3D12Fence,
    },
};

use crate::native_frame::{
    HostWgpuContext, ImportedTexture, InteropBackend, InteropError, InteropSynchronizer,
    NativeFrame, SyncMechanism,
};

/// Synchronizer that uses a shared D3D12 fence to gate consumer submits on
/// producer rendering completion.
#[derive(Debug)]
pub struct Dx12FenceSynchronizer {
    fence: ID3D12Fence,
    queue: ID3D12CommandQueue,
    shared_handle: HANDLE,
    next_value: AtomicU64,
}

// SAFETY: D3D12 device-child interfaces are free-threaded COM objects, the
// shared HANDLE is only closed by this object's Drop, and `next_value` is the
// only mutable Rust state. Queue Wait/Signal operations are thread-safe under
// the D3D12 API contract.
unsafe impl Send for Dx12FenceSynchronizer {}
// SAFETY: see the Send rationale above. Shared access only invokes D3D12's
// free-threaded queue/fence methods and atomic value allocation.
unsafe impl Sync for Dx12FenceSynchronizer {}

impl Dx12FenceSynchronizer {
    /// Create a new shared fence on the host's wgpu D3D12 device and export
    /// an NT handle for the producer.
    pub fn new(host: &HostWgpuContext) -> Result<Self, InteropError> {
        if host.backend != InteropBackend::Dx12 {
            return Err(InteropError::BackendMismatch {
                expected: "Dx12",
                actual: "non-Dx12",
            });
        }

        let (fence, queue, shared_handle) = unsafe {
            let hal_device = host.device.as_hal::<wgpu::wgc::api::Dx12>().ok_or(
                InteropError::BackendMismatch {
                    expected: "Dx12",
                    actual: "non-Dx12",
                },
            )?;
            let d3d_device: ID3D12Device = hal_device.raw_device().clone();
            drop(hal_device);

            let hal_queue = host.queue.as_hal::<wgpu::wgc::api::Dx12>().ok_or(
                InteropError::BackendMismatch {
                    expected: "Dx12",
                    actual: "non-Dx12",
                },
            )?;
            let queue: ID3D12CommandQueue = hal_queue.as_raw().clone();
            drop(hal_queue);

            let fence: ID3D12Fence = d3d_device
                .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_SHARED)
                .map_err(|err| InteropError::Dx12(format!("CreateFence: {}", err)))?;

            let shared_handle = d3d_device
                .CreateSharedHandle(&fence, None, GENERIC_ALL.0, None)
                .map_err(|err| InteropError::Dx12(format!("CreateSharedHandle: {}", err)))?;

            (fence, queue, shared_handle)
        };

        Ok(Self {
            fence,
            queue,
            shared_handle,
            next_value: AtomicU64::new(0),
        })
    }

    /// The shared NT handle for the producer's `OpenSharedFence` call.
    /// The synchronizer closes this handle on drop.
    pub fn shared_handle(&self) -> HANDLE {
        self.shared_handle
    }

    /// Increment the fence counter and return the new value. Producers that
    /// don't carry per-frame fence values in their frame structs can call
    /// this to get the next signal value.
    pub fn advance(&self) -> u64 {
        self.next_value.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The current fence value (highest value returned by [`advance`], or
    /// `0` if `advance` has not been called).
    pub fn current_value(&self) -> u64 {
        self.next_value.load(Ordering::SeqCst)
    }
}

impl InteropSynchronizer for Dx12FenceSynchronizer {
    fn producer_complete(
        &self,
        frame: &NativeFrame,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        match mechanism {
            SyncMechanism::ExplicitFence => {
                // Prefer the per-frame fence value carried in the frame
                // struct; fall back to the synchronizer's internal counter
                // for callers that use `advance()` directly.
                let value = match frame {
                    NativeFrame::Dx12SharedTexture(dx_frame) if dx_frame.fence_value > 0 => {
                        dx_frame.fence_value
                    }
                    _ => self.next_value.load(Ordering::SeqCst),
                };
                if value == 0 {
                    return Ok(());
                }
                unsafe {
                    self.queue
                        .Wait(&self.fence, value)
                        .map_err(|err| InteropError::Dx12(format!("Wait: {}", err)))?;
                }
                Ok(())
            }
            SyncMechanism::None => Ok(()),
            other => Err(InteropError::UnsupportedSynchronization(other)),
        }
    }

    fn consumer_ready(
        &self,
        _texture: &ImportedTexture,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        match mechanism {
            SyncMechanism::None | SyncMechanism::ExplicitFence => Ok(()),
            other => Err(InteropError::UnsupportedSynchronization(other)),
        }
    }
}

impl Drop for Dx12FenceSynchronizer {
    fn drop(&mut self) {
        if !self.shared_handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.shared_handle);
            }
        }
    }
}

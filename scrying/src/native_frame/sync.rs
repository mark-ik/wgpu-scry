// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use crate::native_frame::{ImportedTexture, InteropError, NativeFrame};

/// Describes how the producer signals that a frame is ready and how the
/// consumer signals that it has finished reading.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SyncMechanism {
    None,
    /// An explicit Vulkan external semaphore is signalled by the
    /// producer. Reserved for the Linux WPE producer path.
    ExplicitExternalSemaphore,
    /// An explicit shared D3D12 fence is signalled by the producer.
    ExplicitFence,
    /// An explicit `MTLSharedEvent` is signalled by the producer
    /// (CPU-side via `signaledValue`) before each frame is emitted,
    /// and the consumer waits on the corresponding value before
    /// sampling. Reserved for the macOS WKWebView producer path.
    ///
    /// Currently a no-op on the macOS producer because
    /// ScreenCaptureKit doesn't expose its render queue for explicit
    /// fencing — implicit IOSurface coherence is the contract today.
    /// See [`crate::native_frame::MetalSharedEventSynchronizer`] and
    /// `design_docs/2026-05-07_platform_ceilings.md` for context.
    ExplicitMetalEvent,
}

/// Hook points called by [`WgpuTextureImporter`](crate::native_frame::WgpuTextureImporter)
/// around each frame import.
///
/// Implement this to add custom fence/semaphore logic. Two built-in
/// implementations are provided: [`NoopSynchronizer`] and
/// [`ImplicitOnlySynchronizer`]. Platform-specific synchronizers
/// (e.g. [`Dx12FenceSynchronizer`](crate::native_frame::Dx12FenceSynchronizer))
/// live alongside their producer paths.
pub trait InteropSynchronizer {
    /// Called after the frame is acquired from the producer, before import.
    /// Use this to wait on any producer-side signal.
    fn producer_complete(
        &self,
        frame: &NativeFrame,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError>;
    /// Called after the texture has been imported and is ready for the
    /// consumer. Use this to signal any consumer-side fence or semaphore.
    fn consumer_ready(
        &self,
        texture: &ImportedTexture,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError>;
}

impl<T: InteropSynchronizer + ?Sized> InteropSynchronizer for std::sync::Arc<T> {
    fn producer_complete(
        &self,
        frame: &NativeFrame,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        (**self).producer_complete(frame, mechanism)
    }

    fn consumer_ready(
        &self,
        texture: &ImportedTexture,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        (**self).consumer_ready(texture, mechanism)
    }
}

/// A synchronizer that does nothing. Suitable when the caller manages all
/// synchronization externally (e.g. via a shared queue or explicit barriers).
#[derive(Default)]
pub struct NoopSynchronizer;

impl InteropSynchronizer for NoopSynchronizer {
    fn producer_complete(
        &self,
        _frame: &NativeFrame,
        _mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        Ok(())
    }

    fn consumer_ready(
        &self,
        _texture: &ImportedTexture,
        _mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        Ok(())
    }
}

/// Default synchronizer: accepts only [`SyncMechanism::None`]. It is suitable
/// for implicit platform paths and one-shot diagnostics, not live WebView2
/// composition frames, which require [`crate::native_frame::Dx12FenceSynchronizer`].
#[derive(Default)]
pub struct ImplicitOnlySynchronizer;

impl InteropSynchronizer for ImplicitOnlySynchronizer {
    fn producer_complete(
        &self,
        _frame: &NativeFrame,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        Self::validate(mechanism)
    }

    fn consumer_ready(
        &self,
        _texture: &ImportedTexture,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        Self::validate(mechanism)
    }
}

impl ImplicitOnlySynchronizer {
    pub(crate) fn validate(mechanism: SyncMechanism) -> Result<(), InteropError> {
        match mechanism {
            SyncMechanism::None => Ok(()),
            other => Err(InteropError::UnsupportedSynchronization(other)),
        }
    }
}

/// Linux WPE placeholder synchronizer: accepts the explicit external
/// semaphore mechanism so DMABUF frames can flow to the Vulkan importer.
///
/// The actual `vkQueueSubmit` wait is part of the Linux Vulkan import work;
/// until that lands, importing a DMABUF frame still returns
/// [`InteropError::Unsupported`](crate::native_frame::InteropError::Unsupported).
#[derive(Default)]
pub struct ExplicitExternalSemaphoreSynchronizer;

impl InteropSynchronizer for ExplicitExternalSemaphoreSynchronizer {
    fn producer_complete(
        &self,
        _frame: &NativeFrame,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        Self::validate(mechanism)
    }

    fn consumer_ready(
        &self,
        _texture: &ImportedTexture,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        Self::validate(mechanism)
    }
}

impl ExplicitExternalSemaphoreSynchronizer {
    pub(crate) fn validate(mechanism: SyncMechanism) -> Result<(), InteropError> {
        match mechanism {
            SyncMechanism::None | SyncMechanism::ExplicitExternalSemaphore => Ok(()),
            other => Err(InteropError::UnsupportedSynchronization(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InteropSynchronizer, NoopSynchronizer};
    use std::sync::Arc;

    fn assert_interop_synchronizer<T: InteropSynchronizer>() {}

    #[test]
    fn arc_forwards_interop_synchronizer() {
        assert_interop_synchronizer::<Arc<NoopSynchronizer>>();
    }
}

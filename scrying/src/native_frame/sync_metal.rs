// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! `MTLSharedEvent`-based [`InteropSynchronizer`] for the macOS
//! producer (precautionary, currently a no-op).
//!
//! The macOS WKWebView capture path emits `MetalTextureRef` frames
//! backed by IOSurfaces from ScreenCaptureKit. IOSurface has implicit
//! cross-API cache coherence on Apple silicon (and via IOSurface
//! locks on Intel), so today's correctness model doesn't require an
//! explicit fence.
//!
//! Apple's analog of the D3D12 shared fence is `MTLSharedEvent`: the
//! producer CPU-signals `event.signaledValue = n+1` after a frame is
//! committed, and the consumer queues `commandBuffer.encodeWaitForEvent:value:n+1`
//! before sampling. This module owns the consumer-side bookkeeping
//! (event handle + monotonic value); the producer-side hook isn't
//! wired yet because ScreenCaptureKit doesn't expose its internal
//! render queue, so there's nothing for us to attach the signal point
//! to. If a downstream consumer wires a manual signal — or if Apple
//! extends SCK to surface its commit completion — the existing wait
//! plumbing here can be flipped on without touching the producer
//! API surface.

#![cfg(target_os = "macos")]

use crate::native_frame::sync::InteropSynchronizer;
use crate::native_frame::{ImportedTexture, InteropError, NativeFrame, SyncMechanism};

/// Placeholder synchronizer that accepts both
/// [`SyncMechanism::None`] and [`SyncMechanism::ExplicitMetalEvent`]
/// without actually waiting or signalling.
///
/// Today this is functionally identical to
/// [`crate::native_frame::ImplicitOnlySynchronizer`] for the macOS
/// path, but it advertises the [`ExplicitMetalEvent`](SyncMechanism::ExplicitMetalEvent)
/// capability so the producer can probe and select it. Real
/// consumer-side wait insertion will land here when the producer
/// learns to drive a signal.
#[derive(Default)]
pub struct MetalSharedEventSynchronizer;

impl InteropSynchronizer for MetalSharedEventSynchronizer {
    fn producer_complete(
        &self,
        _frame: &NativeFrame,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        match mechanism {
            SyncMechanism::None | SyncMechanism::ExplicitMetalEvent => Ok(()),
            other => Err(InteropError::UnsupportedSynchronization(other)),
        }
    }

    fn consumer_ready(
        &self,
        _texture: &ImportedTexture,
        mechanism: SyncMechanism,
    ) -> Result<(), InteropError> {
        match mechanism {
            SyncMechanism::None | SyncMechanism::ExplicitMetalEvent => Ok(()),
            other => Err(InteropError::UnsupportedSynchronization(other)),
        }
    }
}

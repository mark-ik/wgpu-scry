// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Scry's producer contract around Graft's direct Metal import.

#![cfg(target_os = "macos")]

use super::{HostWgpuContext, ImportedTexture, InteropBackend, InteropError, MetalTextureRef};

pub(super) fn import(
    frame: MetalTextureRef,
    host: &HostWgpuContext,
) -> Result<ImportedTexture, InteropError> {
    if host.backend != InteropBackend::Metal {
        return Err(InteropError::BackendMismatch {
            expected: "Metal",
            actual: "non-Metal",
        });
    }

    let size = frame.size;
    let format = frame.format;
    let generation = frame.generation;
    let producer_sync = frame.producer_sync;
    let metadata = grafting::FrameMetadata {
        size,
        format,
        generation,
        producer_sync: grafting::SyncMechanism::None,
    };
    let graft_frame = grafting::MetalTextureRef::new(metadata, frame.raw_metal_texture);
    let graft_host = grafting::HostWgpuContext::new(host.device.clone(), host.queue.clone());
    let texture = grafting::import_metal_texture_ref(graft_frame, &graft_host)
        .map_err(|error| InteropError::Metal(error.to_string()))?;

    Ok(ImportedTexture {
        texture,
        format,
        size,
        generation,
        consumer_sync: producer_sync,
    })
}

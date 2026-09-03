// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Scry's producer contract around Graft's direct Metal import.

#![cfg(target_os = "macos")]

use super::{HostWgpuContext, ImportedTexture, InteropBackend, InteropError, MetalTextureRef};

pub(super) fn import(
    frame: &MetalTextureRef,
    host: &HostWgpuContext,
) -> Result<ImportedTexture, InteropError> {
    if frame.raw_metal_texture.is_null() {
        return Err(InteropError::InvalidFrame("raw_metal_texture is null"));
    }
    if host.backend != InteropBackend::Metal {
        return Err(InteropError::BackendMismatch {
            expected: "Metal",
            actual: "non-Metal",
        });
    }

    let graft_host = grafting::HostWgpuContext::new(host.device.clone(), host.queue.clone());
    let graft_frame = grafting::MetalTextureRef {
        size: frame.size,
        format: frame.format,
        generation: frame.generation,
        producer_sync: grafting::SyncMechanism::None,
        raw_metal_texture: frame.raw_metal_texture,
    };
    let texture = grafting::import_metal_texture_ref(&graft_frame, &graft_host)
        .map_err(|error| InteropError::Metal(error.to_string()))?;

    Ok(ImportedTexture {
        texture,
        format: frame.format,
        size: frame.size,
        generation: frame.generation,
        consumer_sync: frame.producer_sync,
    })
}

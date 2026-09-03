// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use thiserror::Error;

use crate::native_frame::sync::SyncMechanism;

/// Why a particular interop path is not available.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UnsupportedReason {
    PlatformNotImplemented,
    HostBackendUnavailable,
    HostBackendMismatch,
    NativeImportNotYetImplemented,
}

/// Errors that can occur during frame import or synchronization.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InteropError {
    #[error("unsupported interop path: {0:?}")]
    Unsupported(UnsupportedReason),

    #[error("backend mismatch: expected {expected}, found {actual}")]
    BackendMismatch {
        expected: &'static str,
        actual: &'static str,
    },

    #[error("invalid frame: {0}")]
    InvalidFrame(&'static str),

    #[error("unsupported synchronization mechanism: {0:?}")]
    UnsupportedSynchronization(SyncMechanism),

    #[error("D3D12 interop failed: {0}")]
    Dx12(String),

    #[error("metal interop failed: {0}")]
    Metal(String),

    #[error("vulkan interop failed: {0}")]
    Vulkan(String),
}

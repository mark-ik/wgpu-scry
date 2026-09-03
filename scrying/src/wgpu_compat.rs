// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Metal-device compatibility across the supported wgpu majors.

/// Retain the selected HAL's Metal device as an objc2-metal object.
pub(crate) unsafe fn metal_device(
    device: &wgpu::Device,
) -> Option<objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLDevice>>> {
    let hal_device = unsafe { device.as_hal::<wgpu::wgc::api::Metal>() }?;
    #[cfg(all(
        feature = "wgpu-28",
        not(feature = "wgpu-29"),
        not(feature = "wgpu-30")
    ))]
    {
        use foreign_types_shared::ForeignType;
        unsafe {
            objc2::rc::Retained::retain(
                hal_device
                    .raw_device()
                    .as_ptr()
                    .cast::<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLDevice>>(),
            )
        }
    }
    #[cfg(any(feature = "wgpu-29", feature = "wgpu-30"))]
    {
        Some(hal_device.raw_device().clone())
    }
}

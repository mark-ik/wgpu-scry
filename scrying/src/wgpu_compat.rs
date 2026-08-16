//! Call-shape compatibility across the supported wgpu majors.

/// Wrap an externally initialized HAL texture in the selected wgpu version.
///
/// # Safety
///
/// `hal_texture` must come from `device`, match `desc`, and satisfy wgpu's
/// `Device::create_texture_from_hal` contract. On wgpu 30, `initial_state`
/// must match the resource's actual backend state.
pub(crate) unsafe fn create_texture_from_hal<A: wgpu_hal::Api>(
    device: &wgpu::Device,
    hal_texture: A::Texture,
    desc: &wgpu::TextureDescriptor<'_>,
    initial_state: wgpu::TextureUses,
) -> wgpu::Texture {
    #[cfg(feature = "wgpu-30")]
    unsafe {
        device.create_texture_from_hal::<A>(hal_texture, desc, initial_state)
    }
    #[cfg(not(feature = "wgpu-30"))]
    let _ = initial_state;
    #[cfg(not(feature = "wgpu-30"))]
    unsafe {
        device.create_texture_from_hal::<A>(hal_texture, desc)
    }
}

/// Retain the selected HAL's Metal device as an objc2-metal object.
#[cfg(target_os = "macos")]
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

//! `MTLTexture` → `wgpu::Texture` import path (Apple platforms).
//!
//! Lifted from wgpu-graft's `import_metal_texture_ref`. Adapts an
//! Objective-C `MTLTexture *` (from any source — typically the
//! producer's `IOSurface`-bridged Metal texture from ScreenCaptureKit)
//! into a `wgpu::Texture` via the wgpu Metal hal.
//!
//! The producer is expected to have created the texture against the
//! **host's** `MTLDevice` (acquired through
//! `wgpu::Device::as_hal::<Metal>().raw_device()`) so it's usable on
//! the host's wgpu queue without cross-device migration.

#![cfg(target_os = "macos")]

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLTexture;
#[cfg(any(feature = "wgpu-29", feature = "wgpu-30"))]
use objc2_metal::MTLTextureType;

#[cfg(all(
    feature = "wgpu-28",
    not(feature = "wgpu-29"),
    not(feature = "wgpu-30")
))]
use foreign_types_shared::ForeignType;

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

    let texture = unsafe {
        // Retain the caller's MTLTexture so wgpu can take ownership of
        // the reference we hand it without invalidating the caller's copy.
        let obj_ptr = frame.raw_metal_texture as *mut ProtocolObject<dyn MTLTexture>;
        let retained = Retained::retain(obj_ptr)
            .ok_or_else(|| InteropError::Metal("failed to retain Metal texture".into()))?;
        #[cfg(all(
            feature = "wgpu-28",
            not(feature = "wgpu-29"),
            not(feature = "wgpu-30")
        ))]
        let retained = metal::Texture::from_ptr(Retained::into_raw(retained).cast());

        let copy_size = wgpu::hal::CopyExtent {
            width: frame.size.width,
            height: frame.size.height,
            depth: 1,
        };
        #[cfg(feature = "wgpu-30")]
        let hal_texture = wgpu::hal::metal::Device::texture_from_raw(
            retained,
            frame.format,
            MTLTextureType::Type2D,
            1,
            1,
            copy_size,
            None,
        );
        #[cfg(all(feature = "wgpu-29", not(feature = "wgpu-30")))]
        let hal_texture = wgpu::hal::metal::Device::texture_from_raw(
            retained,
            frame.format,
            MTLTextureType::Type2D,
            1,
            1,
            copy_size,
        );
        #[cfg(all(
            feature = "wgpu-28",
            not(feature = "wgpu-29"),
            not(feature = "wgpu-30")
        ))]
        let hal_texture = wgpu::hal::metal::Device::texture_from_raw(
            retained,
            frame.format,
            metal::MTLTextureType::D2,
            1,
            1,
            copy_size,
        );

        crate::wgpu_compat::create_texture_from_hal::<wgpu::wgc::api::Metal>(
            &host.device,
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("scrying-metal-texture-ref-import"),
                size: wgpu::Extent3d {
                    width: frame.size.width,
                    height: frame.size.height,
                    depth_or_array_layers: 1,
                },
                format: frame.format,
                dimension: wgpu::TextureDimension::D2,
                mip_level_count: 1,
                sample_count: 1,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
            // Metal has no explicit image layout; this imported frame enters
            // wgpu for shader reads.
            wgpu::TextureUses::RESOURCE,
        )
    };

    Ok(ImportedTexture {
        texture,
        format: frame.format,
        size: frame.size,
        generation: frame.generation,
        consumer_sync: frame.producer_sync,
    })
}

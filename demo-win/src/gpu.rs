pub(crate) fn validate_imported_pixels(
    imported: &scrying::ImportedTexture,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    expected_rgb: (u8, u8, u8),
) -> Result<(), Box<dyn std::error::Error>> {
    if imported.format != wgpu::TextureFormat::Bgra8Unorm {
        return Err(format!(
            "WebView readback: expected Bgra8Unorm imported texture, got {:?}",
            imported.format
        )
        .into());
    }

    let width = imported.size.width;
    let height = imported.size.height;
    let bytes_per_pixel = 4u32;
    let unpadded_bytes_per_row = width * bytes_per_pixel;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
    let buffer_size = (padded_bytes_per_row as u64) * (height as u64);

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("webview-readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("webview-readback-encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &imported.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device.poll(wgpu::PollType::wait_indefinitely())?;
    rx.recv()
        .map_err(|error| format!("readback channel closed: {error}"))?
        .map_err(|error| format!("buffer map failed: {error}"))?;
    let data = slice.get_mapped_range().expect("map range");

    let row_stride = padded_bytes_per_row as usize;
    let sample = |x: u32, y: u32| -> [u8; 4] {
        let offset = (y as usize) * row_stride + (x as usize) * 4;
        [
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]
    };

    let inset = 4u32
        .min(width.saturating_sub(1))
        .min(height.saturating_sub(1));
    let tl = sample(inset, inset);
    let tr = sample(width.saturating_sub(1 + inset), inset);
    let bl = sample(inset, height.saturating_sub(1 + inset));
    let br = sample(
        width.saturating_sub(1 + inset),
        height.saturating_sub(1 + inset),
    );
    let center = sample(width / 2, height / 2);

    drop(data);
    buffer.unmap();

    let (er, eg, eb) = expected_rgb;
    println!(
        "WebView readback: expected background BGRA=({},{},{},255) from CSS rgb({},{},{})",
        eb, eg, er, er, eg, eb
    );
    println!(
        "WebView readback: tl=BGRA{:?} tr=BGRA{:?} bl=BGRA{:?} br=BGRA{:?} center=BGRA{:?}",
        tl, tr, bl, br, center
    );

    let tolerance: i32 = 6;
    let close_to_background = |bgra: [u8; 4]| -> bool {
        let [b, g, r, _a] = bgra;
        (b as i32 - eb as i32).abs() <= tolerance
            && (g as i32 - eg as i32).abs() <= tolerance
            && (r as i32 - er as i32).abs() <= tolerance
    };
    let corners_match = close_to_background(tl)
        && close_to_background(tr)
        && close_to_background(bl)
        && close_to_background(br);
    println!(
        "WebView readback: corner pixels match background within ±{tolerance}: {corners_match}"
    );

    if !corners_match {
        return Err(
            "WebView readback: corner pixels do not match the HTML background; \
             capture content is likely wrong (zeros, swapped channels, or empty)."
                .into(),
        );
    }

    let nonzero_alpha = tl[3] > 0 || tr[3] > 0 || bl[3] > 0 || br[3] > 0 || center[3] > 0;
    if !nonzero_alpha {
        return Err(
            "WebView readback: every sampled alpha is zero; capture is likely uninitialized."
                .into(),
        );
    }

    Ok(())
}

pub(crate) async fn create_host_device() -> Result<
    (wgpu::Instance, wgpu::Device, wgpu::Queue, wgpu::AdapterInfo),
    Box<dyn std::error::Error>,
> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: preferred_backends(),
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    });

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            // wgpu 30 limit bucketing, off to keep the adapter's real limits.
            apply_limit_buckets: false,
        })
        .await
        .map_err(|error| format!("adapter request failed: {error}"))?;

    let adapter_info = adapter.get_info();
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("demo-win"),
            ..Default::default()
        })
        .await
        .map_err(|error| format!("device request failed: {error}"))?;

    Ok((instance, device, queue, adapter_info))
}

fn preferred_backends() -> wgpu::Backends {
    if cfg!(target_os = "windows") {
        wgpu::Backends::DX12
    } else {
        wgpu::Backends::PRIMARY
    }
}

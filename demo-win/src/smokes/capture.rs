// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::super::*;

pub(crate) fn validate_platform_capture(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    host: &HostWgpuContext,
    fence_synchronizer: &Arc<scrying::Dx12FenceSynchronizer>,
) -> Result<(), Box<dyn std::error::Error>> {
    let captured = producer.acquire_full_frame()?;
    let content_size = captured.content_size;
    let imported = import_and_consume(host, captured, fence_synchronizer)?;
    let metrics = producer.capture_metrics();
    let color_pipeline = producer.capture_color_pipeline();
    let texture_format = producer.capture_texture_format();
    println!(
        "demo-win: capture-test: captured {}x{}, imported {:?} {}x{} generation {}, color={:?}, texture_format={:?}, received={}, consumed={}, stale_dropped={}",
        content_size.width,
        content_size.height,
        imported.format,
        imported.size.width,
        imported.size.height,
        imported.generation,
        color_pipeline,
        texture_format,
        metrics.samples_received,
        metrics.samples_consumed,
        metrics.stale_frames_dropped,
    );
    println!("demo-win: capture-test: PASS - WebView2 WGC frame acquired and imported");
    Ok(())
}

pub(crate) fn validate_platform_scale_resize(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    host: &HostWgpuContext,
    fence_synchronizer: &Arc<scrying::Dx12FenceSynchronizer>,
) -> Result<(), Box<dyn std::error::Error>> {
    let samples = [
        winit::dpi::PhysicalSize::new(315, 195),
        winit::dpi::PhysicalSize::new(SMOKE_PROBE_WIDTH as u32, SMOKE_PROBE_HEIGHT as u32),
    ];
    for target in samples {
        producer.resize(target)?;
        let captured = producer.acquire_full_frame()?;
        let content_size = captured.content_size;
        let imported = import_and_consume(host, captured, fence_synchronizer)?;
        if imported.size != target {
            return Err(format!(
                "scale-test imported {}x{} after resize to {}x{}",
                imported.size.width, imported.size.height, target.width, target.height
            )
            .into());
        }
        println!(
            "demo-win: scale-test: captured {}x{}, imported {:?} {}x{} generation {}",
            content_size.width,
            content_size.height,
            imported.format,
            imported.size.width,
            imported.size.height,
            imported.generation,
        );
    }
    let metrics = producer.capture_metrics();
    println!(
        "demo-win: scale-test: PASS - simulated scale resize path, received={}, consumed={}, stale_dropped={}",
        metrics.samples_received, metrics.samples_consumed, metrics.stale_frames_dropped,
    );
    Ok(())
}

fn import_and_consume(
    host: &HostWgpuContext,
    captured: scrying::webview2_composition_producer::WebView2CompositionFrame,
    fence_synchronizer: &Arc<scrying::Dx12FenceSynchronizer>,
) -> Result<scrying::ImportedTexture, Box<dyn std::error::Error>> {
    let WebSurfaceFrame::Native(native_frame) = captured.frame else {
        return Err("WebView2 composition producer did not emit a native frame".into());
    };
    let importer = WgpuTextureImporter::with_synchronizer(
        host.clone(),
        Box::new(Arc::clone(fence_synchronizer)),
    );
    Ok(importer.import_frame(native_frame, &ImportOptions::default())?)
}

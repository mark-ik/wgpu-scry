// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;

use scrying::{
    Dx12FenceSynchronizer, HostWgpuContext, ImportOptions, TextureImporter, WebSurfaceFrame,
    WgpuTextureImporter,
};
use winit::window::Window;

use super::probe::CapturedComposition;
use super::{capture_offset_for_window, capture_size_for_window};

const WEBVIEW_BLIT_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    let uv = vec2<f32>(f32((vid << 1u) & 2u), f32(vid & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(uv.x, 1.0 - uv.y);
    return out;
}

@group(0) @binding(0) var captured: texture_2d<f32>;
@group(0) @binding(1) var captured_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(captured, captured_sampler, in.uv);
}
"#;

pub(crate) struct WebViewRenderer {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    bind_group: wgpu::BindGroup,
    importer: WgpuTextureImporter,
    pub(crate) captured: CapturedComposition,
    /// Tiny destination buffer for a 1x1 `copy_texture_to_buffer` issued
    /// each render. The copy itself is throwaway - the point is to force
    /// wgpu to emit a `SHADER_RESOURCE -> COPY_SRC -> SHADER_RESOURCE`
    /// transition barrier on the imported texture, which on D3D12 flushes
    /// shader caches that would otherwise hold the producer's first
    /// captured frame indefinitely. Without this the wgpu render goes
    /// stale even while the producer continuously `CopyResource`s into
    /// the same shared D3D11 texture.
    cache_flush_buffer: wgpu::Buffer,
    frames_imported: u64,
    frames_polled: u64,
    frames_acquired: u64,
    frames_resource_swapped: u64,
    consecutive_empty_polls: u32,
    capture_restarts: u64,
    last_metric_log: std::time::Instant,
    /// Pending producer resize, deferred until the user stops dragging.
    /// Stores the most recent target window size and the time of the last
    /// `Resized` event. We apply the producer rebuild only after a quiet
    /// period - `producer.resize` + `start_capture` together cost ~300ms,
    /// which would wedge the Win32 modal resize loop if run synchronously
    /// per-event during a drag.
    pending_producer_resize: Option<(winit::dpi::PhysicalSize<u32>, std::time::Instant)>,
    last_committed_capture_size: winit::dpi::PhysicalSize<u32>,
    /// Last (width, height) actually passed to `surface.configure`.
    /// Tracked separately from `surface_config.width/height` so we can
    /// notice when the resize handler updated the desired size and lazily
    /// reconfigure on the next render.
    configured_surface_size: (u32, u32),
}

impl WebViewRenderer {
    pub(crate) fn new(
        instance: wgpu::Instance,
        window: Arc<Window>,
        host: HostWgpuContext,
        captured: CapturedComposition,
        fence_synchronizer: Arc<Dx12FenceSynchronizer>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let device = host.device.clone();
        let queue = host.queue.clone();
        let importer =
            WgpuTextureImporter::with_synchronizer(host.clone(), Box::new(fence_synchronizer));
        let surface = instance.create_surface(window.clone())?;
        let size = window.inner_size();

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            // wgpu 30 limit bucketing, off to keep the adapter's real limits.
            apply_limit_buckets: false,
        }))
        .map_err(|error| format!("renderer adapter request failed: {error}"))?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Bgra8Unorm))
            .unwrap_or_else(|| caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            // wgpu 30 made surface color space explicit; Auto keeps pre-30 behavior.
            color_space: wgpu::SurfaceColorSpace::Auto,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("webview-blit-shader"),
            source: wgpu::ShaderSource::Wgsl(WEBVIEW_BLIT_SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("webview-blit-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("webview-blit-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("webview-blit-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("webview-blit-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let placeholder_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("webview-blit-placeholder"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let bind_group = match captured.imported.as_ref() {
            Some(imported) => build_bind_group(&device, &bind_group_layout, &sampler, imported),
            None => build_bind_group_for_texture(
                &device,
                &bind_group_layout,
                &sampler,
                &placeholder_texture,
            ),
        };

        let cache_flush_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("webview-cache-flush-buffer"),
            size: wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64,
            usage: wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            window,
            device,
            queue,
            surface,
            surface_config,
            pipeline,
            bind_group_layout,
            sampler,
            bind_group,
            importer,
            captured,
            cache_flush_buffer,
            frames_imported: 1,
            frames_polled: 0,
            frames_acquired: 0,
            frames_resource_swapped: 0,
            consecutive_empty_polls: 0,
            capture_restarts: 0,
            last_metric_log: std::time::Instant::now(),
            pending_producer_resize: None,
            last_committed_capture_size: capture_size_for_window(size),
            configured_surface_size: (size.width.max(1), size.height.max(1)),
        })
    }

    fn refresh_captured_texture(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        self.frames_polled = self.frames_polled.saturating_add(1);

        let new_frame = match self.captured.producer.try_acquire_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                self.consecutive_empty_polls = self.consecutive_empty_polls.saturating_add(1);
                if self.consecutive_empty_polls >= 120 {
                    self.captured.producer.force_restart_capture();
                    self.capture_restarts = self.capture_restarts.saturating_add(1);
                    self.consecutive_empty_polls = 0;
                    eprintln!(
                        "demo-win: capture stalled after >=1s of empty polls; restarting capture session (restart #{})",
                        self.capture_restarts
                    );
                }
                self.maybe_log_metrics();
                return Ok(false);
            }
            Err(error) => {
                eprintln!("demo-win: try_acquire_frame failed: {error}");
                return Ok(false);
            }
        };

        self.frames_acquired = self.frames_acquired.saturating_add(1);
        self.consecutive_empty_polls = 0;

        if new_frame.resource_is_new {
            let WebSurfaceFrame::Native(native_frame) = new_frame.frame else {
                return Err("WebView2 producer did not emit a native frame".into());
            };
            let imported = self
                .importer
                .import_frame(native_frame, &ImportOptions::default())?;

            self.bind_group = build_bind_group(
                &self.device,
                &self.bind_group_layout,
                &self.sampler,
                &imported,
            );
            self.captured.imported = Some(imported);
            self.frames_imported = self.frames_imported.saturating_add(1);
            self.frames_resource_swapped = self.frames_resource_swapped.saturating_add(1);
        }

        self.maybe_log_metrics();
        Ok(true)
    }

    fn maybe_log_metrics(&mut self) {
        let elapsed = self.last_metric_log.elapsed();
        if elapsed < std::time::Duration::from_secs(2) {
            return;
        }
        let secs = elapsed.as_secs_f64().max(0.001);
        let producer_metrics = self.captured.producer.capture_metrics();
        println!(
            "renderer metrics ({:.1}s): polled={}, acquired={} ({:.1}/s), resource_swaps={}, total_imports={}, capture_restarts={}, producer_received={}, producer_consumed={}, producer_stale_dropped={}",
            secs,
            self.frames_polled,
            self.frames_acquired,
            (self.frames_acquired as f64) / secs,
            self.frames_resource_swapped,
            self.frames_imported,
            self.capture_restarts,
            producer_metrics.samples_received,
            producer_metrics.samples_consumed,
            producer_metrics.stale_frames_dropped,
        );
        self.frames_polled = 0;
        self.frames_acquired = 0;
        self.frames_resource_swapped = 0;
        self.last_metric_log = std::time::Instant::now();
    }

    pub(crate) fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.surface_config.width = new_size.width;
        self.surface_config.height = new_size.height;
        self.pending_producer_resize = Some((new_size, std::time::Instant::now()));
    }

    fn apply_pending_resize(&mut self) -> bool {
        const SETTLE_MS: u128 = 120;
        let (target_size, last_event) = match self.pending_producer_resize {
            Some(p) => p,
            None => return false,
        };
        let elapsed = last_event.elapsed().as_millis();
        if elapsed < SETTLE_MS {
            return false;
        }
        let capture_size = capture_size_for_window(target_size);
        if capture_size == self.last_committed_capture_size {
            self.pending_producer_resize = None;
            return false;
        }
        let (offset_x, offset_y) = capture_offset_for_window(target_size);
        println!(
            "resize (settled): window={}x{} -> capture={}x{} offset=({}, {})",
            target_size.width,
            target_size.height,
            capture_size.width,
            capture_size.height,
            offset_x,
            offset_y
        );
        let _ = elapsed;
        if let Err(error) = self.captured.producer.set_offset(offset_x, offset_y) {
            eprintln!("demo-win: producer.set_offset({offset_x}, {offset_y}) failed: {error}");
        }
        if let Err(error) = self.captured.producer.resize(capture_size) {
            eprintln!(
                "demo-win: producer.resize({}x{}) failed: {error}",
                capture_size.width, capture_size.height
            );
        }
        self.last_committed_capture_size = capture_size;
        self.pending_producer_resize = None;
        true
    }

    pub(crate) fn render(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let desired = (self.surface_config.width, self.surface_config.height);
        if desired != self.configured_surface_size {
            self.surface.configure(&self.device, &self.surface_config);
            self.configured_surface_size = desired;
        }
        self.apply_pending_resize();
        let _ = self.refresh_captured_texture()?;
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return Ok(()),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("webview-blit-encoder"),
            });

        if let Some(imported) = self.captured.imported.as_ref() {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &imported.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &self.cache_flush_buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
                        rows_per_image: Some(1),
                    },
                },
                wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("webview-blit-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.07,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.window.pre_present_notify();
        // wgpu 30 moved presentation from SurfaceTexture to Queue.
        self.queue.present(frame);
        Ok(())
    }
}

fn build_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    imported: &scrying::ImportedTexture,
) -> wgpu::BindGroup {
    build_bind_group_for_texture(device, layout, sampler, &imported.texture)
}

fn build_bind_group_for_texture(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    texture: &wgpu::Texture,
) -> wgpu::BindGroup {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("webview-blit-bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

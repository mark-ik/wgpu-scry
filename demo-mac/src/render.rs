//! Wgpu render loop for the demo. Sets up a Metal-backed wgpu surface
//! against the winit window and draws the WKWebView's
//! ScreenCaptureKit-imported `MTLTexture` (when capture is live) as a
//! full-screen blit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use scrying::WkWebViewProducer;
use scrying::{
    HostWgpuContext, ImportOptions, ImportedTexture, NativeFrame, TextureImporter, WebSurfaceFrame,
    WgpuTextureImporter,
};
use winit::window::Window;

const SHADER_SRC: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vid: u32) -> VsOut {
    // Full-screen triangle covers the whole NDC viewport. The
    // fragment shader discards the left half so the WKWebView
    // subview shows through unobstructed there; the right half
    // displays the imported texture at native aspect (UV maps the
    // texture's full 0..1 range onto NDC x in [0..1]).
    let x = f32((vid & 1u) << 2u) - 1.0;
    let y = f32((vid & 2u) << 1u) - 1.0;
    var out: VsOut;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    // UV.x = NDC x: negative on the left half (discarded), 0..1
    // across the right half. UV.y is Y-flipped because IOSurface /
    // SCK textures use a top-left origin while wgpu NDC is
    // bottom-left.
    out.uv = vec2<f32>(x, 1.0 - (y + 1.0) * 0.5);
    return out;
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sam: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    if (in.uv.x < 0.0) {
        discard;
    }
    return textureSample(src_tex, src_sam, in.uv);
}
"#;

pub struct WgpuRender {
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    importer: WgpuTextureImporter,
    pub host_context: HostWgpuContext,
    /// Counter incremented per imported frame the renderer actually
    /// drew. Useful for the demo to log progress.
    pub frames_drawn: u64,
    /// When set to `Some(n)`, every `n`-th imported frame is read
    /// back from GPU memory to an `image::RgbaImage` and saved as
    /// `demo-mac-frame-NNNN.png`. Verifies the IOSurface → MTLTexture
    /// → wgpu::Texture chain is pixel-faithful end-to-end.
    pub dump_every: Option<u64>,
    /// Counter for dump file names. Independent of `frames_drawn`
    /// so the dump indices remain monotonic even when `dump_every`
    /// is changed live.
    pub dumps_written: u64,
    /// Most recent imported texture. SCK delivers samples on its
    /// own cadence; on frames where `try_acquire_frame` has no new
    /// sample ready we re-render the previous one rather than
    /// clearing — otherwise the right half blanks on every miss.
    last_imported: Option<ImportedTexture>,
    /// Latency-probe state. Once per second the demo logs SCK
    /// delivery rate (push cadence) vs consume rate (frames the
    /// consumer actually received via `try_acquire_frame`) vs
    /// render rate (wgpu redraws). The deltas come from
    /// [`WkWebViewProducer::capture_metrics`] minus the previous
    /// snapshot, so the rates reflect the *last second*, not the
    /// stream-lifetime average.
    last_metrics_at: Option<Instant>,
    last_samples_received: u64,
    last_samples_consumed: u64,
    last_frames_drawn_at_metrics: u64,
}

impl WgpuRender {
    pub async fn new(window: Arc<Window>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(instance_desc);

        let size = window.inner_size();
        let surface: wgpu::Surface<'static> = instance.create_surface(window.clone())?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                // wgpu 30 limit bucketing, off to keep the adapter's real limits.
                apply_limit_buckets: false,
            })
            .await?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("demo-mac-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::default(),
            })
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: caps.present_modes[0],
            // wgpu 30 made surface color space explicit; Auto keeps pre-30 behavior.
            color_space: wgpu::SurfaceColorSpace::Auto,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("demo-mac-bgl"),
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
            label: Some("demo-mac-pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("demo-mac-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("demo-mac-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("demo-mac-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let host_context = HostWgpuContext::new(device.clone(), queue.clone());
        let importer = WgpuTextureImporter::new(host_context.clone());

        Ok(Self {
            surface,
            surface_config,
            device,
            queue,
            pipeline,
            bind_group_layout,
            sampler,
            importer,
            host_context,
            frames_drawn: 0,
            dump_every: None,
            dumps_written: 0,
            last_imported: None,
            last_metrics_at: None,
            last_samples_received: 0,
            last_samples_consumed: 0,
            last_frames_drawn_at_metrics: 0,
        })
    }

    /// Read back the imported wgpu texture's pixels to CPU memory and
    /// save as `demo-mac-frame-NNNN.png`. Verifies that the
    /// IOSurface → `MTLTexture` → `wgpu::Texture` chain produces the
    /// pixels we expect.
    ///
    /// Allocates a fresh staging buffer per call (one PNG per
    /// `dump_every` frames is rare enough that pooling doesn't pay
    /// for itself).
    fn dump_imported_texture(
        &mut self,
        texture: &wgpu::Texture,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let width = texture.width();
        let height = texture.height();
        // wgpu requires `bytes_per_row` to be a multiple of
        // `COPY_BYTES_PER_ROW_ALIGNMENT` (256). For BGRA8, the natural
        // row stride is `width * 4`; round up to the next 256-byte
        // boundary and trim the padding when we write the PNG.
        let bytes_per_pixel = 4u32;
        let unpadded_bytes_per_row = width * bytes_per_pixel;
        let padded_bytes_per_row = unpadded_bytes_per_row
            .div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer_size = (padded_bytes_per_row as u64) * (height as u64);
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("demo-mac-dump-staging"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("demo-mac-dump-encoder"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
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
        self.queue.submit(std::iter::once(encoder.finish()));

        let slice = staging.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        receiver.recv()??;

        let view = slice.get_mapped_range().expect("map range");
        // Strip the per-row padding into a packed BGRA8 buffer, then
        // swap to RGBA8 for the `image` crate's PNG encoder.
        let mut rgba = Vec::with_capacity((unpadded_bytes_per_row * height) as usize);
        for row in 0..height as usize {
            let start = row * padded_bytes_per_row as usize;
            let end = start + unpadded_bytes_per_row as usize;
            let chunk = &view[start..end];
            for px in chunk.chunks_exact(4) {
                // texture format is BGRA8Unorm.
                rgba.push(px[2]);
                rgba.push(px[1]);
                rgba.push(px[0]);
                rgba.push(px[3]);
            }
        }
        drop(view);
        staging.unmap();

        let img = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or("RgbaImage::from_raw shape mismatch")?;
        let path = format!("demo-mac-frame-{:04}.png", self.dumps_written);
        img.save(&path)?;
        self.dumps_written += 1;
        println!("demo-mac: wrote {path} ({}x{})", width, height);
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.surface_config.width = width.max(1);
        self.surface_config.height = height.max(1);
        self.surface.configure(&self.device, &self.surface_config);
        // Drop the cached pre-resize texture: its pixel dimensions
        // are stale and stretching them over the new right-half
        // shows a briefly distorted page until SCK delivers a new
        // sample at the new window size.
        self.last_imported = None;
    }

    pub fn render(
        &mut self,
        producer: &mut WkWebViewProducer,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame_result = producer.try_acquire_frame();
        let new_imported = match frame_result {
            Ok(Some(WebSurfaceFrame::Native(NativeFrame::MetalTextureRef(frame)))) => {
                let native = NativeFrame::MetalTextureRef(frame);
                match self
                    .importer
                    .import_frame(&native, &ImportOptions::default())
                {
                    Ok(imported) => Some(imported),
                    Err(e) => {
                        eprintln!("demo-mac: import_frame failed: {e}");
                        None
                    }
                }
            }
            Ok(_) | Err(_) => None,
        };

        // Optional readback of the *fresh* import only — re-dumping a
        // cached frame would just produce duplicate PNGs.
        if let Some(new) = new_imported.as_ref()
            && let Some(every) = self.dump_every
            && every > 0
            && self.frames_drawn.is_multiple_of(every)
            && let Err(e) = self.dump_imported_texture(&new.texture)
        {
            eprintln!("demo-mac: dump_imported_texture failed: {e}");
        }
        if new_imported.is_some() {
            self.last_imported = new_imported;
        }

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(());
            }
            other => {
                return Err(format!("surface acquire failed: {other:?}").into());
            }
        };
        let surface_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("demo-mac-encoder"),
            });

        if let Some(imported) = self.last_imported.as_ref() {
            let imported_view = imported
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("demo-mac-bg"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&imported_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("demo-mac-blit"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &surface_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
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
                pass.set_bind_group(0, &bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            self.frames_drawn = self.frames_drawn.wrapping_add(1);
        } else {
            // No imported frame yet: clear with a recognizable color
            // so we can see the surface is alive even before capture.
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("demo-mac-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.06,
                            b: 0.10,
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
            drop(pass);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        // wgpu 30 moved presentation from SurfaceTexture to Queue.
        self.queue.present(surface_texture);

        self.maybe_log_metrics(producer);

        Ok(())
    }

    fn maybe_log_metrics(&mut self, producer: &WkWebViewProducer) {
        let now = Instant::now();
        let last = match self.last_metrics_at {
            None => {
                self.last_metrics_at = Some(now);
                let m = producer.capture_metrics();
                self.last_samples_received = m.samples_received;
                self.last_samples_consumed = m.samples_consumed;
                self.last_frames_drawn_at_metrics = self.frames_drawn;
                return;
            }
            Some(t) => t,
        };
        let elapsed = now.duration_since(last);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let m = producer.capture_metrics();
        let dt = elapsed.as_secs_f64();
        let recv_delta = m
            .samples_received
            .saturating_sub(self.last_samples_received);
        let cons_delta = m
            .samples_consumed
            .saturating_sub(self.last_samples_consumed);
        let render_delta = self
            .frames_drawn
            .saturating_sub(self.last_frames_drawn_at_metrics);
        let dropped = recv_delta.saturating_sub(cons_delta);
        println!(
            "demo-mac: capture cadence — sck push {:.1}/s, demo consume {:.1}/s, wgpu render {:.1}/s, dropped {} this window (totals: recv {}, consumed {})",
            recv_delta as f64 / dt,
            cons_delta as f64 / dt,
            render_delta as f64 / dt,
            dropped,
            m.samples_received,
            m.samples_consumed,
        );
        self.last_metrics_at = Some(now);
        self.last_samples_received = m.samples_received;
        self.last_samples_consumed = m.samples_consumed;
        self.last_frames_drawn_at_metrics = self.frames_drawn;
    }
}

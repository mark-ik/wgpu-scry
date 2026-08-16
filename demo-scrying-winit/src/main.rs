//! Cross-platform winit + wgpu smoke host for scrying backend selection.

use std::sync::Arc;

use scrying::{HostWgpuContext, WebSurfaceCapabilities};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::Window;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut app = App {
        probe_only: std::env::args().any(|arg| arg == "--probe-only"),
        state: None,
    };
    Ok(event_loop.run_app(&mut app)?)
}

#[derive(Default)]
struct App {
    probe_only: bool,
    state: Option<AppState>,
}

struct AppState {
    window: Arc<Window>,
    _device: wgpu::Device,
    _queue: wgpu::Queue,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        match AppState::new(event_loop) {
            Ok(state) => {
                self.state = Some(state);
                if self.probe_only {
                    event_loop.exit();
                }
            }
            Err(error) => {
                eprintln!("demo-scrying-winit: initialization failed: {error}");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }
}

impl AppState {
    fn new(event_loop: &ActiveEventLoop) -> Result<Self, Box<dyn std::error::Error>> {
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title("demo-scrying-winit")
                    .with_inner_size(winit::dpi::PhysicalSize::new(900, 600)),
            )?,
        );

        let (_instance, device, queue, adapter_info) = pollster::block_on(create_host_device())?;
        let host = HostWgpuContext::new(device.clone(), queue.clone());
        let capabilities = WebSurfaceCapabilities::probe(Some(&host));

        println!("wgpu adapter: {}", adapter_info.name);
        println!("wgpu backend: {:?}", host.backend);
        println!("system webview backend: {:?}", capabilities.backend);
        println!("preferred surface mode: {:?}", capabilities.preferred_mode);
        println!(
            "imported texture support: {:?}",
            capabilities.imported_texture
        );
        println!(
            "native overlay support: {:?}",
            capabilities.native_child_overlay
        );
        println!("CPU snapshot support: {:?}", capabilities.cpu_snapshot);
        println!(
            "supported native frames: {:?}",
            capabilities.supported_frames
        );
        println!(
            "platform producer type: {}",
            std::any::type_name::<scrying::PlatformWebSurfaceProducer>()
        );
        println!(
            "platform config type: {}",
            std::any::type_name::<scrying::PlatformWebSurfaceConfig>()
        );
        println!("reason: {}", capabilities.reason);

        Ok(Self {
            window,
            _device: device,
            _queue: queue,
        })
    }
}

async fn create_host_device() -> Result<
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
            // This local smoke host does not expose wgpu to untrusted content.
            apply_limit_buckets: false,
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .map_err(|error| format!("adapter request failed: {error}"))?;

    let adapter_info = adapter.get_info();
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("demo-scrying-winit"),
            ..Default::default()
        })
        .await
        .map_err(|error| format!("device request failed: {error}"))?;

    Ok((instance, device, queue, adapter_info))
}

fn preferred_backends() -> wgpu::Backends {
    #[cfg(target_os = "windows")]
    {
        wgpu::Backends::DX12
    }
    #[cfg(not(target_os = "windows"))]
    {
        wgpu::Backends::PRIMARY
    }
}

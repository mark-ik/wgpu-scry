use super::*;

impl WebView2CompositionProducer {
    pub fn capture_snapshot_png(&self) -> Result<Vec<u8>, WebSurfaceError> {
        let stream: IStream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }
            .map_err(platform("CreateStreamOnHGlobal"))?;
        let (tx, rx) = mpsc::channel::<windows::core::Result<()>>();
        let handler = webview2_com::CapturePreviewCompletedHandler::create(Box::new(
            move |result: windows::core::Result<()>| {
                let _ = tx.send(result);
                Ok(())
            },
        ));
        unsafe {
            self.webview
                .CapturePreview(
                    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                    &stream,
                    &handler,
                )
                .map_err(platform("CapturePreview"))?;
        }

        loop {
            pump_messages_for(Duration::from_millis(16));
            match rx.try_recv() {
                Ok(result) => {
                    result.map_err(platform("CapturePreview completion"))?;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(WebSurfaceError::Platform(
                        "CapturePreview completion channel closed unexpectedly".into(),
                    ));
                }
            }
        }

        unsafe {
            let hglobal =
                GetHGlobalFromStream(&stream).map_err(platform("GetHGlobalFromStream"))?;
            let size = GlobalSize(hglobal);
            if size == 0 {
                return Ok(Vec::new());
            }
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                return Err(WebSurfaceError::Platform("GlobalLock returned null".into()));
            }
            let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
            let _ = GlobalUnlock(hglobal);
            Ok(bytes)
        }
    }

    pub fn force_restart_capture(&mut self) {
        if let Some(state) = self.capture_state.take() {
            let _ = state.pool.RemoveFrameArrived(state.frame_arrived_token);
            let _ = state.session.Close();
            let _ = state.pool.Close();
        }
    }

    /// Keep a visual-bound WGC session through a composition-host move.
    ///
    /// `GraphicsCaptureItem::CreateFromVisual` captures `webview_visual`, not
    /// its `DesktopWindowTarget` or parent HWND. Reparenting moves the same
    /// visual tree to another target, so closing a pending session here loses
    /// the first post-move callback before the externally driven redraw loop
    /// can observe it. Frames already queued before the target handoff are
    /// deliberately skipped; the next arrival is the post-move observation.
    pub(super) fn preserve_capture_after_visual_reparent(&mut self) {
        if let Some(state) = self.capture_state.as_mut() {
            state.mark_arrivals_observed();
        }
    }

    pub fn invalidate_persistent_dest(&mut self) {
        self.persistent_dest = None;
    }

    pub fn set_offset(&self, x: f32, y: f32) -> Result<(), WebSurfaceError> {
        self.pane_container
            .SetOffset(Vector3 { X: x, Y: y, Z: 0.0 })
            .map_err(platform("pane_container.SetOffset"))
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), WebSurfaceError> {
        if size.width == 0 || size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WebView2 producer resize must be non-zero, got {}x{}",
                size.width, size.height
            )));
        }
        if size == self.size {
            return Ok(());
        }
        eprintln!(
            "[producer] resize: {}x{} -> {}x{}",
            self.size.width, self.size.height, size.width, size.height
        );
        let visual_size = Vector2 {
            X: size.width as f32,
            Y: size.height as f32,
        };
        self.pane_container
            .SetSize(visual_size)
            .map_err(platform("pane_container.SetSize"))?;
        self.webview_visual
            .SetSize(visual_size)
            .map_err(platform("webview_visual.SetSize"))?;
        unsafe {
            self.controller
                .SetBounds(RECT {
                    left: 0,
                    top: 0,
                    right: size.width as i32,
                    bottom: size.height as i32,
                })
                .map_err(platform("controller.SetBounds"))?;
        }
        self.force_restart_capture();
        self.persistent_dest = None;
        self.size = size;
        Ok(())
    }

    pub fn acquire_full_frame(&mut self) -> Result<WebView2CompositionFrame, WebSurfaceError> {
        if self.capture_state.is_none() {
            self.start_capture()?;
        }
        self.acquire_frame_with_timeout(Duration::from_secs(2))
    }

    pub fn capture_metrics(&self) -> CaptureMetrics {
        CaptureMetrics {
            samples_received: self.capture_samples_received.load(Ordering::Relaxed),
            samples_consumed: self.capture_samples_consumed.load(Ordering::Relaxed),
            stale_frames_dropped: self.capture_stale_frames_dropped.load(Ordering::Relaxed),
        }
    }

    /// Color-space target emitted by the current Windows WebView2/WGC path.
    ///
    /// WebView2 composition capture is fixed to SDR sRGB in the public APIs
    /// used here. P3 and HDR remain unsupported until WebView2 or WGC expose
    /// a configurable color-space / pixel-format control for this path.
    pub fn capture_color_pipeline(&self) -> ColorPipeline {
        ColorPipeline::Srgb
    }

    /// Texture format emitted by the current Windows WebView2/WGC path.
    pub fn capture_texture_format(&self) -> wgpu::TextureFormat {
        wgpu::TextureFormat::Bgra8Unorm
    }

    /// Poll the WGC pool once without driving the host message loop.
    ///
    /// This is the redraw-path API for externally-driven loops such as winit:
    /// `Ok(None)` means a frame has not arrived yet. Call
    /// [`Self::acquire_frame_with_timeout`] only when the caller explicitly
    /// owns a bounded wait for a first frame.
    pub fn try_acquire_frame(
        &mut self,
    ) -> Result<Option<WebView2CompositionFrame>, WebSurfaceError> {
        if self.capture_state.is_none() {
            self.start_capture()?;
        }
        let needs_nudge = self
            .capture_state
            .as_ref()
            .map(|state| !state.first_frame_emitted)
            .unwrap_or(true);
        if needs_nudge {
            let _ = self.nudge_content(FIRST_FRAME_NUDGE_LABEL);
        }

        // `try_acquire_frame` runs from a host-owned event loop (usually
        // winit's redraw path). Pumping messages here can re-enter that loop;
        // the demo observed an intermittent hang on the initial acquire as a
        // result. The bounded `acquire_frame_with_timeout` API owns its own
        // pump for callers that explicitly need to wait for a first frame.
        let frame = {
            let state = self
                .capture_state
                .as_mut()
                .expect("capture state populated above");
            if !state.frame_ready() {
                return Ok(None);
            }
            state.pool.TryGetNextFrame()
        };

        match frame {
            Ok(frame) => self.capture_frame_to_shared(frame),
            Err(_) => Ok(None),
        }
    }

    /// Wait for a frame while pumping this thread's Windows messages.
    ///
    /// Unlike [`Self::try_acquire_frame`], this is a bounded, caller-explicit
    /// operation. It is suitable for setup and diagnostics, not a winit redraw
    /// callback.
    pub fn acquire_frame_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<WebView2CompositionFrame, WebSurfaceError> {
        if self.capture_state.is_none() {
            self.start_capture()?;
        }
        let needs_nudge = self
            .capture_state
            .as_ref()
            .map(|state| !state.first_frame_emitted)
            .unwrap_or(true);
        if needs_nudge {
            let _ = self.nudge_content(FIRST_FRAME_NUDGE_LABEL);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let state = self
                .capture_state
                .as_mut()
                .expect("capture state populated above");
            if !state.frame_ready() {
                if Instant::now() < deadline {
                    pump_messages_for(Duration::from_millis(16));
                    continue;
                }
                return Err(WebSurfaceError::Platform(format!(
                    "WGC frame did not arrive within {timeout:?} for {}x{}",
                    self.size.width, self.size.height
                )));
            }
            match state.pool.TryGetNextFrame() {
                Ok(frame) => match self.capture_frame_to_shared(frame)? {
                    Some(frame) => return Ok(frame),
                    None if Instant::now() < deadline => {
                        pump_messages_for(Duration::from_millis(16))
                    }
                    None => {
                        return Err(WebSurfaceError::NotReady(
                            "WGC only returned stale frames before the acquire timeout",
                        ));
                    }
                },
                Err(_) if Instant::now() < deadline => pump_messages_for(Duration::from_millis(16)),
                Err(error) => {
                    return Err(WebSurfaceError::Platform(format!(
                        "TryGetNextFrame timed out after {timeout:?} for {}x{}: {error}",
                        self.size.width, self.size.height
                    )));
                }
            }
        }
    }

    fn capture_frame_to_shared(
        &mut self,
        frame: windows::Graphics::Capture::Direct3D11CaptureFrame,
    ) -> Result<Option<WebView2CompositionFrame>, WebSurfaceError> {
        let content_size = frame
            .ContentSize()
            .map_err(platform("Direct3D11CaptureFrame::ContentSize"))?;
        self.capture_samples_received
            .fetch_add(1, Ordering::Relaxed);
        let captured_size =
            PhysicalSize::new(content_size.Width as u32, content_size.Height as u32);
        if captured_size != self.size {
            self.capture_stale_frames_dropped
                .fetch_add(1, Ordering::Relaxed);
            let _ = frame.Close();
            return Ok(None);
        }
        let surface = frame
            .Surface()
            .map_err(platform("Direct3D11CaptureFrame::Surface"))?;
        let access = surface
            .cast::<IDirect3DDxgiInterfaceAccess>()
            .map_err(platform(
                "IDirect3DSurface cast to IDirect3DDxgiInterfaceAccess",
            ))?;
        let texture = unsafe { access.GetInterface::<ID3D11Texture2D>() }
            .map_err(platform("GetInterface<ID3D11Texture2D>"))?;
        let raw_texture = Interface::as_raw(&texture);
        self.generation = self.generation.saturating_add(1);
        let allocated_now = self.ensure_persistent_dest(captured_size)?;
        let dest = self
            .persistent_dest
            .as_mut()
            .expect("persistent_dest populated above");
        let fence_value = self.capture_factory.copy_capture_into_existing_target(
            &dest.texture.texture,
            WebView2D3D11CaptureFrame {
                size: captured_size,
                format: wgpu::TextureFormat::Bgra8Unorm,
                generation: self.resource_epoch,
                raw_d3d11_texture: raw_texture,
            },
        )?;
        let _ = frame.Close();
        if let Some(state) = self.capture_state.as_mut() {
            state.first_frame_emitted = true;
        }
        let resource_is_new = allocated_now || !dest.handle_handed_off;
        let shared_handle = if resource_is_new {
            dest.handle_handed_off = true;
            dest.texture.shared_frame.shared_handle
        } else {
            std::ptr::null_mut()
        };
        let surface_frame = WebView2DxgiSharedHandleFrame {
            size: captured_size,
            format: wgpu::TextureFormat::Bgra8Unorm,
            generation: self.resource_epoch,
            shared_handle,
            producer_sync: self.capture_factory.sync_mechanism(),
            fence_value,
        }
        .into_surface_frame();
        let webview_frame = WebView2CompositionFrame {
            frame: surface_frame,
            content_size: captured_size,
            generation: self.generation,
            shared_handle,
            resource_is_new,
        };
        self.capture_samples_consumed
            .fetch_add(1, Ordering::Relaxed);
        Ok(Some(webview_frame))
    }

    fn ensure_persistent_dest(&mut self, size: PhysicalSize<u32>) -> Result<bool, WebSurfaceError> {
        if self
            .persistent_dest
            .as_ref()
            .map(|dest| dest.size == size)
            .unwrap_or(false)
        {
            return Ok(false);
        }
        self.persistent_dest = None;
        self.resource_epoch = self.resource_epoch.saturating_add(1);
        let texture = self.capture_factory.create_shared_texture(
            size,
            wgpu::TextureFormat::Bgra8Unorm,
            self.resource_epoch,
        )?;
        self.persistent_dest = Some(PersistentDest {
            texture,
            size,
            handle_handed_off: false,
        });
        Ok(true)
    }

    fn start_capture(&mut self) -> Result<(), WebSurfaceError> {
        let started = Instant::now();
        if !GraphicsCaptureSession::IsSupported()
            .map_err(platform("GraphicsCaptureSession::IsSupported"))?
        {
            return Err(WebSurfaceError::Unsupported(
                "Windows.Graphics.Capture is not supported in this session",
            ));
        }
        let visual: Visual = self
            .webview_visual
            .cast()
            .map_err(platform("webview_visual cast to Visual"))?;
        let item = GraphicsCaptureItem::CreateFromVisual(&visual)
            .map_err(platform("GraphicsCaptureItem::CreateFromVisual"))?;
        let item_size = item.Size().map_err(platform("GraphicsCaptureItem::Size"))?;
        if item_size.Width <= 0 || item_size.Height <= 0 {
            return Err(WebSurfaceError::Platform(format!(
                "GraphicsCaptureItem returned invalid size {}x{}",
                item_size.Width, item_size.Height
            )));
        }
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &self.capture_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item_size,
        )
        .map_err(platform("Direct3D11CaptureFramePool::CreateFreeThreaded"))?;
        let frame_arrivals = Arc::new(AtomicU64::new(0));
        let frame_arrived_handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new({
                let frame_arrivals = frame_arrivals.clone();
                move |_, _| {
                    frame_arrivals.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            });
        let frame_arrived_token = pool
            .FrameArrived(&frame_arrived_handler)
            .map_err(platform("Direct3D11CaptureFramePool::FrameArrived"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(platform("CreateCaptureSession"))?;
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().map_err(platform("StartCapture"))?;
        self.capture_state = Some(CaptureState {
            item,
            pool,
            session,
            frame_arrivals,
            frame_arrivals_observed: 0,
            frame_arrived_token,
            first_frame_emitted: false,
        });
        eprintln!(
            "[producer] start_capture: {}x{} ready in {}ms",
            item_size.Width,
            item_size.Height,
            started.elapsed().as_millis()
        );
        Ok(())
    }

    pub fn nudge_content(&self, label: &str) -> Result<(), WebSurfaceError> {
        let _ = label;
        Ok(())
    }

    pub fn webview(&self) -> &ICoreWebView2 {
        &self.webview
    }

    pub fn controller(&self) -> &ICoreWebView2Controller {
        &self.controller
    }
}

impl CaptureState {
    fn frame_ready(&mut self) -> bool {
        take_unobserved_arrival(&self.frame_arrivals, &mut self.frame_arrivals_observed)
    }

    fn mark_arrivals_observed(&mut self) {
        mark_arrivals_observed(&self.frame_arrivals, &mut self.frame_arrivals_observed);
    }
}

fn take_unobserved_arrival(arrivals: &AtomicU64, observed: &mut u64) -> bool {
    let current = arrivals.load(Ordering::Relaxed);
    if current == *observed {
        return false;
    }
    *observed = current;
    true
}

fn mark_arrivals_observed(arrivals: &AtomicU64, observed: &mut u64) {
    *observed = arrivals.load(Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{mark_arrivals_observed, take_unobserved_arrival};

    #[test]
    fn visual_reparent_keeps_pending_capture_and_discards_only_pre_move_arrivals() {
        let arrivals = AtomicU64::new(4);
        let mut observed = 1;

        // A visual-host move retains the pool/session and advances only the
        // observation cursor, so old queued frames are not presented after
        // the move.
        mark_arrivals_observed(&arrivals, &mut observed);
        assert_eq!(observed, 4);
        assert!(!take_unobserved_arrival(&arrivals, &mut observed));

        // The same retained callback stream still exposes the first frame
        // that arrives after reparenting.
        arrivals.fetch_add(1, Ordering::Relaxed);
        assert!(take_unobserved_arrival(&arrivals, &mut observed));
        assert_eq!(observed, 5);
    }
}

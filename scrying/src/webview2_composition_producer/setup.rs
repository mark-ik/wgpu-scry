use super::*;

use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SW_SHOWNOACTIVATE, ShowWindow, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

/// Shared per-HWND composition resources. Windows allows exactly one
/// `DesktopWindowTarget` per HWND — a second `CreateDesktopWindowTarget` on the
/// same HWND fails with `DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`. To host
/// several WebViews under one OS window, attach multiple producers to a single
/// `CompositionRoot` via [`WebView2CompositionProducer::new_attached`] rather
/// than giving each producer its own target.
pub struct CompositionRoot {
    pub(super) parent_hwnd: HWND,
    pub(super) compositor: Compositor,
    /// Held to keep the per-HWND target alive for the lifetime of every
    /// attached producer; not otherwise read after construction.
    #[allow(dead_code)]
    desktop_target: windows::UI::Composition::Desktop::DesktopWindowTarget,
    pub(super) root_visual: ContainerVisual,
    /// A private off-screen host window this root owns (capture-only hosting via
    /// [`CompositionRoot::new_offscreen`]). `None` when bound to a consumer's
    /// HWND. Declared last so it is dropped — and the window destroyed — only
    /// after `desktop_target` releases its hold on the HWND.
    #[allow(dead_code)]
    owned_host: Option<OwnedHostWindow>,
}

impl CompositionRoot {
    /// Create the shared composition tree for `parent_hwnd` and bind it to the
    /// HWND's (single) `DesktopWindowTarget`.
    ///
    /// # Safety
    ///
    /// `parent_hwnd` must be a live top-level HWND for the lifetime of the
    /// returned root and every producer attached to it.
    pub unsafe fn new(parent_hwnd: *mut std::ffi::c_void) -> Result<Arc<Self>, WebSurfaceError> {
        let parent_hwnd = host_hwnd(parent_hwnd)?;
        // Safety: forwarded from this fn's `# Safety` contract.
        unsafe { Self::build(parent_hwnd, None) }
    }

    /// Capture-only hosting: bind the composition tree to a private off-screen
    /// host window this root creates and owns, instead of a consumer window. The
    /// WebView visual is then never displayed — the consumer composites the
    /// WGC-captured texture into its own scene — so it can never occlude the
    /// consumer's own rendering. The host is a top-level tool window positioned
    /// off every monitor and shown without activation, so DWM keeps compositing it
    /// for capture (a minimized / `SW_HIDE` window would be throttled) while it
    /// stays invisible and never steals focus.
    ///
    /// `size` only needs to anchor the host; capture is `CreateFromVisual` off
    /// each pane's own visual at the visual's size, independent of the host window
    /// size (proven by `--composition-focus-hwnd-test`, which captures 260x220
    /// panes on a 1x1 host), so a modest size suffices.
    pub fn new_offscreen(size: PhysicalSize<u32>) -> Result<Arc<Self>, WebSurfaceError> {
        let host = OwnedHostWindow::new_offscreen(size)?;
        let hwnd = host.hwnd;
        // Safety: `hwnd` is a live top-level window owned by `host`, which is moved
        // into the returned root and so outlives the target bound to it.
        unsafe { Self::build(hwnd, Some(host)) }
    }

    /// Shared core of [`new`](Self::new) / [`new_offscreen`](Self::new_offscreen):
    /// build the compositor, bind a `DesktopWindowTarget` to `parent_hwnd`, and
    /// set the root visual.
    ///
    /// # Safety
    ///
    /// `parent_hwnd` must be a live top-level HWND for the lifetime of the
    /// returned root (satisfied by the caller, or by `owned_host` owning it).
    unsafe fn build(
        parent_hwnd: HWND,
        owned_host: Option<OwnedHostWindow>,
    ) -> Result<Arc<Self>, WebSurfaceError> {
        // `Windows.UI.Composition.Compositor` requires a `DispatcherQueue` on the
        // calling thread. A consumer's own message-loop thread may not have one
        // (winit, for instance, does not create it), so ensure it here before
        // `Compositor::new` rather than making every consumer set it up.
        ensure_dispatcher_queue()?;
        let compositor = Compositor::new().map_err(platform("Compositor::new"))?;
        let desktop_interop: ICompositorDesktopInterop = compositor
            .cast()
            .map_err(platform("Compositor cast to ICompositorDesktopInterop"))?;
        let desktop_target =
            unsafe { desktop_interop.CreateDesktopWindowTarget(parent_hwnd, false) }
                .map_err(platform("CreateDesktopWindowTarget"))?;
        let root_visual = compositor
            .CreateContainerVisual()
            .map_err(platform("CreateContainerVisual (root)"))?;
        desktop_target
            .SetRoot(&root_visual)
            .map_err(platform("DesktopWindowTarget::SetRoot"))?;
        Ok(Arc::new(Self {
            parent_hwnd,
            compositor,
            desktop_target,
            root_visual,
            owned_host,
        }))
    }

    /// Prepare a new target on this root's existing compositor without moving
    /// its root visual yet. A composition visual can be the root of only one
    /// target, so assigning it directly while `desktop_target` still owns it
    /// fails with `E_INVALIDARG`.
    ///
    /// # Safety
    ///
    /// `parent_hwnd` must be a live top-level HWND on this composition thread.
    unsafe fn prepare_reparent_to_hwnd(
        &self,
        parent_hwnd: *mut std::ffi::c_void,
    ) -> Result<PreparedCompositionRootHost, WebSurfaceError> {
        let parent_hwnd = host_hwnd(parent_hwnd)?;
        let desktop_interop: ICompositorDesktopInterop = self
            .compositor
            .cast()
            .map_err(platform("Compositor cast to ICompositorDesktopInterop"))?;
        let desktop_target =
            unsafe { desktop_interop.CreateDesktopWindowTarget(parent_hwnd, false) }
                .map_err(platform("CreateDesktopWindowTarget (reparent)"))?;
        Ok(PreparedCompositionRootHost {
            parent_hwnd,
            desktop_target,
        })
    }

    /// Move the root visual from the committed target to `prepared`.
    ///
    /// This is deliberately an explicit detach/attach transaction: WinComp
    /// rejects a visual that is still the root of another target. If the
    /// destination rejects the visual, release that candidate before putting
    /// the visual back on the source target. A failed source restore has no
    /// truthful ordinary-failure state and is reported as indeterminate.
    fn activate_prepared_reparent(
        &self,
        prepared: PreparedCompositionRootHost,
    ) -> Result<PreparedCompositionRootHost, WebSurfaceError> {
        self.desktop_target
            .SetRoot(None::<&Visual>)
            .map_err(platform(
                "DesktopWindowTarget::SetRoot (detach reparent source)",
            ))?;
        if let Err(error) = prepared.desktop_target.SetRoot(&self.root_visual) {
            drop(prepared);
            if let Err(restore_error) = self.restore_current_target() {
                return Err(WebSurfaceError::HostMigrationIndeterminate(format!(
                    "DesktopWindowTarget::SetRoot (reparent) returned {error} after detaching the source root, and source-root restore failed: {restore_error}"
                )));
            }
            return Err(platform("DesktopWindowTarget::SetRoot (reparent)")(error));
        }
        Ok(prepared)
    }

    /// Commit a prepared target after the WebView2 controller has moved.
    /// Replacing the field drops the old target only at this commit point.
    fn commit_reparent(&mut self, prepared: PreparedCompositionRootHost) {
        self.desktop_target = prepared.desktop_target;
        self.parent_hwnd = prepared.parent_hwnd;
    }

    /// Reassert this root on its currently committed target after the
    /// destination candidate has been dropped.
    fn restore_current_target(&self) -> Result<(), WebSurfaceError> {
        self.desktop_target
            .SetRoot(&self.root_visual)
            .map_err(platform("DesktopWindowTarget::SetRoot (restore reparent)"))
    }
}

struct PreparedCompositionRootHost {
    parent_hwnd: HWND,
    desktop_target: windows::UI::Composition::Desktop::DesktopWindowTarget,
}

fn host_hwnd(parent_hwnd: *mut std::ffi::c_void) -> Result<HWND, WebSurfaceError> {
    if parent_hwnd.is_null() {
        return Err(WebSurfaceError::Platform(
            "parent HWND was null".to_string(),
        ));
    }
    Ok(HWND(parent_hwnd))
}

/// A top-level off-screen window owned by a capture-only [`CompositionRoot`].
/// Destroyed on drop.
struct OwnedHostWindow {
    hwnd: HWND,
}

impl OwnedHostWindow {
    fn new_offscreen(size: PhysicalSize<u32>) -> Result<Self, WebSurfaceError> {
        let width = size.width.max(1) as i32;
        let height = size.height.max(1) as i32;
        // Built-in `STATIC` class (no class registration needed, matching the
        // demo's HWND smokes). `WS_POPUP` top-level so it can carry its own
        // `DesktopWindowTarget`; tool window + `NOACTIVATE` keep it out of the
        // taskbar / alt-tab and stop it stealing focus; positioned far off every
        // monitor so it is never visible.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                -32000,
                -32000,
                width,
                height,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(platform("CreateWindowExW (offscreen host)"))?;
        // Shown (not minimized / `SW_HIDE`) so DWM keeps compositing it for WGC;
        // the off-screen position keeps it invisible, `SHOWNOACTIVATE` avoids any
        // focus change.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(Self { hwnd })
    }
}

impl Drop for OwnedHostWindow {
    fn drop(&mut self) {
        // Safety: `hwnd` was created by `new_offscreen` and is destroyed only here.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Ensure a `DispatcherQueue` exists on the current (UI) thread, which
/// `Windows.UI.Composition.Compositor` requires before it can be created.
/// Idempotent: a queue the consumer already created (or one made by an earlier
/// call here) is honored; otherwise a `DispatcherQueueController` is created for
/// the current thread and retained thread-locally for the thread's lifetime
/// (dropping the controller would tear the queue down).
fn ensure_dispatcher_queue() -> Result<(), WebSurfaceError> {
    use std::cell::RefCell;

    use windows::System::{DispatcherQueue, DispatcherQueueController};
    use windows::Win32::System::WinRT::{
        CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
        DispatcherQueueOptions,
    };

    thread_local! {
        static CONTROLLER: RefCell<Option<DispatcherQueueController>> =
            const { RefCell::new(None) };
    }

    // We already created one on this thread.
    if CONTROLLER.with(|c| c.borrow().is_some()) {
        return Ok(());
    }
    // The consumer may have created its own (e.g. the demo keeps one); honor it.
    if DispatcherQueue::GetForCurrentThread().is_ok() {
        return Ok(());
    }
    let controller = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })
    }
    .map_err(platform("CreateDispatcherQueueController"))?;
    CONTROLLER.with(|c| *c.borrow_mut() = Some(controller));
    Ok(())
}

impl WebView2CompositionProducer {
    /// Build a single-pane producer: create a private [`CompositionRoot`] for
    /// `parent_hwnd`, then attach one WebView to it. Capture is not started
    /// until the first `acquire_frame` call.
    ///
    /// # Safety
    ///
    /// `parent_hwnd` must be a live top-level HWND for the lifetime of the
    /// returned producer.
    pub unsafe fn new(
        parent_hwnd: *mut std::ffi::c_void,
        config: WebView2CompositionConfig,
    ) -> Result<Self, WebSurfaceError> {
        let composition_root = unsafe { CompositionRoot::new(parent_hwnd)? };
        unsafe { Self::new_attached(&composition_root, config) }
    }

    /// Attach a WebView to an existing [`CompositionRoot`] so several producers
    /// can share one HWND's `DesktopWindowTarget`. Each producer gets its own
    /// pane container visual (positioned by `config.offset`), WebView2
    /// environment, controller, and capture pipeline.
    ///
    /// # Safety
    ///
    /// The `CompositionRoot`'s parent HWND must remain a live top-level HWND
    /// for the lifetime of the returned producer.
    pub unsafe fn new_attached(
        composition_root: &Arc<CompositionRoot>,
        config: WebView2CompositionConfig,
    ) -> Result<Self, WebSurfaceError> {
        if config.size.width == 0 || config.size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WebView2 producer size must be non-zero, got {}x{}",
                config.size.width, config.size.height
            )));
        }

        let parent_hwnd = composition_root.parent_hwnd;
        let compositor = &composition_root.compositor;

        // Per-pane container: a child of the shared root carrying this pane's
        // offset and size. The diagnostic sprite and the webview visual live
        // inside it, so `set_offset` / `resize` move/resize one pane without
        // disturbing siblings.
        let pane_container = compositor
            .CreateContainerVisual()
            .map_err(platform("CreateContainerVisual (pane)"))?;
        pane_container
            .SetOffset(Vector3 {
                X: config.offset.0,
                Y: config.offset.1,
                Z: 0.0,
            })
            .map_err(platform("ContainerVisual::SetOffset (pane)"))?;
        let visual_size = Vector2 {
            X: config.size.width as f32,
            Y: config.size.height as f32,
        };
        pane_container
            .SetSize(visual_size)
            .map_err(platform("ContainerVisual::SetSize (pane)"))?;

        if let Some((r, g, b)) = config.diagnostic_backdrop {
            let sprite = compositor
                .CreateSpriteVisual()
                .map_err(platform("CreateSpriteVisual (diagnostic)"))?;
            sprite
                .SetSize(visual_size)
                .map_err(platform("SpriteVisual::SetSize"))?;
            let brush = compositor
                .CreateColorBrushWithColor(windows::UI::Color {
                    A: 255,
                    R: r,
                    G: g,
                    B: b,
                })
                .map_err(platform("CreateColorBrushWithColor"))?;
            sprite
                .SetBrush(&brush)
                .map_err(platform("SpriteVisual::SetBrush"))?;
            pane_container
                .Children()
                .map_err(platform("pane.Children()"))?
                .InsertAtBottom(&sprite)
                .map_err(platform("Children::InsertAtBottom"))?;
        }

        let webview_visual = compositor
            .CreateContainerVisual()
            .map_err(platform("CreateContainerVisual (webview)"))?;
        webview_visual
            .SetSize(visual_size)
            .map_err(platform("ContainerVisual::SetSize (webview)"))?;
        pane_container
            .Children()
            .map_err(platform("pane.Children() (webview)"))?
            .InsertAtTop(&webview_visual)
            .map_err(platform("Children::InsertAtTop (webview)"))?;
        composition_root
            .root_visual
            .Children()
            .map_err(platform("root.Children() (pane)"))?
            .InsertAtTop(&pane_container)
            .map_err(platform("Children::InsertAtTop (pane)"))?;

        let environment = create_environment(&config.user_data_dir)?;
        let composition_controller =
            create_composition_controller(&environment, parent_hwnd, config.non_persistent)?;
        unsafe {
            composition_controller
                .SetRootVisualTarget(&webview_visual)
                .map_err(platform("SetRootVisualTarget"))?;
        }
        let controller: ICoreWebView2Controller = composition_controller
            .cast()
            .map_err(platform("composition controller cast"))?;
        unsafe {
            controller
                .SetBounds(RECT {
                    left: 0,
                    top: 0,
                    right: config.size.width as i32,
                    bottom: config.size.height as i32,
                })
                .map_err(platform("controller.SetBounds"))?;
            controller
                .SetIsVisible(true)
                .map_err(platform("controller.SetIsVisible"))?;
        }
        let webview =
            unsafe { controller.CoreWebView2() }.map_err(platform("controller.CoreWebView2"))?;

        let capture_factory = match config.fence_shared_handle {
            Some(handle) => D3D11SharedTextureFactory::new_hardware_with_fence(handle)?,
            None => D3D11SharedTextureFactory::new_hardware()?,
        };
        let capture_device = capture_factory.create_winrt_direct3d_device()?;

        let nav_event_queue: Arc<Mutex<VecDeque<NavigationEvent>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let web_message_queue: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let cursor_queue: Arc<Mutex<VecDeque<CursorShape>>> = Arc::new(Mutex::new(VecDeque::new()));
        let pending_cookies = Arc::new(Mutex::new(None));
        let pending_find = Arc::new(Mutex::new(None));
        let pending_pdf = Arc::new(Mutex::new(None));
        let cookie_change_handler = Arc::new(Mutex::new(None));
        let download_handler = Arc::new(Mutex::new(None));
        let auth_handler = Arc::new(Mutex::new(None));
        let permission_handler = Arc::new(Mutex::new(None));
        let download_registry = Arc::new(Mutex::new(WebView2DownloadRegistry::default()));
        let download_id_allocator = Arc::new(DownloadIdAllocator::new());
        let resource_handlers = Arc::new(Mutex::new(HashMap::new()));
        let default_context_menus_enabled = Arc::new(Mutex::new(false));

        cookies::install_cookie_change_bridge(&webview)?;
        browser::install_context_menu_bridge(&webview)?;
        browser::install_drop_detected_bridge(&webview)?;
        browser::install_media_capture_bridge(&webview)?;
        browser::install_text_input_bridge(&webview)?;

        let (
            nav_starting_token,
            nav_completed_token,
            source_changed_token,
            title_changed_token,
            new_window_requested_token,
            process_failed_token,
            web_message_token,
        ) = navigation::register_persistent_handlers(
            &webview,
            nav_event_queue.clone(),
            web_message_queue.clone(),
            cookie_change_handler.clone(),
        )?;
        let context_menu_requested_token = browser::register_context_menu_requested_handler(
            &webview,
            nav_event_queue.clone(),
            default_context_menus_enabled.clone(),
        )?;
        let accelerator_key_pressed_token =
            input::register_accelerator_key_pressed_handler(&controller, nav_event_queue.clone())?;
        let cursor_changed_token =
            input::register_cursor_changed_handler(&composition_controller, cursor_queue.clone())?;
        let download_starting_token = downloads::register_download_starting_handler(
            &webview,
            nav_event_queue.clone(),
            config.download_dir.clone(),
            download_handler.clone(),
            download_registry.clone(),
            download_id_allocator.clone(),
        )?;
        let basic_auth_token = auth_permissions::register_basic_auth_handler(
            &webview,
            nav_event_queue.clone(),
            auth_handler.clone(),
            download_registry.clone(),
        )?;
        let permission_requested_token = auth_permissions::register_permission_requested_handler(
            &webview,
            permission_handler.clone(),
        )?;
        let web_resource_response_received_token =
            browser::register_web_resource_response_received_handler(
                &webview,
                cookie_change_handler.clone(),
            )?;

        Ok(Self {
            parent_hwnd,
            size: config.size,
            generation: 0,
            resource_epoch: 0,
            composition_root: composition_root.clone(),
            pane_container,
            webview_visual,
            environment,
            composition_controller,
            controller,
            webview,
            capture_factory,
            capture_device,
            capture_state: None,
            persistent_dest: None,
            capture_samples_received: AtomicU64::new(0),
            capture_samples_consumed: AtomicU64::new(0),
            capture_stale_frames_dropped: AtomicU64::new(0),
            nav_event_queue,
            web_message_queue,
            cursor_queue,
            pending_cookies,
            pending_find,
            pending_pdf,
            cookie_change_handler,
            download_handler,
            auth_handler,
            permission_handler,
            download_registry,
            resource_handlers,
            default_context_menus_enabled,
            nav_starting_token,
            nav_completed_token,
            source_changed_token,
            title_changed_token,
            new_window_requested_token,
            process_failed_token,
            download_starting_token,
            basic_auth_token,
            permission_requested_token,
            context_menu_requested_token,
            web_message_token,
            web_resource_response_received_token,
            web_resource_requested_token: None,
            accelerator_key_pressed_token,
            cursor_changed_token,
        })
    }

    /// Move this producer to `parent_hwnd` without recreating the WebView2
    /// controller, page, profile, or D3D capture device.
    ///
    /// A producer constructed with [`Self::new_attached`] deliberately shares
    /// its composition root with sibling producers and cannot be migrated by
    /// itself. This method rejects that shape instead of moving unrelated
    /// panes. Construct a standalone producer with [`Self::new`] for a
    /// movable native host.
    ///
    /// The Windows Graphics Capture session is restarted because its capture
    /// item is tied to the composition visual's destination. The next
    /// `try_acquire_frame` starts it again; the existing shared-device factory
    /// and the imported-resource contract are retained.
    ///
    /// # Safety
    ///
    /// `parent_hwnd` must be a live top-level HWND on this producer's UI
    /// thread and must outlive the producer or a later successful migration.
    pub unsafe fn reparent_to_hwnd(
        &mut self,
        parent_hwnd: *mut std::ffi::c_void,
    ) -> Result<(), WebSurfaceError> {
        let parent_hwnd = host_hwnd(parent_hwnd)?;
        if parent_hwnd == self.parent_hwnd {
            return Ok(());
        }

        let old_parent_hwnd = self.parent_hwnd;
        let controller = self.controller.clone();
        let composition_root = Arc::get_mut(&mut self.composition_root).ok_or_else(|| {
            WebSurfaceError::Unsupported(
                "cannot reparent a WebView2 producer attached to a shared CompositionRoot",
            )
        })?;

        // Create the destination target first, then explicitly move the root
        // visual off its source target. WinComp does not permit a visual to be
        // root of both targets at once. `activate_prepared_reparent` restores
        // the source before it returns an ordinary error.
        let prepared = unsafe { composition_root.prepare_reparent_to_hwnd(parent_hwnd.0) }?;
        let prepared = composition_root.activate_prepared_reparent(prepared)?;
        match unsafe { controller.SetParentWindow(parent_hwnd) } {
            Ok(()) => composition_root.commit_reparent(prepared),
            Err(error) => {
                let mut observed_parent = HWND(std::ptr::null_mut());
                match unsafe { controller.ParentWindow(&mut observed_parent) } {
                    Ok(()) if observed_parent == old_parent_hwnd => {
                        drop(prepared);
                        if let Err(restore_error) = composition_root.restore_current_target() {
                            return Err(WebSurfaceError::HostMigrationIndeterminate(format!(
                                "controller.SetParentWindow returned {error}, source host was retained, but its composition root could not be restored: {restore_error}"
                            )));
                        }
                        return Err(platform("controller.SetParentWindow")(error));
                    }
                    Ok(()) if observed_parent == parent_hwnd => {
                        // The controller committed despite the failing HRESULT.
                        // Complete the paired composition move and report the
                        // observed host as the successful terminal state.
                        composition_root.commit_reparent(prepared);
                    }
                    Ok(()) => {
                        // Preserve the candidate target because it currently
                        // owns the root visual. The controller's terminal host
                        // is unknown, so neither source nor destination
                        // custody can be reported truthfully as ordinary.
                        composition_root.commit_reparent(prepared);
                        self.parent_hwnd = parent_hwnd;
                        return Err(WebSurfaceError::HostMigrationIndeterminate(format!(
                            "controller.SetParentWindow returned {error}, then reported unexpected parent {:p} (source {:p}, destination {:p}); composition root remains on destination",
                            observed_parent.0, old_parent_hwnd.0, parent_hwnd.0,
                        )));
                    }
                    Err(observe_error) => {
                        // As above, keep the root's known destination target
                        // alive rather than dropping it into an unobservable
                        // state while the controller host is unknown.
                        composition_root.commit_reparent(prepared);
                        self.parent_hwnd = parent_hwnd;
                        return Err(WebSurfaceError::HostMigrationIndeterminate(format!(
                            "controller.SetParentWindow returned {error}, and ParentWindow could not determine the terminal host: {observe_error}; composition root remains on destination",
                        )));
                    }
                }
            }
        }
        // `SetParentWindow` is the migration commit point. Reporting an
        // error after it succeeds would make callers retain source custody
        // even though the page has already moved. Position notification is
        // only a best-effort layout refresh; WebView2 will still receive the
        // next bounds/position update from the destination host.
        self.parent_hwnd = parent_hwnd;
        self.force_restart_capture();
        if let Err(error) = unsafe { controller.NotifyParentWindowPositionChanged() } {
            eprintln!(
                "[producer] reparented WebView2 to {:p}, but position notification failed: {error}",
                parent_hwnd.0
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::host_hwnd;

    #[test]
    fn host_migration_rejects_a_null_hwnd() {
        assert!(host_hwnd(std::ptr::null_mut()).is_err());
    }

    #[test]
    fn host_migration_preserves_a_non_null_hwnd_value() {
        let raw = 0x1234usize as *mut std::ffi::c_void;
        assert_eq!(host_hwnd(raw).unwrap().0, raw);
    }
}

fn create_environment(user_data_dir: &Path) -> Result<ICoreWebView2Environment, WebSurfaceError> {
    if let Err(error) = std::fs::create_dir_all(user_data_dir) {
        return Err(WebSurfaceError::Platform(format!(
            "create user_data_dir {}: {error}",
            user_data_dir.display()
        )));
    }
    let user_data_dir = user_data_dir.to_string_lossy().into_owned();
    let (tx, rx) = mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            let user_data_dir = CoTaskMemPWSTR::from(user_data_dir.as_str());
            let options = CoreWebView2EnvironmentOptions::default();
            // Auto-hiding Windows-style overlay scrollbars: an embedded compat tile
            // should not show a persistent scrollbar gutter that reserves layout width
            // (the page is composited into a card, not a full window).
            unsafe {
                options.set_additional_browser_arguments(
                    "--enable-features=msOverlayScrollbarWinStyle,msOverlayScrollbarWinStyleAnimation"
                        .to_string(),
                );
            }
            unsafe {
                webview2_com::Microsoft::Web::WebView2::Win32::CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    *user_data_dir.as_ref().as_pcwstr(),
                    &ICoreWebView2EnvironmentOptions::from(options),
                    &handler,
                )
                .map_err(webview2_com::Error::WindowsError)
            }
        }),
        Box::new(move |error_code, environment| {
            error_code?;
            tx.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                .expect("send over mpsc channel");
            Ok(())
        }),
    )
    .map_err(|error| WebSurfaceError::Platform(format!("CreateCoreWebView2Environment: {error}")))?;
    rx.recv()
        .map_err(|_| {
            WebSurfaceError::Platform(
                "CreateCoreWebView2Environment completion channel closed".to_string(),
            )
        })?
        .map_err(platform("CreateCoreWebView2Environment result"))
}

fn create_composition_controller(
    environment: &ICoreWebView2Environment,
    parent_hwnd: HWND,
    non_persistent: bool,
) -> Result<ICoreWebView2CompositionController, WebSurfaceError> {
    let (tx, rx) = mpsc::channel();
    CreateCoreWebView2CompositionControllerCompletedHandler::wait_for_async_operation(
        if non_persistent {
            let environment10: ICoreWebView2Environment10 = environment
                .cast()
                .map_err(platform("environment cast to ICoreWebView2Environment10"))?;
            Box::new(move |handler| unsafe {
                let options = environment10
                    .CreateCoreWebView2ControllerOptions()
                    .map_err(webview2_com::Error::WindowsError)?;
                options
                    .SetIsInPrivateModeEnabled(true)
                    .map_err(webview2_com::Error::WindowsError)?;
                environment10
                    .CreateCoreWebView2CompositionControllerWithOptions(
                        parent_hwnd,
                        &options,
                        &handler,
                    )
                    .map_err(webview2_com::Error::WindowsError)
            })
        } else {
            let environment3: ICoreWebView2Environment3 = environment
                .cast()
                .map_err(platform("environment cast to ICoreWebView2Environment3"))?;
            Box::new(move |handler| unsafe {
                environment3
                    .CreateCoreWebView2CompositionController(parent_hwnd, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            })
        },
        Box::new(move |error_code, controller| {
            error_code?;
            tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                .expect("send over mpsc channel");
            Ok(())
        }),
    )
    .map_err(|error| {
        WebSurfaceError::Platform(format!("CreateCoreWebView2CompositionController: {error}"))
    })?;
    rx.recv()
        .map_err(|_| {
            WebSurfaceError::Platform(
                "CreateCoreWebView2CompositionController completion channel closed".to_string(),
            )
        })?
        .map_err(platform("CreateCoreWebView2CompositionController result"))
}

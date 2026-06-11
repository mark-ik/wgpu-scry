use super::*;

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
        if parent_hwnd.is_null() {
            return Err(WebSurfaceError::Platform(
                "parent HWND was null".to_string(),
            ));
        }
        let parent_hwnd = HWND(parent_hwnd);
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
        }))
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

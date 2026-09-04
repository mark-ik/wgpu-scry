// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Windows-only WebView2 composition-controller capture producer.
//!
//! This owns the moving parts the demo previously inlined:
//! - WebView2 environment / composition controller / controller / webview
//! - Windows.UI.Composition compositor, desktop window target, root + webview
//!   visuals
//! - Windows.Graphics.Capture item / frame pool / session lifecycle
//! - Post-StartCapture content invalidation nudge for the first frame
//! - Shared-handle export for the host's `wgpu-native-texture-interop` importer
//!
//! The proven flow this encapsulates was validated as:
//! 1. Create a real WebView2 composition-controller WebView.
//! 2. Attach it to a WinComp container visual.
//! 3. Feed the visual to `GraphicsCaptureItem::CreateFromVisual`.
//! 4. Start WGC capture.
//! 5. Nudge WebView content after `StartCapture`.
//! 6. Receive a `Bgra8Unorm` frame.
//! 7. Bridge D3D11 capture output into a DX12-importable native frame.

mod auth_permissions;
mod browser;
mod capture;
mod cookies;
mod downloads;
mod input;
mod navigation;
mod resources;
mod settings;
mod setup;
mod teardown;

pub use setup::CompositionRoot;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON,
    COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_CANCELED,
    COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_PAUSED, COREWEBVIEW2_DOWNLOAD_STATE,
    COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED, COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED,
    COREWEBVIEW2_KEY_EVENT_KIND, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
    COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP, COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN,
    COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_UP, COREWEBVIEW2_MOUSE_EVENT_KIND,
    COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS, COREWEBVIEW2_MOVE_FOCUS_REASON,
    COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_CAMERA,
    COREWEBVIEW2_PERMISSION_KIND_MICROPHONE, COREWEBVIEW2_PERMISSION_KIND_OTHER_SENSORS,
    COREWEBVIEW2_PERMISSION_STATE, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
    COREWEBVIEW2_PERMISSION_STATE_DEFAULT, COREWEBVIEW2_PERMISSION_STATE_DENY,
    COREWEBVIEW2_PHYSICAL_KEY_STATUS, COREWEBVIEW2_PRINT_DIALOG_KIND_BROWSER,
    COREWEBVIEW2_PROCESS_FAILED_KIND, COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED,
    COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
    COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
    COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
    ICoreWebView2, ICoreWebView2_2, ICoreWebView2_4, ICoreWebView2_10, ICoreWebView2_11,
    ICoreWebView2_16, ICoreWebView2_28, ICoreWebView2AcceleratorKeyPressedEventArgs2,
    ICoreWebView2CompositionController, ICoreWebView2Controller, ICoreWebView2Cookie,
    ICoreWebView2CookieManager, ICoreWebView2DownloadOperation, ICoreWebView2Environment,
    ICoreWebView2Environment3, ICoreWebView2Environment6, ICoreWebView2Environment10,
    ICoreWebView2Environment15, ICoreWebView2EnvironmentOptions,
    ICoreWebView2PermissionRequestedEventArgs2,
};
use webview2_com::{
    AcceleratorKeyPressedEventHandler, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    BasicAuthenticationRequestedEventHandler, BytesReceivedChangedEventHandler,
    CallDevToolsProtocolMethodCompletedHandler, CoTaskMemPWSTR, ContextMenuRequestedEventHandler,
    CoreWebView2EnvironmentOptions, CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DocumentTitleChangedEventHandler,
    DownloadStartingEventHandler, ExecuteScriptCompletedHandler, FindStartCompletedHandler,
    GetCookiesCompletedHandler, NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, PermissionRequestedEventHandler,
    PrintToPdfStreamCompletedHandler, ProcessFailedEventHandler, SourceChangedEventHandler,
    StateChangedEventHandler, WebMessageReceivedEventHandler, WebResourceRequestedEventHandler,
    WebResourceResponseReceivedEventHandler,
};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat};
use windows::UI::Composition::{Compositor, ContainerVisual, Visual};
use windows::Win32::Foundation::{E_POINTER, HGLOBAL, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{CoTaskMemFree, IStream, STREAM_SEEK_SET};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, HCURSOR, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP,
    IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT,
    LoadCursorW, MSG, PM_REMOVE, PeekMessageW, PostMessageW, TranslateMessage, WM_CHAR,
    WM_DEADCHAR, WM_IME_CHAR, WM_IME_COMPOSITION, WM_IME_COMPOSITIONFULL, WM_IME_CONTROL,
    WM_IME_ENDCOMPOSITION, WM_IME_KEYDOWN, WM_IME_KEYUP, WM_IME_NOTIFY, WM_IME_REQUEST,
    WM_IME_SELECT, WM_IME_SETCONTEXT, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP, WM_SYSCHAR,
    WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows::core::{IInspectable, Interface, PCWSTR, PWSTR};
use windows_numerics::{Vector2, Vector3};

use crate::{
    AcceleratorKeyEvent, AuthChallenge, AuthDisposition, AuthSource, ColorPipeline, Cookie,
    CursorShape, DownloadDecision, DownloadDestinationRequest, DownloadId, FocusReason,
    KeyEventKind, KeyboardInput, MouseEventKind, MouseInput, NavigationEvent, PermissionDecision,
    PermissionKind, PermissionRequest, PhysicalKeyStatus, TextInputRect, TextInputState,
    UrlSchemeHandlerFn, UrlSchemeResponse,
};

use crate::windows_capture::{
    D3D11SharedTexture, D3D11SharedTextureFactory, WebView2D3D11CaptureFrame,
    WebView2DxgiSharedHandleFrame,
};
use crate::{
    SystemWebviewBackend, WebSurfaceCapabilities, WebSurfaceError, WebSurfaceFrame, WebSurfaceMode,
};

use downloads::{DownloadIdAllocator, WebView2DownloadRegistry};

const FIRST_FRAME_NUDGE_LABEL: &str = "WebView2CompositionProducer.first-frame";
const COOKIE_CHANGE_BRIDGE_MESSAGE: &str = "\0scrying:cookie-change";
const CONTEXT_MENU_BRIDGE_PREFIX: &str = "scrying:context-menu:";
const DROP_DETECTED_BRIDGE_PREFIX: &str = "scrying:drop-detected:";
const MEDIA_CAPTURE_BRIDGE_PREFIX: &str = "scrying:media-capture:";
const TEXT_INPUT_BRIDGE_PREFIX: &str = "scrying:text-input:";
const MAX_MESSAGES_PER_PUMP_SLICE: usize = 256;

pub type WebView2CookieChangeHandlerFn = Box<dyn Fn() + Send + Sync + 'static>;
pub type WebView2DownloadHandlerFn =
    Box<dyn Fn(DownloadDestinationRequest) -> DownloadDecision + Send + Sync + 'static>;
pub type WebView2AuthHandlerFn =
    Box<dyn Fn(AuthChallenge) -> AuthDisposition + Send + Sync + 'static>;
pub type WebView2PermissionHandlerFn =
    Box<dyn Fn(PermissionRequest) -> PermissionDecision + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug)]
pub struct WebView2FindOptions {
    pub case_sensitive: bool,
    pub highlight_all_matches: bool,
    pub match_word: bool,
    pub suppress_default_find_dialog: bool,
    pub backwards: bool,
}

impl Default for WebView2FindOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            highlight_all_matches: true,
            match_word: false,
            suppress_default_find_dialog: true,
            backwards: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WebView2FindResult {
    pub matched: bool,
    pub active_match_index: i32,
    pub match_count: i32,
}

/// Configuration for `WebView2CompositionProducer::new`.
#[derive(Clone, Debug)]
pub struct WebView2CompositionConfig {
    /// Initial size of the WebView visual and capture region.
    pub size: PhysicalSize<u32>,
    /// Offset of the root visual relative to the parent window.
    pub offset: (f32, f32),
    /// User-data directory for the WebView2 environment. Created if missing.
    pub user_data_dir: PathBuf,
    /// Default directory for WebView2 downloads when no host download handler
    /// overrides the destination.
    pub download_dir: PathBuf,
    /// When `true`, create the CompositionController in WebView2 InPrivate
    /// mode. Cookies, local storage, and IndexedDB are scoped to this
    /// controller and are discarded when it is dropped. The `user_data_dir`
    /// is still supplied to WebView2 as the environment root, but this
    /// producer does not persist page storage into that profile.
    pub non_persistent: bool,
    /// Optional CSS color used for a sprite visual placed under the WebView
    /// visual. Mostly useful as a diagnostic backstop while the WebView paints.
    pub diagnostic_backdrop: Option<(u8, u8, u8)>,
    /// Timeout for the navigation-completed wait inside `navigate_to_string`.
    pub navigation_timeout: Duration,
    /// Timeout for `acquire_frame` to wait on `TryGetNextFrame`.
    pub frame_timeout: Duration,
    /// Optional NT shared handle to a `D3D12_FENCE_FLAG_SHARED` fence
    /// (typically from
    /// `crate::native_frame::Dx12FenceSynchronizer::shared_handle`).
    /// When `Some`, the producer opens the fence on its D3D11 device and
    /// signals it after each `CopyResource` instead of using a keyed mutex
    /// + CPU spin. Frames are then emitted with
    /// `producer_sync = SyncMechanism::ExplicitFence` and a per-frame
    /// `fence_value`. The consumer-side `Dx12FenceSynchronizer` owns the
    /// canonical handle; the producer never closes it.
    pub fence_shared_handle: Option<*mut std::ffi::c_void>,
}

impl WebView2CompositionConfig {
    pub fn new(size: PhysicalSize<u32>, user_data_dir: impl Into<PathBuf>) -> Self {
        let user_data_dir = user_data_dir.into();
        let download_dir = user_data_dir.join("downloads");
        Self {
            size,
            offset: (0.0, 0.0),
            user_data_dir,
            download_dir,
            non_persistent: false,
            diagnostic_backdrop: None,
            navigation_timeout: Duration::from_secs(5),
            frame_timeout: Duration::from_secs(2),
            fence_shared_handle: None,
        }
    }

    /// Switch this config into WebView2 InPrivate / non-persistent mode.
    /// Cookie, local-storage, and IndexedDB activity for this producer does
    /// not touch the persistent `user_data_dir` profile and is wiped on drop.
    pub fn non_persistent(mut self) -> Self {
        self.non_persistent = true;
        self
    }

    pub fn with_offset(mut self, x: f32, y: f32) -> Self {
        self.offset = (x, y);
        self
    }

    pub fn with_diagnostic_backdrop(mut self, rgb: (u8, u8, u8)) -> Self {
        self.diagnostic_backdrop = Some(rgb);
        self
    }

    pub fn with_download_dir(mut self, download_dir: impl Into<PathBuf>) -> Self {
        self.download_dir = download_dir.into();
        self
    }

    /// Enable explicit-fence sync using the shared NT handle from the
    /// consumer's `Dx12FenceSynchronizer`. See
    /// [`fence_shared_handle`](Self::fence_shared_handle) for semantics.
    pub fn with_fence_shared_handle(mut self, handle: *mut std::ffi::c_void) -> Self {
        self.fence_shared_handle = Some(handle);
        self
    }
}

/// Captured WebView frame ready to be imported via `wgpu-native-texture-interop`.
///
/// When `resource_is_new` is `true`, this frame points at a freshly allocated
/// shared D3D11 texture that the consumer must (re-)import; the consumer owns
/// the NT handle and is responsible for calling
/// `crate::windows_capture::close_shared_handle` after import.
///
/// When `resource_is_new` is `false`, the producer reused the previous
/// allocation: the consumer should keep its previously-imported `wgpu::Texture`
/// (whose underlying memory was just overwritten by the producer's
/// `CopyResource`) and ignore `shared_handle`.
pub struct WebView2CompositionFrame {
    pub frame: WebSurfaceFrame,
    pub content_size: PhysicalSize<u32>,
    pub generation: u64,
    pub shared_handle: *mut std::ffi::c_void,
    pub resource_is_new: bool,
}

/// Live Windows.Graphics.Capture counters for the WebView2 producer.
///
/// `samples_received` counts WGC frames pulled from the frame pool,
/// `samples_consumed` counts frames emitted to the host, and
/// `stale_frames_dropped` counts frames skipped because their dimensions no
/// longer match the producer after a resize or capture restart.
#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureMetrics {
    pub samples_received: u64,
    pub samples_consumed: u64,
    pub stale_frames_dropped: u64,
}

/// WebView2 + WinComp + WGC capture producer.
///
/// Construction sets up the composition tree and the WebView2 environment.
/// Capture is started lazily on the first `acquire_frame` call so the caller
/// can navigate and prepare content first.
pub struct WebView2CompositionProducer {
    #[allow(dead_code)]
    parent_hwnd: HWND,
    size: PhysicalSize<u32>,
    /// Content-frame counter. Bumps for every emitted capture frame.
    generation: u64,
    /// Shared-resource allocation counter. Bumps only when the persistent
    /// destination texture is allocated or reallocated.
    resource_epoch: u64,

    /// Shared per-HWND composition target. Held so the `DesktopWindowTarget`
    /// outlives every producer attached to it; for a single-pane producer the
    /// `Arc` refcount is 1.
    #[allow(dead_code)]
    composition_root: Arc<setup::CompositionRoot>,
    /// This producer's pane container — a child of the shared root visual.
    /// Carries the pane's offset and size; `set_offset` / `resize` operate on
    /// it so one pane moves without disturbing siblings.
    pane_container: ContainerVisual,
    webview_visual: ContainerVisual,

    #[allow(dead_code)]
    environment: ICoreWebView2Environment,
    #[allow(dead_code)]
    composition_controller: ICoreWebView2CompositionController,
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,

    capture_factory: D3D11SharedTextureFactory,
    capture_device: IDirect3DDevice,
    capture_state: Option<CaptureState>,
    persistent_dest: Option<PersistentDest>,
    capture_samples_received: AtomicU64,
    capture_samples_consumed: AtomicU64,
    capture_stale_frames_dropped: AtomicU64,

    // Persistent event queues drained by `poll_navigation_event` and
    // `poll_web_message`. Handler closures own clones of these `Arc`s and
    // push from the COM thread; consumer code drains from any thread.
    nav_event_queue: Arc<Mutex<VecDeque<NavigationEvent>>>,
    web_message_queue: Arc<Mutex<VecDeque<String>>>,
    cursor_queue: Arc<Mutex<VecDeque<CursorShape>>>,
    pending_cookies: Arc<Mutex<Option<Vec<Cookie>>>>,
    pending_find: Arc<Mutex<Option<Result<WebView2FindResult, String>>>>,
    pending_pdf: Arc<Mutex<Option<Result<Vec<u8>, String>>>>,
    cookie_change_handler: Arc<Mutex<Option<WebView2CookieChangeHandlerFn>>>,
    download_handler: Arc<Mutex<Option<WebView2DownloadHandlerFn>>>,
    auth_handler: Arc<Mutex<Option<WebView2AuthHandlerFn>>>,
    permission_handler: Arc<Mutex<Option<WebView2PermissionHandlerFn>>>,
    download_registry: Arc<Mutex<WebView2DownloadRegistry>>,
    resource_handlers: Arc<Mutex<HashMap<String, UrlSchemeHandlerFn>>>,
    default_context_menus_enabled: Arc<Mutex<bool>>,
    nav_starting_token: i64,
    nav_completed_token: i64,
    source_changed_token: i64,
    title_changed_token: i64,
    new_window_requested_token: i64,
    process_failed_token: i64,
    download_starting_token: i64,
    basic_auth_token: i64,
    permission_requested_token: i64,
    context_menu_requested_token: i64,
    web_message_token: i64,
    web_resource_response_received_token: i64,
    web_resource_requested_token: Option<i64>,
    accelerator_key_pressed_token: i64,
    cursor_changed_token: i64,
}

/// A reusable shared D3D11 destination texture and its NT handle. The handle
/// is exposed exactly once via `WebView2CompositionFrame::shared_handle` (with
/// `resource_is_new = true`); subsequent frames reuse the same texture and
/// signal `resource_is_new = false`.
struct PersistentDest {
    texture: D3D11SharedTexture,
    size: PhysicalSize<u32>,
    handle_handed_off: bool,
}

struct CaptureState {
    #[allow(dead_code)]
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_arrivals: Arc<AtomicU64>,
    frame_arrivals_observed: u64,
    frame_arrived_token: i64,
    first_frame_emitted: bool,
}

impl WebView2CompositionProducer {
    pub fn size(&self) -> PhysicalSize<u32> {
        self.size
    }
}

impl crate::WebSurfaceProducer for WebView2CompositionProducer {
    fn capabilities(&self) -> WebSurfaceCapabilities {
        // Windows can produce a `Dx12SharedTexture` whenever the host's
        // wgpu device is on the DX12 backend; the host context isn't
        // visible from inside the producer, so we report the shape we
        // actually emit (`Dx12SharedTexture` frames) and leave the
        // host-backend match-up to the consumer's import call.
        WebSurfaceCapabilities {
            backend: SystemWebviewBackend::WebView2,
            preferred_mode: WebSurfaceMode::ImportedTexture,
            imported_texture: crate::native_frame::CapabilityStatus::Supported,
            native_child_overlay: crate::native_frame::CapabilityStatus::Supported,
            cpu_snapshot: crate::native_frame::CapabilityStatus::Supported,
            supported_frames: vec![crate::native_frame::NativeFrameKind::Dx12SharedTexture],
            reason: "WebView2 CompositionController visual + Windows.Graphics.Capture + shared D3D11 NT-handle texture imported as Dx12SharedTexture; keyboard/text uses WebView2 CDP Input on the pure visual-hosted path.",
            features: crate::webview2_features_for_producer(),
        }
    }

    fn acquire_frame(&mut self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        let full = self.acquire_full_frame()?;
        Ok(full.frame)
    }

    fn try_acquire_frame(&mut self) -> Result<Option<WebSurfaceFrame>, WebSurfaceError> {
        WebView2CompositionProducer::try_acquire_frame(self).map(|frame| frame.map(|f| f.frame))
    }

    fn restart_capture_after_stall(&mut self) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::force_restart_capture(self);
        Ok(())
    }

    unsafe fn reparent_to_hwnd(
        &mut self,
        parent_hwnd: *mut std::ffi::c_void,
    ) -> Result<(), WebSurfaceError> {
        // Safety: upheld by the trait method's safety contract.
        unsafe { WebView2CompositionProducer::reparent_to_hwnd(self, parent_hwnd) }
    }

    fn load_html(&mut self, html: &str) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::load_html(self, html)
    }

    fn load_url(&mut self, url: &str) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::load_url(self, url)
    }

    fn navigate_to_string(
        &mut self,
        html: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::navigate_to_string(self, html, timeout)
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::resize(self, size)
    }

    fn set_offset(&mut self, x: f32, y: f32) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::set_offset(self, x, y)
    }

    fn navigate_to_url(
        &mut self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::navigate_to_url(self, url, timeout)
    }

    fn send_mouse_input(&mut self, event: MouseInput) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::send_mouse_input(self, event)
    }

    fn move_focus(&mut self, reason: FocusReason) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::move_focus(self, reason)
    }

    fn send_keyboard_input(&mut self, event: KeyboardInput) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::send_keyboard_input(self, event)
    }

    fn poll_navigation_event(&mut self) -> Option<NavigationEvent> {
        WebView2CompositionProducer::poll_navigation_event(self)
    }

    fn post_web_message(&mut self, message: &str) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::post_web_message(self, message)
    }

    fn poll_web_message(&mut self) -> Option<String> {
        WebView2CompositionProducer::poll_web_message(self)
    }

    fn set_cookie(&mut self, cookie: &crate::Cookie) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::set_cookie(self, cookie)
    }

    fn execute_script_with_result(
        &mut self,
        script: &str,
        timeout: std::time::Duration,
    ) -> Result<String, WebSurfaceError> {
        WebView2CompositionProducer::execute_script_with_result(self, script, timeout)
    }

    fn capture_snapshot_png(&mut self) -> Result<Vec<u8>, WebSurfaceError> {
        WebView2CompositionProducer::capture_snapshot_png(self)
    }

    fn reload(&mut self) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::reload(self)
    }

    fn stop(&mut self) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::stop(self)
    }

    fn go_back(&mut self) -> Result<bool, WebSurfaceError> {
        WebView2CompositionProducer::go_back(self)
    }

    fn go_forward(&mut self) -> Result<bool, WebSurfaceError> {
        WebView2CompositionProducer::go_forward(self)
    }

    fn can_go_back(&self) -> bool {
        WebView2CompositionProducer::can_go_back(self)
    }

    fn can_go_forward(&self) -> bool {
        WebView2CompositionProducer::can_go_forward(self)
    }

    fn open_devtools_window(&mut self) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::open_devtools_window(self)
    }

    fn set_visible(&mut self, visible: bool) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::set_visible(self, visible)
    }

    fn apply_settings(
        &mut self,
        settings: &crate::WebSurfaceSettings,
    ) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::apply_settings(self, settings)
    }

    fn poll_cursor_shape(&mut self) -> Option<CursorShape> {
        WebView2CompositionProducer::poll_cursor_shape(self)
    }

    fn send_pointer_input(&mut self, event: crate::PointerInput) -> Result<(), WebSurfaceError> {
        WebView2CompositionProducer::send_pointer_input(self, event)
    }

    fn send_drag_input(&mut self, event: crate::DragInput) -> Result<(), WebSurfaceError> {
        let _ = event;
        Err(WebSurfaceError::Unsupported(
            "WebView2 drag/drop requires the host's OLE IDataObject; use WebView2CompositionProducer::drag_enter, drag_over, drag_leave, and drop_data",
        ))
    }
}

fn execute_script_blocking(webview: &ICoreWebView2, script: String) -> Result<(), WebSurfaceError> {
    let webview = webview.clone();
    ExecuteScriptCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            let script = CoTaskMemPWSTR::from(script.as_str());
            webview
                .ExecuteScript(*script.as_ref().as_pcwstr(), &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(|error_code, _result| error_code),
    )
    .map_err(|error| WebSurfaceError::Platform(format!("ExecuteScript: {error}")))
}

fn add_script_to_execute_on_document_created_blocking(
    webview: &ICoreWebView2,
    script: String,
) -> Result<(), WebSurfaceError> {
    let webview = webview.clone();
    AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            let script = CoTaskMemPWSTR::from(script.as_str());
            webview
                .AddScriptToExecuteOnDocumentCreated(*script.as_ref().as_pcwstr(), &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(|error_code, _script_id| error_code),
    )
    .map_err(|error| {
        WebSurfaceError::Platform(format!("AddScriptToExecuteOnDocumentCreated: {error}"))
    })
}

fn pump_messages_for(duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        unsafe {
            let mut message = MSG::default();
            let mut drained = 0;
            while drained < MAX_MESSAGES_PER_PUMP_SLICE
                && PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool()
            {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
                drained += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn pump_until(timeout: Duration, rx: &mpsc::Receiver<()>) -> Result<(), ()> {
    let deadline = Instant::now() + timeout;
    loop {
        if rx.try_recv().is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(());
        }
        unsafe {
            let mut message = MSG::default();
            let mut drained = 0;
            while drained < MAX_MESSAGES_PER_PUMP_SLICE
                && PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool()
            {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
                drained += 1;
                if rx.try_recv().is_ok() {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn platform<E: std::fmt::Display>(context: &'static str) -> impl FnOnce(E) -> WebSurfaceError {
    move |error| WebSurfaceError::Platform(format!("{context} failed: {error}"))
}

fn stream_to_bytes(stream: &IStream) -> windows::core::Result<Vec<u8>> {
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None)? };
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 8192];
        let mut read = 0u32;
        unsafe {
            stream
                .Read(
                    chunk.as_mut_ptr() as *mut std::ffi::c_void,
                    chunk.len() as u32,
                    Some(&mut read),
                )
                .ok()?;
        }
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read as usize]);
    }
    Ok(bytes)
}

unsafe fn read_pwstr_from<F>(read: F) -> String
where
    F: FnOnce(*mut PWSTR) -> windows::core::Result<()>,
{
    let mut value = PWSTR::null();
    if read(&mut value).is_ok() {
        unsafe { consume_pwstr(value) }
    } else {
        String::new()
    }
}

unsafe fn read_bool_from<F>(read: F) -> bool
where
    F: FnOnce(*mut windows::core::BOOL) -> windows::core::Result<()>,
{
    let mut value = windows::core::BOOL::default();
    read(&mut value).is_ok() && value.as_bool()
}

unsafe fn consume_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

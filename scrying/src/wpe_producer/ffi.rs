//! Hand-written `extern "C"` declarations for WPE-specific symbols in
//! libWPEWebKit-2.0.so. GObject-generic operations (ref/unref, signal
//! connect, type checks, `g_object_new`) use the `glib` crate's low-level
//! `gobject_ffi` re-export. Signatures verified against WPEWebKit 2.52.3
//! headers under /usr/include/wpe-webkit-2.0.

use glib::ffi::GType;
use std::os::raw::{c_char, c_int};

#[repr(C)]
pub struct WPEDisplay {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WPEView {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WPEBuffer {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WPEBufferDMABuf {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WebKitWebView {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WebKitNetworkSession {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WebKitUserContentManager {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WebKitUserScript {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WPEToplevel {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WPEEvent {
    _opaque: [u8; 0],
}

// WPEEventType discriminants — verified against
// /usr/include/wpe-webkit-2.0/wpe-platform/wpe/WPEEvent.h. The C enum
// is zero-based in declaration order.
pub const WPE_EVENT_POINTER_DOWN: i32 = 1;
pub const WPE_EVENT_POINTER_UP: i32 = 2;
pub const WPE_EVENT_POINTER_MOVE: i32 = 3;
/// Documented as part of the enum even though `wpe_event_scroll_new`
/// takes no `WPEEventType` arg (the event implicitly is a scroll).
#[allow(dead_code)]
pub const WPE_EVENT_SCROLL: i32 = 6;
pub const WPE_EVENT_KEYBOARD_KEY_DOWN: i32 = 7;
pub const WPE_EVENT_KEYBOARD_KEY_UP: i32 = 8;
pub const WPE_EVENT_TOUCH_DOWN: i32 = 9;
pub const WPE_EVENT_TOUCH_UP: i32 = 10;
pub const WPE_EVENT_TOUCH_MOVE: i32 = 11;
pub const WPE_EVENT_TOUCH_CANCEL: i32 = 12;

// WPEModifiers bitmask flags — verified against the same header.
// Pointer-button modifier bits exist (1<<5..1<<9) but are not used by
// the MVP, so we don't bind them yet.
pub const WPE_MODIFIER_KEYBOARD_CONTROL: u32 = 1 << 0;
pub const WPE_MODIFIER_KEYBOARD_SHIFT: u32 = 1 << 1;
pub const WPE_MODIFIER_KEYBOARD_ALT: u32 = 1 << 2;
pub const WPE_MODIFIER_KEYBOARD_META: u32 = 1 << 3;
pub const WPE_MODIFIER_KEYBOARD_CAPS_LOCK: u32 = 1 << 4;

// WPEInputSource enum discriminants — verified against
// /usr/include/wpe-webkit-2.0/wpe-platform/wpe/WPEEvent.h. The C enum is
// zero-based in declaration order. Every wpe_event_*_new constructor
// takes a `WPEInputSource source` as its third argument.
pub const WPE_INPUT_SOURCE_MOUSE: i32 = 0;
pub const WPE_INPUT_SOURCE_PEN: i32 = 1;
pub const WPE_INPUT_SOURCE_KEYBOARD: i32 = 2;
pub const WPE_INPUT_SOURCE_TOUCHSCREEN: i32 = 3;

// WebKitUserContentInjectedFrames: first variant in enum.
pub const WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES: i32 = 0;
// WebKitUserScriptInjectionTime: first variant in enum.
pub const WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START: i32 = 0;

unsafe extern "C" {
    // WPEPlatform headless display constructor — the self-owned display the
    // producer renders into (no compositor surface).
    pub fn wpe_display_headless_new() -> *mut WPEDisplay;

    // WPEView frame lifecycle — release a buffer back to the producer once
    // scrying has finished importing it (used in later tasks).
    pub fn wpe_view_buffer_released(view: *mut WPEView, buffer: *mut WPEBuffer);

    // View-side size notification — emits the view's `resized` signal and
    // updates `wpe_view_get_width/height`. On the headless display this is
    // what actually drives the rendered buffer dimensions: the toplevel's
    // `resize` vfunc is a no-op there, so `wpe_toplevel_resize` returning
    // TRUE doesn't propagate to the WebView's render target. Calling this
    // explicitly after the toplevel resize tells WebKit the new size.
    pub fn wpe_view_resized(view: *mut WPEView, width: c_int, height: c_int);

    // Toplevel chain — under WPEPlatform the view's render size is set on
    // its WPEToplevel, not on the view directly.
    pub fn wpe_view_get_toplevel(view: *mut WPEView) -> *mut WPEToplevel;
    pub fn wpe_toplevel_resize(t: *mut WPEToplevel, width: c_int, height: c_int) -> c_int; // gboolean

    // Generic WPEBuffer geometry.
    pub fn wpe_buffer_get_width(buffer: *mut WPEBuffer) -> c_int;
    pub fn wpe_buffer_get_height(buffer: *mut WPEBuffer) -> c_int;

    // WPEBufferDMABuf — the DMABUF-backed frame contract scrying imports
    // through wgpu's Vulkan external-memory path.
    pub fn wpe_buffer_dma_buf_get_type() -> GType;
    pub fn wpe_buffer_dma_buf_get_format(buffer: *mut WPEBufferDMABuf) -> u32;
    pub fn wpe_buffer_dma_buf_get_n_planes(buffer: *mut WPEBufferDMABuf) -> u32;
    pub fn wpe_buffer_dma_buf_get_fd(buffer: *mut WPEBufferDMABuf, plane: u32) -> c_int;
    pub fn wpe_buffer_dma_buf_get_offset(buffer: *mut WPEBufferDMABuf, plane: u32) -> u32;
    pub fn wpe_buffer_dma_buf_get_stride(buffer: *mut WPEBufferDMABuf, plane: u32) -> u32;
    pub fn wpe_buffer_dma_buf_get_modifier(buffer: *mut WPEBufferDMABuf) -> u64;

    // Ephemeral network session — passed as the WebView's `network-session`
    // construct property so an implicit on-disk WebsiteDataStore isn't
    // created (that default store's destructor asserts at process exit).
    pub fn webkit_network_session_new_ephemeral() -> *mut WebKitNetworkSession;

    // WebKitWebView bridge accessors + GType.
    pub fn webkit_web_view_get_display(web_view: *mut WebKitWebView) -> *mut WPEDisplay;
    pub fn webkit_web_view_get_wpe_view(web_view: *mut WebKitWebView) -> *mut WPEView;
    pub fn webkit_web_view_get_type() -> GType;

    // Inline HTML load; both strings are copied by WebKit before returning.
    // `base_uri` may be NULL (treated as "about:blank").
    pub fn webkit_web_view_load_html(
        web_view: *mut WebKitWebView,
        content: *const c_char,
        base_uri: *const c_char,
    );

    pub fn webkit_web_view_load_uri(view: *mut WebKitWebView, uri: *const c_char);

    // Script-message bridge / user-content (4c.5.a).
    pub fn webkit_web_view_get_user_content_manager(
        web_view: *mut WebKitWebView,
    ) -> *mut WebKitUserContentManager;
    pub fn webkit_user_content_manager_register_script_message_handler(
        manager: *mut WebKitUserContentManager,
        name: *const c_char,
        world_name: *const c_char,
    ) -> c_int; // gboolean
    pub fn webkit_user_content_manager_add_script(
        manager: *mut WebKitUserContentManager,
        script: *mut WebKitUserScript,
    );
    pub fn webkit_user_script_new(
        source: *const c_char,
        injected_frames: i32,
        injection_time: i32,
        allow_list: *const *const c_char,
        block_list: *const *const c_char,
    ) -> *mut WebKitUserScript;
    pub fn webkit_web_view_evaluate_javascript(
        web_view: *mut WebKitWebView,
        script: *const c_char,
        length: isize, // -1 for NUL-terminated
        world_name: *const c_char,
        source_uri: *const c_char,
        cancellable: *mut std::ffi::c_void,
        callback: *mut std::ffi::c_void,
        user_data: *mut std::ffi::c_void,
    );

    // --- Input event construction + dispatch (4c.4) ---
    // Signatures verified against
    // /usr/include/wpe-webkit-2.0/wpe-platform/wpe/WPEEvent.h. Every
    // constructor takes a `WPEInputSource source` as its third argument.
    // Argument order matches the C declarations exactly (Rust's C ABI
    // is positional — order is load-bearing, not just types).
    pub fn wpe_event_keyboard_new(
        ty: i32,
        view: *mut WPEView,
        source: i32,
        time: u32,
        modifiers: u32,
        keycode: u32,
        keyval: u32,
    ) -> *mut WPEEvent;
    pub fn wpe_event_pointer_button_new(
        ty: i32,
        view: *mut WPEView,
        source: i32,
        time: u32,
        modifiers: u32,
        button: u32,
        x: f64,
        y: f64,
        press_count: u32,
    ) -> *mut WPEEvent;
    pub fn wpe_event_pointer_move_new(
        ty: i32,
        view: *mut WPEView,
        source: i32,
        time: u32,
        modifiers: u32,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    ) -> *mut WPEEvent;
    pub fn wpe_event_scroll_new(
        view: *mut WPEView,
        source: i32,
        time: u32,
        modifiers: u32,
        dx: f64,
        dy: f64,
        has_precise_deltas: i32,
        is_stop: i32,
        x: f64,
        y: f64,
    ) -> *mut WPEEvent;
    pub fn wpe_event_touch_new(
        ty: i32,
        view: *mut WPEView,
        source: i32,
        time: u32,
        modifiers: u32,
        sequence_id: u32,
        x: f64,
        y: f64,
    ) -> *mut WPEEvent;
    pub fn wpe_view_event(view: *mut WPEView, event: *mut WPEEvent);
}

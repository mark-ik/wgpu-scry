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
pub struct JSCValue {
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
#[repr(C)]
pub struct WebKitCookieManager {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WebKitWebContext {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WebKitURISchemeRequest {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WebKitURISchemeResponse {
    _opaque: [u8; 0],
}
/// Opaque GInputStream — the URI scheme response consumes one. We never
/// touch the body directly from Rust; the stream is constructed from a
/// `glib::Bytes` via `g_memory_input_stream_new_from_bytes` and handed
/// straight to `webkit_uri_scheme_response_new`.
#[repr(C)]
pub struct GInputStream {
    _opaque: [u8; 0],
}

/// Matches the C `WebKitURISchemeRequestCallback` typedef. Fires once
/// per page-side fetch of a custom-scheme URI, on the producer's affine
/// main context. `user_data` is the `Box::into_raw`d trampoline payload;
/// the registration's `GDestroyNotify` releases it when the WebContext
/// is finalized.
pub type WebKitURISchemeRequestCallback = unsafe extern "C" fn(
    request: *mut WebKitURISchemeRequest,
    user_data: *mut std::ffi::c_void,
);

/// Matches the C `GDestroyNotify` typedef. Called once when the
/// WebContext is finalized (i.e. when the producer drops); reclaims the
/// boxed scheme-handler payload.
pub type GDestroyNotify = unsafe extern "C" fn(user_data: *mut std::ffi::c_void);
/// Opaque on the C side too — every consumer either passes it to a
/// `*_finish` FFI (which crashes through `_opaque`) or hands it back to
/// glib through a trampoline. Declaring it as zero-sized is sufficient
/// for the FFI signatures.
#[repr(C)]
pub struct GAsyncResult {
    _opaque: [u8; 0],
}

/// Matches the C `GAsyncReadyCallback` typedef. The `source` arg is the
/// GObject that initiated the async op (in our case the
/// `WebKitCookieManager`); `result` is the per-op `GAsyncResult`
/// the matching `*_finish` FFI consumes; `user_data` is the
/// `Box::into_raw`'d (manager, cell) pointer the trampoline takes back.
pub type GAsyncReadyCallback = unsafe extern "C" fn(
    source: *mut glib::gobject_ffi::GObject,
    result: *mut GAsyncResult,
    user_data: *mut std::ffi::c_void,
);

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
    // JSCValue string extraction — used by the script-message-received::scry
    // signal closure. `jsc_value_to_string` returns a heap-allocated C string
    // the caller must release with `g_free`.
    pub fn jsc_value_to_string(value: *mut JSCValue) -> *mut c_char;
    pub fn jsc_value_is_string(value: *mut JSCValue) -> c_int; // gboolean

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

    // --- Cookie store (4c.5.b) ---
    // Both getters are transfer-none — the WebView owns its network
    // session; the network session owns its cookie manager. We can hand
    // the resulting pointers straight to the cookie ops without ref
    // bookkeeping. Signatures verified against
    // /usr/include/wpe-webkit-2.0/wpe/WebKitWebView.h,
    // /usr/include/wpe-webkit-2.0/wpe/WebKitNetworkSession.h, and
    // /usr/include/wpe-webkit-2.0/wpe/WebKitCookieManager.h.
    pub fn webkit_web_view_get_network_session(
        web_view: *mut WebKitWebView,
    ) -> *mut WebKitNetworkSession;
    pub fn webkit_network_session_get_cookie_manager(
        session: *mut WebKitNetworkSession,
    ) -> *mut WebKitCookieManager;
    pub fn webkit_cookie_manager_get_cookies(
        manager: *mut WebKitCookieManager,
        uri: *const c_char,
        cancellable: *mut std::ffi::c_void, // NULL
        callback: GAsyncReadyCallback,
        user_data: *mut std::ffi::c_void,
    );
    pub fn webkit_cookie_manager_get_cookies_finish(
        manager: *mut WebKitCookieManager,
        result: *mut GAsyncResult,
        error: *mut *mut glib::ffi::GError,
    ) -> *mut glib::ffi::GList; // transfer-full GList<SoupCookie*>
    pub fn webkit_cookie_manager_add_cookie(
        manager: *mut WebKitCookieManager,
        cookie: *mut std::ffi::c_void, // SoupCookie* — opaque from our side
        cancellable: *mut std::ffi::c_void,
        callback: GAsyncReadyCallback,
        user_data: *mut std::ffi::c_void,
    );
    pub fn webkit_cookie_manager_add_cookie_finish(
        manager: *mut WebKitCookieManager,
        result: *mut GAsyncResult,
        error: *mut *mut glib::ffi::GError,
    ) -> c_int; // gboolean
    pub fn webkit_cookie_manager_delete_cookie(
        manager: *mut WebKitCookieManager,
        cookie: *mut std::ffi::c_void,
        cancellable: *mut std::ffi::c_void,
        callback: GAsyncReadyCallback,
        user_data: *mut std::ffi::c_void,
    );
    pub fn webkit_cookie_manager_delete_cookie_finish(
        manager: *mut WebKitCookieManager,
        result: *mut GAsyncResult,
        error: *mut *mut glib::ffi::GError,
    ) -> c_int;

    // --- Custom URL scheme handlers (4c.5.c) ---
    // Signatures verified against
    // /usr/include/wpe-webkit-2.0/wpe/WebKitWebContext.h,
    // /usr/include/wpe-webkit-2.0/wpe/WebKitURISchemeRequest.h, and
    // /usr/include/wpe-webkit-2.0/wpe/WebKitURISchemeResponse.h.
    // `webkit_web_context_new` is transfer-full; we own one ref after the
    // call. Passing the resulting context as the `"web-context"` construct
    // property on the WebView makes the WebView take its own ref; we then
    // release ours, same shape as the headless `display`/`network-session`
    // dance in `build_producer_view`.
    pub fn webkit_web_context_new() -> *mut WebKitWebContext;
    pub fn webkit_web_context_register_uri_scheme(
        context: *mut WebKitWebContext,
        scheme: *const c_char,
        callback: WebKitURISchemeRequestCallback,
        user_data: *mut std::ffi::c_void,
        user_data_destroy_func: Option<GDestroyNotify>,
    );

    // URI scheme request introspection + completion. `get_uri` is
    // transfer-none — the request owns the string.
    pub fn webkit_uri_scheme_request_get_uri(
        request: *mut WebKitURISchemeRequest,
    ) -> *const c_char;
    pub fn webkit_uri_scheme_request_finish_with_response(
        request: *mut WebKitURISchemeRequest,
        response: *mut WebKitURISchemeResponse,
    );

    // URI scheme response construction. `webkit_uri_scheme_response_new`
    // is transfer-full — the WebKit response object takes a ref on the
    // input stream, so the caller can drop its own once construction is
    // done.
    pub fn webkit_uri_scheme_response_new(
        input_stream: *mut GInputStream,
        stream_length: i64,
    ) -> *mut WebKitURISchemeResponse;
    pub fn webkit_uri_scheme_response_set_content_type(
        response: *mut WebKitURISchemeResponse,
        content_type: *const c_char,
    );
    pub fn webkit_uri_scheme_response_set_http_headers(
        response: *mut WebKitURISchemeResponse,
        headers: *mut std::ffi::c_void, // SoupMessageHeaders* — passed in transfer-full
    );

    // GIO memory input stream — backs the URI scheme response body. The
    // returned `GInputStream*` is transfer-full; `webkit_uri_scheme_response_new`
    // takes its own ref, so we release ours after handing it over.
    // Lives in libgio-2.0.so; libwpe-webkit-2.0 already depends on it
    // transitively, so the symbol is reachable at link time.
    pub fn g_memory_input_stream_new_from_bytes(
        bytes: *mut glib::ffi::GBytes,
    ) -> *mut GInputStream;
}

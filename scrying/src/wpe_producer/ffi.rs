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

unsafe extern "C" {
    // WPEPlatform headless display constructor — the self-owned display the
    // producer renders into (no compositor surface).
    pub fn wpe_display_headless_new() -> *mut WPEDisplay;
    // GType of WPEDisplay; used to type the GValue when binding the display
    // construct property on the WebView.
    pub fn wpe_display_get_type() -> GType;

    // WPEView frame lifecycle — release a buffer back to the producer once
    // scrying has finished importing it (used in later tasks).
    pub fn wpe_view_buffer_released(view: *mut WPEView, buffer: *mut WPEBuffer);

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

    // Load an in-memory HTML document into the WebView. Both strings are
    // copied by WebKit before the call returns. `base_uri` may be NULL,
    // in which case "about:blank" is used.
    pub fn webkit_web_view_load_html(
        web_view: *mut WebKitWebView,
        content: *const c_char,
        base_uri: *const c_char,
    );
}

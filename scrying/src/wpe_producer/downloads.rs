//! Download lifecycle bridge: `NetworkSession::download-started` →
//! per-Download signal handlers → `NavigationEvent::DownloadStarted` /
//! `DownloadProgress` / `DownloadFinished` on the producer's nav-event
//! queue.
//!
//! Port of `webkitgtk_producer/downloads.rs`, with two WPE-specific
//! deviations forced by the WebKit 2022 GLib API the WPE 2.0 build
//! uses (`ENABLE_2022_GLIB_API=ON`):
//!
//! 1. **`download-started` lives on `WebKitNetworkSession`, not
//!    `WebKitWebContext`.** Under the 2022 API the signal was moved
//!    off the WebContext (see
//!    `Source/WebKit/UIProcess/API/glib/WebKitWebContext.cpp` —
//!    declaration is gated on `#if !ENABLE(2022_GLIB_API)`). It now
//!    lives on the per-WebView NetworkSession. The producer already
//!    owns an ephemeral NetworkSession (built in `headless.rs`); we
//!    fetch it back via `webkit_web_view_get_network_session` and
//!    connect the signal there.
//!
//! 2. **`webkit_download_set_destination` requires an absolute path,
//!    NOT a `file://` URI.** Under `ENABLE(2022_GLIB_API)` the impl
//!    is `g_return_if_fail(g_path_is_absolute(destination))`; the
//!    older `file://` URI form the GTK precedent uses would trip the
//!    fail-on-not-absolute check. We hand it a plain absolute path.
//!
//! Destination handling: each download lands under
//! `<config.data_dir>/downloads/<suggested_filename>`. The destination
//! is set synchronously inside `on_download_started` (before any bytes
//! flow), matching the GTK precedent — hosts that want a per-download
//! prompt can layer a `set_download_handler` API on top in a future
//! slice.
//!
//! Progress throttling: `received-data` fires at engine refresh rate.
//! `DownloadProgress` events are coalesced to ≤ ~10 Hz per download
//! so the nav-event queue doesn't drown.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use glib::prelude::*;
use glib::translate::ToGlibPtr;

use crate::{DownloadId, NavigationEvent};

use super::ffi;
use super::navigation::NavState;

/// Wire the NetworkSession `download-started` signal to the shared
/// nav-event queue. Called from the producer constructor with the
/// configured downloads directory, the shared `NavState`, and the
/// shared monotonic-id counter.
///
/// `network_session` is a [`glib::Object`] wrapper over the WebView's
/// `WebKitNetworkSession`. The wrapper need only outlive this call —
/// `connect_closure` registers the closure on the GObject itself,
/// which is kept alive by the WebView for the producer's lifetime.
pub(super) fn install(
    network_session: &glib::Object,
    downloads_dir: PathBuf,
    state: Rc<RefCell<NavState>>,
    next_id: Rc<Cell<u64>>,
) {
    let closure = glib::RustClosure::new_local(move |values| {
        // values: [session: GObject, download: WebKitDownload (GObject)]
        let Some(download_value) = values.get(1) else {
            return None;
        };
        let raw_download: *mut ffi::WebKitDownload = match download_value.get::<glib::Object>() {
            Ok(obj) => {
                // The Download GObject is owned by WebKit for the
                // duration of the transfer. The per-Download signal
                // closures we connect below take their own refs on
                // the underlying object (via `connect_closure` on the
                // wrapper we build here), so once `obj` drops at the
                // end of this branch the Download still lives until
                // `finished` / `failed` fires.
                ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&obj).0
                    as *mut ffi::WebKitDownload
            }
            Err(_) => std::ptr::null_mut(),
        };
        if raw_download.is_null() {
            return None;
        }
        on_download_started(raw_download, &downloads_dir, &state, &next_id);
        None
    });
    network_session.connect_closure("download-started", false, closure);
}

fn on_download_started(
    raw_download: *mut ffi::WebKitDownload,
    downloads_dir: &Path,
    state: &Rc<RefCell<NavState>>,
    next_id: &Rc<Cell<u64>>,
) {
    let id_value = {
        let v = next_id.get().saturating_add(1);
        next_id.set(v);
        v
    };
    let id = DownloadId(id_value);

    // SAFETY: raw_download is a borrowed (transfer-none) GObject ptr
    // owned by WebKit for the duration of the transfer.
    let raw_request = unsafe { ffi::webkit_download_get_request(raw_download) };
    let request_uri = if raw_request.is_null() {
        String::new()
    } else {
        let raw_uri = unsafe { ffi::webkit_uri_request_get_uri(raw_request) };
        cstr_to_string(raw_uri)
    };

    let raw_response = unsafe { ffi::webkit_download_get_response(raw_download) };
    let (suggested_filename, total_bytes_expected) = if raw_response.is_null() {
        (format!("download-{id_value}"), None)
    } else {
        let raw_name = unsafe { ffi::webkit_uri_response_get_suggested_filename(raw_response) };
        let name = if raw_name.is_null() {
            format!("download-{id_value}")
        } else {
            cstr_to_string(raw_name)
        };
        let len = unsafe { ffi::webkit_uri_response_get_content_length(raw_response) };
        (name, if len > 0 { Some(len) } else { None })
    };

    let _ = std::fs::create_dir_all(downloads_dir);
    let destination_path = downloads_dir.join(&suggested_filename);
    // WebKit refuses to overwrite an existing file ("File exists");
    // for host-default placement we own the slot — mirror the GTK
    // precedent and clear the stale entry.
    let _ = std::fs::remove_file(&destination_path);

    // WPE 2.52.3 / 2022 GLib API requires an absolute path here, not
    // a file:// URI — see module doc deviation #2.
    if let Some(path_str) = destination_path.to_str() {
        if let Ok(c_path) = std::ffi::CString::new(path_str) {
            // SAFETY: raw_download is a borrowed GObject ptr owned by
            // WebKit; the C string is copied before set_destination
            // returns.
            unsafe { ffi::webkit_download_set_destination(raw_download, c_path.as_ptr()) };
        }
    }

    state
        .borrow_mut()
        .events
        .push_back(NavigationEvent::DownloadStarted {
            id,
            url: request_uri,
            suggested_filename,
            destination_path: destination_path.clone(),
            total_bytes_expected,
        });

    // Wrap the Download as a glib::Object so we can `connect_closure`
    // on it. transfer-none -> from_glib_none adds a ref the wrapper
    // releases on drop. Each connect_closure below installs a
    // marshaller that keeps its own ref on the underlying GObject for
    // as long as the connection lives (i.e. for the Download's
    // lifetime), so the connections survive `download_obj` going out
    // of scope at the end of this function.
    let download_obj: glib::Object = unsafe {
        glib::translate::from_glib_none(raw_download as *mut glib::gobject_ffi::GObject)
    };

    // Per-download progress throttle + cumulative-bytes counter.
    let last_progress = Rc::new(Cell::new(Instant::now() - Duration::from_secs(1)));
    let bytes_written = Rc::new(Cell::new(0u64));

    {
        let lp = last_progress.clone();
        let bw = bytes_written.clone();
        let progress_state = state.clone();
        let total_bytes = total_bytes_expected;
        // `received-data` signature: void (WebKitDownload*, guint64).
        // Verified guint64 against
        // `Source/WebKit/UIProcess/API/glib/WebKitDownload.cpp:381`
        // (`webkitDownloadNotifyProgress(.., guint64)`).
        let closure = glib::RustClosure::new_local(move |values| {
            let Some(v) = values.get(1) else {
                return None;
            };
            // `received-data`'s payload is a guint64 — extract as u64.
            // If marshalling somehow lands a smaller width, fall back
            // to zero (progress just skips a tick).
            let n: u64 = v.get::<u64>().unwrap_or(0);
            bw.set(bw.get().saturating_add(n));
            let now = Instant::now();
            if now.duration_since(lp.get()) >= Duration::from_millis(100) {
                lp.set(now);
                progress_state
                    .borrow_mut()
                    .events
                    .push_back(NavigationEvent::DownloadProgress {
                        id,
                        bytes_written: bw.get(),
                        total_bytes_expected: total_bytes,
                    });
            }
            None
        });
        download_obj.connect_closure("received-data", false, closure);
    }

    {
        let finished_state = state.clone();
        let finished_dest = destination_path.clone();
        let closure = glib::RustClosure::new_local(move |values| {
            // `finished` signature: void (WebKitDownload*). The signal
            // arg[0] is the Download; if we can extract a destination
            // off it use that (it may have been altered by another
            // handler), otherwise fall back to what we set originally.
            let final_dest = values
                .first()
                .and_then(|v| v.get::<glib::Object>().ok())
                .map(|obj| {
                    let raw = ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&obj).0
                        as *mut ffi::WebKitDownload;
                    // SAFETY: raw is the Download ptr from the signal
                    // arg, borrowed for the closure body.
                    let raw_dest = unsafe { ffi::webkit_download_get_destination(raw) };
                    if raw_dest.is_null() {
                        finished_dest.clone()
                    } else {
                        let s = cstr_to_string(raw_dest);
                        destination_uri_to_path(&s)
                    }
                })
                .unwrap_or_else(|| finished_dest.clone());
            finished_state
                .borrow_mut()
                .events
                .push_back(NavigationEvent::DownloadFinished {
                    id,
                    destination_path: final_dest,
                    error: None,
                });
            None
        });
        download_obj.connect_closure("finished", false, closure);
    }

    {
        let failed_state = state.clone();
        let failed_dest = destination_path.clone();
        // `failed` signature: void (WebKitDownload*, GError*). The
        // GError landing in a `glib::Value` round-trips as a
        // `glib::Error` we can read `.message()` off.
        let closure = glib::RustClosure::new_local(move |values| {
            let message = values
                .get(1)
                .and_then(|v| v.get::<glib::Error>().ok())
                .map(|e| e.message().to_string())
                .unwrap_or_else(|| "download failed".to_string());
            failed_state
                .borrow_mut()
                .events
                .push_back(NavigationEvent::DownloadFinished {
                    id,
                    destination_path: failed_dest.clone(),
                    error: Some(message),
                });
            None
        });
        download_obj.connect_closure("failed", false, closure);
    }
}

/// Pull a Rust `String` out of a borrowed C string, returning an
/// empty string for null. Lossy UTF-8 decode — the URIs and
/// filenames WebKit hands us are conventionally valid UTF-8, but
/// `to_string_lossy` is correctness-safe against the off-spec case.
fn cstr_to_string(raw: *const std::os::raw::c_char) -> String {
    if raw.is_null() {
        return String::new();
    }
    // SAFETY: raw is a borrowed (transfer-none) NUL-terminated C
    // string from a WebKit accessor; valid for the duration of the
    // call.
    unsafe { std::ffi::CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned()
}

/// Strip a `file://` prefix off `uri`, returning a [`PathBuf`].
///
/// WPE 2.52.3's `webkit_download_get_destination` under the 2022
/// GLib API returns a plain path (mirroring what we set), so this
/// helper is defensive against a future WPE version reverting to
/// returning a URI — and is what the unit tests exercise.
fn destination_uri_to_path(uri: &str) -> PathBuf {
    uri.strip_prefix("file://")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(uri))
}

use super::producer::WpeProducer;
use crate::WebSurfaceError;

impl WpeProducer {
    /// Kick off a programmatic download of `url`. Returns immediately
    /// once WebKit has accepted the download; the host watches
    /// `NavigationEvent::DownloadStarted` / `DownloadProgress` /
    /// `DownloadFinished` on the nav-event queue for lifecycle
    /// updates.
    ///
    /// Mirrors `WebKitGtkProducer::download_url` — direct port of the
    /// GTK precedent.
    #[cfg(feature = "wpe")]
    pub fn download_url(&self, url: &str) -> Result<(), WebSurfaceError> {
        use glib::translate::ToGlibPtr;
        let raw_view: *mut ffi::WebKitWebView =
            ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&self.handles.webview).0
                as *mut _;
        let c_url = std::ffi::CString::new(url).map_err(|_| {
            WebSurfaceError::Platform("download_url: URL contained interior NUL".into())
        })?;
        // SAFETY: raw_view is borrowed from the owned webview;
        // download_uri copies the URI string before returning. The
        // returned Download pointer is transfer-none (WebKit owns it);
        // we drop our local copy — the `download-started` closure has
        // already wired the per-Download signal handlers.
        let dl = unsafe { ffi::webkit_web_view_download_uri(raw_view, c_url.as_ptr()) };
        if dl.is_null() {
            return Err(WebSurfaceError::Platform(format!(
                "WPE refused to download {url}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_strips_file_uri() {
        assert_eq!(
            destination_uri_to_path("file:///tmp/foo.bin"),
            PathBuf::from("/tmp/foo.bin")
        );
    }

    #[test]
    fn destination_passthrough_for_plain_path() {
        assert_eq!(
            destination_uri_to_path("/tmp/foo.bin"),
            PathBuf::from("/tmp/foo.bin")
        );
    }

    #[test]
    fn destination_passthrough_for_relative_path() {
        assert_eq!(
            destination_uri_to_path("downloads/foo.bin"),
            PathBuf::from("downloads/foo.bin")
        );
    }

    #[test]
    fn destination_handles_empty_string() {
        assert_eq!(destination_uri_to_path(""), PathBuf::from(""));
    }

    #[test]
    fn destination_does_not_strip_https() {
        // Only file:// is stripped; a stray https:// would be passed
        // through verbatim (caller's problem).
        assert_eq!(
            destination_uri_to_path("https://example.com/x"),
            PathBuf::from("https://example.com/x")
        );
    }
}

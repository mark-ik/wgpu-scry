//! Download lifecycle bridge: `NetworkSession::download-started` →
//! per-Download signal handlers → `NavigationEvent::DownloadStarted` /
//! `DownloadProgress` / `DownloadFinished` on the producer's nav-event
//! queue.
//!
//! Port of `webkitgtk_producer/downloads.rs`, with two webkit6-specific
//! deviations forced by the WebKit 2022 GLib API webkit6 uses
//! (`ENABLE_2022_GLIB_API=ON`) — same deviations the WPE 4c.5.f port
//! ran into:
//!
//! 1. **`download-started` lives on `WebKitNetworkSession`, not
//!    `WebKitWebContext`.** Under the 2022 API the signal was moved
//!    off the WebContext (see
//!    `Source/WebKit/UIProcess/API/glib/WebKitWebContext.cpp` —
//!    declaration is gated on `#if !ENABLE(2022_GLIB_API)`). webkit6's
//!    auto-binding reflects this: `connect_download_started` is on
//!    [`webkit6::NetworkSession`] (verified at
//!    `webkit6-0.6.1/src/auto/network_session.rs`); no such method
//!    exists on `WebContext`. The producer already owns the
//!    NetworkSession (`_network_session`) — we connect there.
//!
//! 2. **`webkit_download_set_destination` requires an absolute path,
//!    NOT a `file://` URI.** Under `ENABLE(2022_GLIB_API)` the impl
//!    is `g_return_if_fail(g_path_is_absolute(destination))`; the
//!    older `file://` URI form the GTK 3 precedent uses would trip the
//!    fail-on-not-absolute check. webkit6's auto-binding takes a plain
//!    `&str` (`Download::set_destination(&str)`); we hand it the
//!    absolute path string of the destination `PathBuf`.
//!
//! Cleaner than the WPE port: webkit6's gtk-rs bindings expose
//! typed `connect_download_started` / `connect_received_data` /
//! `connect_finished` / `connect_failed` with native Rust closures
//! (no hand-rolled `RustClosure` / `connect_closure` plumbing — that
//! WPE port needed `RustClosure` because the WPE `Download` GObject
//! isn't auto-bound).
//!
//! Destination handling: each download lands under
//! `<config.data_dir>/downloads/<suggested_filename>`. The destination
//! is set synchronously inside the `download-started` handler (before
//! any bytes flow), matching both prior precedents — hosts that want
//! a per-download prompt can layer a `set_download_handler` API on
//! top in a future slice.
//!
//! Progress throttling: `received-data` fires at engine refresh rate.
//! `DownloadProgress` events are coalesced to ≤ ~10 Hz per download
//! so the nav-event queue doesn't drown.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use webkit6::prelude::*;
use webkit6::{Download, NetworkSession};

use crate::{DownloadId, NavigationEvent, WebSurfaceError};

use super::navigation::NavState;
use super::producer::WebKit6Producer;

/// Wire the NetworkSession `download-started` signal to the shared
/// nav-event queue. Called from the producer constructor with the
/// configured downloads directory, the shared `NavState`, and the
/// shared monotonic-id counter.
pub(crate) fn install(
    network_session: &NetworkSession,
    downloads_dir: PathBuf,
    state: Rc<RefCell<NavState>>,
    next_id: Rc<Cell<u64>>,
) {
    network_session.connect_download_started(move |_session, download| {
        on_download_started(download, &downloads_dir, &state, &next_id);
    });
}

fn on_download_started(
    download: &Download,
    downloads_dir: &PathBuf,
    state: &Rc<RefCell<NavState>>,
    next_id: &Rc<Cell<u64>>,
) {
    let id_value = {
        let v = next_id.get().saturating_add(1);
        next_id.set(v);
        v
    };
    let id = DownloadId(id_value);

    let request_uri = download
        .request()
        .and_then(|r| r.uri())
        .map(|g| g.to_string())
        .unwrap_or_default();

    let response = download.response();
    let suggested_filename = response
        .as_ref()
        .and_then(|r| r.suggested_filename())
        .map(|g| g.to_string())
        .unwrap_or_else(|| format!("download-{id_value}"));
    let total_bytes_expected = response
        .as_ref()
        .map(|r| r.content_length())
        .filter(|n| *n > 0);

    // Ensure the downloads directory exists, then pick a path.
    let _ = std::fs::create_dir_all(downloads_dir);
    let destination_path = downloads_dir.join(&suggested_filename);
    // WebKit refuses to overwrite an existing file with the same
    // destination ("File exists"). For host-default placement, clear
    // the stale entry — the host is opting into our naming policy
    // here, so we own the slot. Hosts that want collision avoidance
    // can wrap this with their own pre-check.
    let _ = std::fs::remove_file(&destination_path);

    // webkit6 / 2022 GLib API requires an absolute path here, not a
    // file:// URI — see module doc deviation #2.
    if let Some(path_str) = destination_path.to_str() {
        download.set_destination(path_str);
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

    // Per-download progress throttle + cumulative-bytes counter.
    let last_progress = Rc::new(Cell::new(Instant::now() - Duration::from_secs(1)));
    let bytes_written = Rc::new(Cell::new(0u64));

    let lp = last_progress.clone();
    let bw = bytes_written.clone();
    let progress_state = state.clone();
    // webkit6's `connect_received_data` closure signature is
    // `Fn(&Download, u64)` — `u64` matches the GTK 3 precedent and
    // the underlying `guint64` payload in `webkitDownloadNotifyProgress`.
    download.connect_received_data(move |_dl, n| {
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
                    total_bytes_expected,
                });
        }
    });

    let finished_state = state.clone();
    let finished_dest = destination_path.clone();
    download.connect_finished(move |dl| {
        // webkit6's `Download::destination()` under the 2022 GLib API
        // returns a plain absolute path (mirroring what we set). The
        // `destination_uri_to_path` helper is defensive against a
        // future binding reverting to a `file://` URI.
        let final_dest = dl
            .destination()
            .map(|g| destination_uri_to_path(&g))
            .unwrap_or_else(|| finished_dest.clone());
        finished_state
            .borrow_mut()
            .events
            .push_back(NavigationEvent::DownloadFinished {
                id,
                destination_path: final_dest,
                error: None,
            });
    });

    let failed_state = state.clone();
    let failed_dest = destination_path.clone();
    download.connect_failed(move |_dl, error| {
        failed_state
            .borrow_mut()
            .events
            .push_back(NavigationEvent::DownloadFinished {
                id,
                destination_path: failed_dest.clone(),
                error: Some(error.message().to_string()),
            });
    });
}

/// Strip a `file://` prefix off `uri`, returning a [`PathBuf`].
///
/// webkit6 / 2022 GLib API's `webkit_download_get_destination`
/// returns a plain absolute path (mirroring what we set), so this
/// helper is defensive against a future binding reverting to
/// returning a URI — and is what the unit tests exercise.
fn destination_uri_to_path(uri: &str) -> PathBuf {
    uri.strip_prefix("file://")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(uri))
}

impl WebKit6Producer {
    /// Kick off a programmatic download of `url`. Returns immediately
    /// once WebKit has accepted the download; the host watches
    /// `NavigationEvent::DownloadStarted` / `DownloadProgress` /
    /// `DownloadFinished` on the nav-event queue for lifecycle
    /// updates.
    ///
    /// Mirrors `WebKitGtkProducer::download_url` — direct port of the
    /// GTK 3 precedent.
    pub fn download_url(&self, url: &str) -> Result<(), WebSurfaceError> {
        match self.webview.download_uri(url) {
            Some(_dl) => Ok(()),
            None => Err(WebSurfaceError::Platform(format!(
                "WebKitGTK 6 refused to download {url}"
            ))),
        }
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

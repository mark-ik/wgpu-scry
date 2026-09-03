// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! `WKDownloadDelegate` and the unique-destination resolver.
//!
//! Tracks every active download in a per-producer
//! [`DownloadRegistry`] so that the four lifecycle events
//! (`DownloadStarted` / `DownloadProgress` / `DownloadFinished` /
//! `DownloadCancelled`) can carry a stable [`crate::DownloadId`] and
//! a real `destination_path`. One delegate instance is shared
//! across every `WKDownload` we receive; WebKit holds delegate
//! references weakly via `WKDownload::setDelegate:`, so the
//! producer keeps the strong ref alive.
//!
//! Also drives the optional host-driven destination policy
//! ([`super::api`]'s `set_download_handler`). With no handler
//! registered, the delegate falls back to the previous default —
//! `<config.download_dir>/<suggested_filename>` with `-N`
//! collision-suffixing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{
    MainThreadMarker, NSData, NSError, NSObject, NSObjectProtocol, NSString, NSURL,
    NSURLAuthenticationChallenge, NSURLCredential, NSURLCredentialPersistence, NSURLResponse,
    NSURLSessionAuthChallengeDisposition,
};
use objc2_web_kit::{WKDownload, WKDownloadDelegate};

use crate::{
    AuthChallenge, AuthDisposition, DownloadDecision, DownloadDestinationRequest, DownloadId,
    NavigationEvent,
};

use super::nav_delegate::{AuthHandlerFn, NavState, read_protection_space};

/// Host-registered destination handler. Called synchronously inside
/// the WKDownload `decideDestination` callback. `None` falls back
/// to the default `unique_destination(config.download_dir, ...)`
/// policy.
pub type DownloadHandlerFn =
    Box<dyn Fn(DownloadDestinationRequest) -> DownloadDecision + Send + Sync + 'static>;

/// Throttling threshold for `DownloadProgress` events: don't emit
/// more than one per 100ms per download.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Throttling threshold for `DownloadProgress` events: emit early
/// (ignoring the time interval) when this many bytes have been
/// written since the last emit, so multi-MB downloads still get
/// timely updates even if `didWriteData:` callback cadence is
/// uneven.
const PROGRESS_MIN_BYTES: u64 = 1_048_576; // 1 MiB

/// Per-download state owned by the [`DownloadRegistry`]. Lives from
/// `decideDestination` (when WebKit first introduces the download)
/// until either `downloadDidFinish:` or
/// `download:didFailWithError:resumeData:` fires, at which point
/// the registry entry is removed and the `Retained<WKDownload>`
/// strong ref drops.
pub(super) struct DownloadEntry {
    pub(super) id: DownloadId,
    pub(super) destination_path: PathBuf,
    /// `Content-Length` from the response that initiated this
    /// download. `None` when the server didn't announce one
    /// (chunked / streamed responses). Captured here at
    /// `decideDestination` time so the final `DownloadProgress`
    /// event emitted in `downloadDidFinish:` can report it
    /// faithfully — pre-fix the field used `last_progress_bytes`
    /// as a stand-in, which gave wrong numbers for throttled
    /// downloads (the final on-disk byte count exceeded the
    /// last-throttled `bytes_written`, so consumers saw
    /// `bytes_written > total_bytes_expected`).
    pub(super) total_bytes_expected: Option<u64>,
    /// Strong ref so `cancel_download(id)` can call
    /// `WKDownload::cancel(_:)` later. WebKit also holds an
    /// internal reference for the duration of the transfer; the
    /// strong ref here is what gives the producer its imperative
    /// handle.
    pub(super) wk_download: Retained<WKDownload>,
    /// Last time we emitted a `DownloadProgress` event, for
    /// throttling.
    pub(super) last_progress_emit: Instant,
    /// Cumulative bytes written reported in the last
    /// `DownloadProgress`. The next event fires when either
    /// `PROGRESS_MIN_INTERVAL` or `PROGRESS_MIN_BYTES` is exceeded.
    pub(super) last_progress_bytes: u64,
    /// `true` once we've decided this download will be reported via
    /// `DownloadCancelled` rather than `DownloadFinished` —
    /// either because the host's destination handler returned
    /// `Cancel`, or because `cancel_download(id)` was called.
    /// Drives the routing in `did_fail`.
    pub(super) cancelled_by_host: bool,
}

/// Lookup tables shared between the producer (which calls
/// `cancel_download`) and the [`DownloadHandler`] delegate (which
/// fires events on Apple's callbacks). The pointer-keyed
/// `by_pointer` index lets `didWriteData` / `didFinish` / `didFail`
/// callbacks look up an entry in O(1) given the `&WKDownload`
/// parameter Apple hands them.
#[derive(Default)]
pub(super) struct DownloadRegistry {
    pub(super) by_id: HashMap<DownloadId, DownloadEntry>,
    /// Map from a `WKDownload` instance pointer (cast as `usize`)
    /// to the `DownloadId` we issued for it. Pointers stay stable
    /// because we hold a strong `Retained<WKDownload>` in the
    /// matching entry.
    pub(super) by_pointer: HashMap<usize, DownloadId>,
}

// SAFETY: `DownloadRegistry` is created and accessed exclusively
// from the main thread (the `DownloadHandler` delegate is
// `MainThreadOnly`; the producer's `cancel_download` /
// `resume_download` methods are gated on `MainThreadMarker::new()`
// before touching the registry). The `Retained<WKDownload>` it
// holds is therefore never sent across threads in practice. We
// claim `Send + Sync` so the registry can sit behind a
// `Arc<Mutex<...>>` (which `clippy::arc_with_non_send_sync`
// otherwise rejects); the `Mutex` doubles as a poison-detection
// barrier without ever resolving cross-thread contention.
unsafe impl Send for DownloadRegistry {}
unsafe impl Sync for DownloadRegistry {}

/// Atomic ID allocator handed to the delegate so each new download
/// gets a unique ID without needing `&mut` access to the producer.
pub(super) struct DownloadIdAllocator(AtomicU64);

impl DownloadIdAllocator {
    pub(super) fn new() -> Self {
        Self(AtomicU64::new(1))
    }
    pub(super) fn next(&self) -> DownloadId {
        DownloadId(self.0.fetch_add(1, Ordering::Relaxed))
    }
}

/// `DownloadHandler`'s ivars. The shared `NavState` is the
/// nav-event FIFO it pushes lifecycle events into; `download_dir`
/// is the default destination root; `registry` is the per-producer
/// download tracking map; `id_allocator` issues fresh
/// `DownloadId`s; `host_handler` is the optional destination
/// policy.
pub(super) struct DownloadHandlerIvars {
    pub(super) state: Arc<Mutex<NavState>>,
    pub(super) download_dir: PathBuf,
    pub(super) registry: Arc<Mutex<DownloadRegistry>>,
    pub(super) id_allocator: Arc<DownloadIdAllocator>,
    pub(super) host_handler: Arc<Mutex<Option<DownloadHandlerFn>>>,
    /// Optional host-driven auth-challenge handler. Shared with
    /// the page-level [`super::nav_delegate::NavDelegate`] so a
    /// single registered handler covers both navigation auth and
    /// download auth — hosts that need download-specific
    /// behavior can branch on the URL inside their handler. With
    /// no handler, downloads default to `PerformDefaultHandling`
    /// (system Keychain / interactive prompts).
    pub(super) auth_handler: Arc<Mutex<Option<AuthHandlerFn>>>,
}

define_class!(
    // SAFETY:
    // - Superclass NSObject has no subclassing requirements.
    // - `DownloadHandler` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = DownloadHandlerIvars]
    pub(super) struct DownloadHandler;

    unsafe impl NSObjectProtocol for DownloadHandler {}

    unsafe impl WKDownloadDelegate for DownloadHandler {
        #[unsafe(method(download:decideDestinationUsingResponse:suggestedFilename:completionHandler:))]
        fn decide_destination(
            &self,
            download: &WKDownload,
            response: &NSURLResponse,
            suggested_filename: &NSString,
            completion_handler: &block2::DynBlock<dyn Fn(*mut NSURL)>,
        ) {
            let suggested = suggested_filename.to_string();
            let url = response
                .URL()
                .and_then(|u| u.absoluteString())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let mime_type = response
                .MIMEType()
                .map(|s| s.to_string())
                .unwrap_or_default();
            // Apple's `expectedContentLength` returns
            // `NSURLResponseUnknownLength` (-1) when the server
            // didn't announce one (chunked / streamed responses).
            // Surface that as `None` rather than a sentinel value.
            let expected_len = response.expectedContentLength();
            let total_bytes_expected = if expected_len < 0 {
                None
            } else {
                Some(expected_len as u64)
            };

            let ivars = self.ivars();
            let id = ivars.id_allocator.next();

            // Consult the host's destination handler. With no
            // handler registered, fall back to the legacy default:
            // `<download_dir>/<suggested_filename>` with collision
            // suffixing.
            let request = DownloadDestinationRequest {
                id,
                url: url.clone(),
                suggested_filename: suggested.clone(),
                mime_type,
                total_bytes_expected,
            };
            let decision = ivars
                .host_handler
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|f| f(request)));
            let (destination, cancelled_by_host) = match decision {
                Some(DownloadDecision::AcceptAt(path)) => (path, false),
                Some(DownloadDecision::Cancel) => {
                    // We still need to register the entry so the
                    // ensuing `download:didFailWithError:` callback
                    // (which Apple fires after we hand back a null
                    // destination) can route to `DownloadCancelled`.
                    // Use an empty placeholder path.
                    (PathBuf::new(), true)
                }
                None => (unique_destination(&ivars.download_dir, &suggested), false),
            };

            // Best-effort `mkdir -p` on the parent dir. Failure
            // surfaces via the eventual `didFailWithError:`
            // callback.
            if let Some(parent) = destination.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            // Take a strong ref so `cancel_download` can call
            // `cancel:` later, and so the `WKDownload` pointer we
            // use as a registry key stays valid.
            // SAFETY: `download` is a live `&WKDownload` from
            // Apple's callback; retaining is the standard way to
            // extend its lifetime past the callback.
            let download_strong: Retained<WKDownload> =
                unsafe { Retained::retain(NonNull::from(download).as_ptr()) }
                    .expect("Retained::retain on WKDownload returned None");
            let pointer_key = Retained::as_ptr(&download_strong) as usize;

            if let Ok(mut registry) = ivars.registry.lock() {
                registry.by_pointer.insert(pointer_key, id);
                registry.by_id.insert(
                    id,
                    DownloadEntry {
                        id,
                        destination_path: destination.clone(),
                        total_bytes_expected,
                        wk_download: download_strong,
                        last_progress_emit: Instant::now(),
                        last_progress_bytes: 0,
                        cancelled_by_host,
                    },
                );
            }

            if !cancelled_by_host && let Ok(mut state) = ivars.state.lock() {
                state.events.push_back(NavigationEvent::DownloadStarted {
                    id,
                    url,
                    suggested_filename: suggested,
                    destination_path: destination,
                    total_bytes_expected,
                });
            }

            // Apple's docs require the handler to be invoked
            // exactly once. Returning a null destination triggers
            // `download:didFailWithError:` with
            // `NSURLErrorCancelled`, which we route to
            // `DownloadCancelled` for `cancelled_by_host == true`
            // entries.
            if cancelled_by_host {
                completion_handler.call((std::ptr::null_mut(),));
            } else {
                let path_ns = NSString::from_str(&destination_path_for_handoff(ivars, id));
                let file_url = NSURL::fileURLWithPath(&path_ns);
                completion_handler.call((Retained::as_ptr(&file_url) as *mut _,));
            }
        }

        /// Cumulative-progress callback. Apple invokes this on
        /// every internal write, so we throttle to ~10 Hz / 1 MiB
        /// per download to keep the nav-event FIFO from
        /// drowning in progress events.
        #[unsafe(method(download:didWriteData:totalBytesWritten:totalBytesExpectedToWrite:))]
        fn did_write_data(
            &self,
            download: &WKDownload,
            _bytes_written_chunk: i64,
            total_bytes_written: i64,
            total_bytes_expected: i64,
        ) {
            let ivars = self.ivars();
            let pointer_key = download as *const WKDownload as usize;
            let bytes_written = total_bytes_written.max(0) as u64;
            let total_bytes_expected = if total_bytes_expected < 0 {
                None
            } else {
                Some(total_bytes_expected as u64)
            };

            let id = {
                let mut registry = match ivars.registry.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let Some(&id) = registry.by_pointer.get(&pointer_key) else {
                    return;
                };
                let Some(entry) = registry.by_id.get_mut(&id) else {
                    return;
                };
                let now = Instant::now();
                let elapsed = now.duration_since(entry.last_progress_emit);
                let delta = bytes_written.saturating_sub(entry.last_progress_bytes);
                if elapsed < PROGRESS_MIN_INTERVAL && delta < PROGRESS_MIN_BYTES {
                    return;
                }
                entry.last_progress_emit = now;
                entry.last_progress_bytes = bytes_written;
                id
            };

            if let Ok(mut state) = ivars.state.lock() {
                state.events.push_back(NavigationEvent::DownloadProgress {
                    id,
                    bytes_written,
                    total_bytes_expected,
                });
            }
        }

        #[unsafe(method(downloadDidFinish:))]
        fn did_finish(&self, download: &WKDownload) {
            let ivars = self.ivars();
            let pointer_key = download as *const WKDownload as usize;
            let Some((id, destination_path, total_bytes)) =
                lookup_and_remove(&ivars.registry, pointer_key)
            else {
                return;
            };
            // Always emit a final `DownloadProgress` matching the
            // file's actual size so consumers tracking byte-counts
            // see a clean "complete" tick before the
            // `DownloadFinished` lands.
            if let Ok(meta) = std::fs::metadata(&destination_path)
                && let Ok(mut state) = ivars.state.lock()
            {
                let bytes_written = meta.len();
                state.events.push_back(NavigationEvent::DownloadProgress {
                    id,
                    bytes_written,
                    total_bytes_expected: total_bytes,
                });
            }
            if let Ok(mut state) = ivars.state.lock() {
                state.events.push_back(NavigationEvent::DownloadFinished {
                    id,
                    destination_path,
                    error: None,
                });
            }
        }

        #[unsafe(method(download:didFailWithError:resumeData:))]
        fn did_fail(&self, download: &WKDownload, error: &NSError, resume_data: Option<&NSData>) {
            let ivars = self.ivars();
            let pointer_key = download as *const WKDownload as usize;

            // Pull the entry out and learn whether the failure was
            // host-driven cancellation (so we can route to
            // `DownloadCancelled` instead of `DownloadFinished`).
            let entry = {
                let mut registry = match ivars.registry.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let Some(&id) = registry.by_pointer.remove(&pointer_key).as_ref() else {
                    return;
                };
                registry.by_id.remove(&id)
            };
            let Some(entry) = entry else { return };

            // Materialize the resume bytes (if any) before the
            // borrow expires. Apple hands us the opaque blob
            // they'd want back through
            // `resumeDownloadFromResumeData:`; we copy the bytes
            // into a `Vec<u8>` so the host can stash them across
            // process restarts if desired.
            let resume_bytes = resume_data.map(|d| d.to_vec()).filter(|v| !v.is_empty());

            // NSURLError = -999 means "user cancelled", which Apple
            // emits both for our `completion(NULL)` from the host
            // Cancel path and for `WKDownload::cancel(_:)` calls.
            // The `cancelled_by_host` bit on the entry (set by
            // either path) is the authoritative signal.
            if entry.cancelled_by_host {
                if let Ok(mut state) = ivars.state.lock() {
                    state.events.push_back(NavigationEvent::DownloadCancelled {
                        id: entry.id,
                        destination_path: entry.destination_path,
                        resume_data: resume_bytes,
                    });
                }
                return;
            }

            let msg = error.localizedDescription().to_string();
            if let Ok(mut state) = ivars.state.lock() {
                state.events.push_back(NavigationEvent::DownloadFinished {
                    id: entry.id,
                    destination_path: entry.destination_path,
                    error: Some(msg),
                });
            }
        }

        /// HTTP basic / digest / server-trust / client-cert auth
        /// challenge fired during a download (separate from the
        /// page-level auth callback). Mirrors
        /// [`super::nav_delegate::NavDelegate`]'s page-level
        /// auth handling: emits [`crate::NavigationEvent::AuthChallenged`]
        /// for logging, then either consults a host-registered
        /// handler (option B) or falls back to
        /// `PerformDefaultHandling` (option A — system Keychain /
        /// interactive prompts).
        ///
        /// Skipping the typed `protectionSpace()` accessor for
        /// the same reason `nav_delegate.rs` does — see
        /// `read_protection_space` for the bypass rationale.
        #[unsafe(method(download:didReceiveAuthenticationChallenge:completionHandler:))]
        fn download_did_receive_auth_challenge(
            &self,
            download: &WKDownload,
            challenge: &NSURLAuthenticationChallenge,
            completion_handler: &block2::DynBlock<
                dyn Fn(NSURLSessionAuthChallengeDisposition, *mut NSURLCredential),
            >,
        ) {
            let ivars = self.ivars();
            let (host, auth_method, realm) = match read_protection_space(challenge) {
                Some(ps) => {
                    let host = ps.host().to_string();
                    let auth_method = ps.authenticationMethod().to_string();
                    let realm = ps.realm().map(|r| r.to_string()).unwrap_or_default();
                    (host, auth_method, realm)
                }
                None => (String::new(), String::new(), String::new()),
            };

            // Extract the URL of the resource being authenticated
            // from `WKDownload::originalRequest` so the event /
            // handler get a stable identifier for correlation
            // with `DownloadStarted` / `DownloadProgress` and for
            // host-side credential lookup. Empty when the download
            // has no original request (defensive fallback for
            // post-promotion redirected flows).
            let url = unsafe {
                download
                    .originalRequest()
                    .and_then(|req| req.URL())
                    .and_then(|nsurl| nsurl.absoluteString())
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            };

            if let Ok(mut state) = ivars.state.lock() {
                state.events.push_back(NavigationEvent::AuthChallenged {
                    url: url.clone(),
                    host: host.clone(),
                    auth_method: auth_method.clone(),
                    source: crate::AuthSource::Download,
                });
            }

            let disposition = if let Ok(guard) = ivars.auth_handler.lock() {
                guard.as_ref().map(|f| {
                    f(AuthChallenge {
                        url,
                        host,
                        auth_method,
                        realm,
                        source: crate::AuthSource::Download,
                    })
                })
            } else {
                None
            };

            match disposition {
                None | Some(AuthDisposition::PerformDefault) => {
                    completion_handler.call((
                        NSURLSessionAuthChallengeDisposition::PerformDefaultHandling,
                        std::ptr::null_mut(),
                    ));
                }
                Some(AuthDisposition::Cancel) => {
                    completion_handler.call((
                        NSURLSessionAuthChallengeDisposition::CancelAuthenticationChallenge,
                        std::ptr::null_mut(),
                    ));
                }
                Some(AuthDisposition::RejectProtectionSpace) => {
                    completion_handler.call((
                        NSURLSessionAuthChallengeDisposition::RejectProtectionSpace,
                        std::ptr::null_mut(),
                    ));
                }
                Some(AuthDisposition::UseCredential { username, password }) => {
                    let user_ns = NSString::from_str(&username);
                    let pass_ns = NSString::from_str(&password);
                    let credential = NSURLCredential::initWithUser_password_persistence(
                        NSURLCredential::alloc(),
                        &user_ns,
                        &pass_ns,
                        NSURLCredentialPersistence::ForSession,
                    );
                    completion_handler.call((
                        NSURLSessionAuthChallengeDisposition::UseCredential,
                        Retained::as_ptr(&credential) as *mut _,
                    ));
                }
            }
        }
    }
);

impl DownloadHandler {
    pub(super) fn new(
        mtm: MainThreadMarker,
        state: Arc<Mutex<NavState>>,
        download_dir: PathBuf,
        registry: Arc<Mutex<DownloadRegistry>>,
        id_allocator: Arc<DownloadIdAllocator>,
        host_handler: Arc<Mutex<Option<DownloadHandlerFn>>>,
        auth_handler: Arc<Mutex<Option<AuthHandlerFn>>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DownloadHandlerIvars {
            state,
            download_dir,
            registry,
            id_allocator,
            host_handler,
            auth_handler,
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// Read the entry's recorded destination path while the registry
/// is locked. Used by `decideDestination` to format the path
/// string for `fileURLWithPath:` without holding the registry
/// lock past the format call.
fn destination_path_for_handoff(ivars: &DownloadHandlerIvars, id: DownloadId) -> String {
    ivars
        .registry
        .lock()
        .ok()
        .and_then(|registry| {
            registry
                .by_id
                .get(&id)
                .map(|entry| entry.destination_path.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// Pull an entry out of the registry by pointer key. Returns
/// `(id, destination_path, total_bytes_expected)` so the caller
/// can construct a final `DownloadProgress` + `DownloadFinished`
/// pair without needing to hold the lock during the emit. The
/// `total_bytes_expected` here is the response's announced
/// `Content-Length` (captured at `decideDestination` time), not a
/// bytes-written running total — so the final `DownloadProgress`
/// event reports the same upper bound consumers saw on
/// `DownloadStarted`.
fn lookup_and_remove(
    registry: &Arc<Mutex<DownloadRegistry>>,
    pointer_key: usize,
) -> Option<(DownloadId, PathBuf, Option<u64>)> {
    let mut registry = registry.lock().ok()?;
    let id = *registry.by_pointer.get(&pointer_key)?;
    registry.by_pointer.remove(&pointer_key);
    let entry = registry.by_id.remove(&id)?;
    Some((entry.id, entry.destination_path, entry.total_bytes_expected))
}

/// Pick a path under `dir` that doesn't already exist. If
/// `<dir>/<name>` exists, try `<dir>/<stem>-1.<ext>`,
/// `<dir>/<stem>-2.<ext>`, etc. Caller is responsible for
/// `create_dir_all(dir)`.
fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem;
    let ext;
    if let Some(dot) = name.rfind('.') {
        stem = &name[..dot];
        ext = &name[dot..]; // includes the leading "."
    } else {
        stem = name;
        ext = "";
    }
    for n in 1..u32::MAX {
        let candidate = dir.join(format!("{stem}-{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Astronomically unlikely; fall back to the original name.
    dir.join(name)
}

// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::*;

#[derive(Default)]
pub(super) struct WebView2DownloadRegistry {
    pub(super) by_id: HashMap<DownloadId, WebView2DownloadEntry>,
}

// SAFETY: WebView2 download callbacks and producer methods are expected to run
// on the same STA thread as the WebView2 controller. The registry sits behind a
// mutex only to share state between COM callback closures and producer methods.
unsafe impl Send for WebView2DownloadRegistry {}
unsafe impl Sync for WebView2DownloadRegistry {}

pub(super) struct WebView2DownloadEntry {
    pub(super) url: String,
    pub(super) destination_path: PathBuf,
    pub(super) total_bytes_expected: Option<u64>,
    pub(super) operation: ICoreWebView2DownloadOperation,
    pub(super) bytes_received_token: i64,
    pub(super) state_changed_token: i64,
    pub(super) last_progress_emit: Instant,
    pub(super) last_progress_bytes: u64,
    pub(super) cancelled_by_host: bool,
}

pub(super) struct DownloadIdAllocator(AtomicU64);

impl DownloadIdAllocator {
    pub(super) fn new() -> Self {
        Self(AtomicU64::new(1))
    }

    fn next(&self) -> DownloadId {
        DownloadId(self.0.fetch_add(1, Ordering::Relaxed))
    }
}

const DOWNLOAD_PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);
const DOWNLOAD_PROGRESS_MIN_BYTES: u64 = 1_048_576;

impl WebView2CompositionProducer {
    pub fn set_download_handler(
        &mut self,
        handler: WebView2DownloadHandlerFn,
    ) -> Result<(), WebSurfaceError> {
        let mut slot = self
            .download_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("download_handler lock poisoned".into()))?;
        *slot = Some(handler);
        Ok(())
    }

    pub fn clear_download_handler(&mut self) -> Result<(), WebSurfaceError> {
        let mut slot = self
            .download_handler
            .lock()
            .map_err(|_| WebSurfaceError::Platform("download_handler lock poisoned".into()))?;
        *slot = None;
        Ok(())
    }

    pub fn cancel_download(&mut self, id: DownloadId) -> Result<(), WebSurfaceError> {
        let operation = {
            let mut registry = self
                .download_registry
                .lock()
                .map_err(|_| WebSurfaceError::Platform("download registry lock poisoned".into()))?;
            let Some(entry) = registry.by_id.get_mut(&id) else {
                return Err(WebSurfaceError::NotReady("unknown WebView2 download id"));
            };
            entry.cancelled_by_host = true;
            entry.operation.clone()
        };
        unsafe { operation.Cancel() }.map_err(platform("DownloadOperation.Cancel"))
    }

    pub fn pause_download(&mut self, id: DownloadId) -> Result<(), WebSurfaceError> {
        let operation = {
            let registry = self
                .download_registry
                .lock()
                .map_err(|_| WebSurfaceError::Platform("download registry lock poisoned".into()))?;
            let Some(entry) = registry.by_id.get(&id) else {
                return Err(WebSurfaceError::NotReady("unknown WebView2 download id"));
            };
            entry.operation.clone()
        };
        unsafe { operation.Pause() }.map_err(platform("DownloadOperation.Pause"))
    }

    pub fn resume_download(&mut self, id: DownloadId) -> Result<bool, WebSurfaceError> {
        let operation = {
            let registry = self
                .download_registry
                .lock()
                .map_err(|_| WebSurfaceError::Platform("download registry lock poisoned".into()))?;
            let Some(entry) = registry.by_id.get(&id) else {
                return Err(WebSurfaceError::NotReady("unknown WebView2 download id"));
            };
            entry.operation.clone()
        };
        let mut can_resume = windows::core::BOOL::default();
        unsafe { operation.CanResume(&mut can_resume) }
            .map_err(platform("DownloadOperation.CanResume"))?;
        if !can_resume.as_bool() {
            return Ok(false);
        }
        unsafe { operation.Resume() }.map_err(platform("DownloadOperation.Resume"))?;
        Ok(true)
    }

    pub fn can_resume_download(&mut self, id: DownloadId) -> Result<bool, WebSurfaceError> {
        let operation = {
            let registry = self
                .download_registry
                .lock()
                .map_err(|_| WebSurfaceError::Platform("download registry lock poisoned".into()))?;
            let Some(entry) = registry.by_id.get(&id) else {
                return Err(WebSurfaceError::NotReady("unknown WebView2 download id"));
            };
            entry.operation.clone()
        };
        let mut can_resume = windows::core::BOOL::default();
        unsafe { operation.CanResume(&mut can_resume) }
            .map_err(platform("DownloadOperation.CanResume"))?;
        Ok(can_resume.as_bool())
    }
}

pub(super) fn register_download_starting_handler(
    webview: &ICoreWebView2,
    nav_queue: Arc<Mutex<VecDeque<NavigationEvent>>>,
    download_dir: PathBuf,
    host_handler: Arc<Mutex<Option<WebView2DownloadHandlerFn>>>,
    registry: Arc<Mutex<WebView2DownloadRegistry>>,
    id_allocator: Arc<DownloadIdAllocator>,
) -> Result<i64, WebSurfaceError> {
    let webview4: ICoreWebView2_4 = webview
        .cast()
        .map_err(platform("webview cast to ICoreWebView2_4"))?;
    let handler = DownloadStartingEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let operation = unsafe { args.DownloadOperation()? };
        let id = id_allocator.next();
        let url =
            unsafe { download_operation_string(&operation, ICoreWebView2DownloadOperation::Uri) };
        let mime_type = unsafe {
            download_operation_string(&operation, ICoreWebView2DownloadOperation::MimeType)
        };
        let total_bytes_expected = unsafe { download_total_bytes(&operation) };
        let suggested_filename = suggested_download_filename(&operation, &args);
        let request = DownloadDestinationRequest {
            id,
            url: url.clone(),
            suggested_filename: suggested_filename.clone(),
            mime_type,
            total_bytes_expected,
        };
        let decision = host_handler
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|handler| handler(request)));
        let (destination_path, cancelled_by_host) = match decision {
            Some(DownloadDecision::AcceptAt(path)) => (path, false),
            Some(DownloadDecision::Cancel) => (PathBuf::new(), true),
            None => (
                unique_destination(&download_dir, &suggested_filename),
                false,
            ),
        };

        if cancelled_by_host {
            unsafe {
                args.SetCancel(true)?;
                args.SetHandled(true)?;
            }
            if let Ok(mut queue) = nav_queue.lock() {
                queue.push_back(NavigationEvent::DownloadCancelled {
                    id,
                    destination_path,
                    resume_data: None,
                });
            }
            return Ok(());
        }

        if let Some(parent) = destination_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let destination = destination_path.to_string_lossy().into_owned();
        let destination_w = CoTaskMemPWSTR::from(destination.as_str());
        unsafe {
            args.SetResultFilePath(*destination_w.as_ref().as_pcwstr())?;
            args.SetHandled(true)?;
        }

        let progress_registry = registry.clone();
        let progress_queue = nav_queue.clone();
        let progress_handler =
            BytesReceivedChangedEventHandler::create(Box::new(move |sender, _| {
                let Some(operation) = sender else {
                    return Ok(());
                };
                let bytes_written = unsafe { download_bytes_received(&operation) }.unwrap_or(0);
                let total_bytes_expected = unsafe { download_total_bytes(&operation) };
                let should_emit = {
                    let mut registry = match progress_registry.lock() {
                        Ok(registry) => registry,
                        Err(_) => return Ok(()),
                    };
                    let Some(entry) = registry.by_id.get_mut(&id) else {
                        return Ok(());
                    };
                    let now = Instant::now();
                    let elapsed = now.duration_since(entry.last_progress_emit);
                    let delta = bytes_written.saturating_sub(entry.last_progress_bytes);
                    if elapsed < DOWNLOAD_PROGRESS_MIN_INTERVAL
                        && delta < DOWNLOAD_PROGRESS_MIN_BYTES
                    {
                        false
                    } else {
                        entry.last_progress_emit = now;
                        entry.last_progress_bytes = bytes_written;
                        true
                    }
                };
                if should_emit && let Ok(mut queue) = progress_queue.lock() {
                    queue.push_back(NavigationEvent::DownloadProgress {
                        id,
                        bytes_written,
                        total_bytes_expected,
                    });
                }
                Ok(())
            }));
        let state_registry = registry.clone();
        let state_queue = nav_queue.clone();
        let state_handler = StateChangedEventHandler::create(Box::new(move |sender, _| {
            let Some(operation) = sender else {
                return Ok(());
            };
            let Some(state) = (unsafe { download_state(&operation) }) else {
                return Ok(());
            };
            if state != COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED
                && state != COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED
            {
                return Ok(());
            }
            let bytes_written = unsafe { download_bytes_received(&operation) }.unwrap_or(0);
            let reason = unsafe { download_interrupt_reason(&operation) }
                .unwrap_or(COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON(0));
            if state == COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED
                && reason == COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_PAUSED
            {
                let total_bytes_expected = state_registry.lock().ok().and_then(|registry| {
                    registry
                        .by_id
                        .get(&id)
                        .and_then(|entry| entry.total_bytes_expected)
                });
                if let Ok(mut queue) = state_queue.lock() {
                    queue.push_back(NavigationEvent::DownloadProgress {
                        id,
                        bytes_written,
                        total_bytes_expected,
                    });
                }
                return Ok(());
            }
            let entry = state_registry
                .lock()
                .ok()
                .and_then(|mut registry| registry.by_id.remove(&id));
            let Some(entry) = entry else { return Ok(()) };
            if let Ok(mut queue) = state_queue.lock() {
                queue.push_back(NavigationEvent::DownloadProgress {
                    id,
                    bytes_written,
                    total_bytes_expected: entry.total_bytes_expected,
                });
                if state == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED {
                    queue.push_back(NavigationEvent::DownloadFinished {
                        id,
                        destination_path: entry.destination_path,
                        error: None,
                    });
                } else {
                    if entry.cancelled_by_host
                        || reason == COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_CANCELED
                    {
                        queue.push_back(NavigationEvent::DownloadCancelled {
                            id,
                            destination_path: entry.destination_path,
                            resume_data: None,
                        });
                    } else {
                        queue.push_back(NavigationEvent::DownloadFinished {
                            id,
                            destination_path: entry.destination_path,
                            error: Some(format!(
                                "WebView2 download interrupted: {}",
                                download_interrupt_reason_label(reason)
                            )),
                        });
                    }
                }
            }
            Ok(())
        }));

        let mut bytes_received_token = 0i64;
        let mut state_changed_token = 0i64;
        unsafe {
            operation.add_BytesReceivedChanged(&progress_handler, &mut bytes_received_token)?;
            operation.add_StateChanged(&state_handler, &mut state_changed_token)?;
        }
        if let Ok(mut registry) = registry.lock() {
            registry.by_id.insert(
                id,
                WebView2DownloadEntry {
                    url: url.clone(),
                    destination_path: destination_path.clone(),
                    total_bytes_expected,
                    operation: operation.clone(),
                    bytes_received_token,
                    state_changed_token,
                    last_progress_emit: Instant::now(),
                    last_progress_bytes: 0,
                    cancelled_by_host: false,
                },
            );
        }
        if let Ok(mut queue) = nav_queue.lock() {
            queue.push_back(NavigationEvent::DownloadStarted {
                id,
                url,
                suggested_filename,
                destination_path,
                total_bytes_expected,
            });
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe { webview4.add_DownloadStarting(&handler, &mut token) }
        .map_err(platform("add_DownloadStarting"))?;
    Ok(token)
}

unsafe fn download_operation_string(
    operation: &ICoreWebView2DownloadOperation,
    read: unsafe fn(&ICoreWebView2DownloadOperation, *mut PWSTR) -> windows::core::Result<()>,
) -> String {
    let mut value = PWSTR::null();
    if unsafe { read(operation, &mut value) }.is_ok() {
        unsafe { consume_pwstr(value) }
    } else {
        String::new()
    }
}

unsafe fn download_total_bytes(operation: &ICoreWebView2DownloadOperation) -> Option<u64> {
    let mut total = -1i64;
    if unsafe { operation.TotalBytesToReceive(&mut total) }.is_ok() && total >= 0 {
        Some(total as u64)
    } else {
        None
    }
}

unsafe fn download_bytes_received(operation: &ICoreWebView2DownloadOperation) -> Option<u64> {
    let mut bytes = 0i64;
    if unsafe { operation.BytesReceived(&mut bytes) }.is_ok() {
        Some(bytes.max(0) as u64)
    } else {
        None
    }
}

unsafe fn download_state(
    operation: &ICoreWebView2DownloadOperation,
) -> Option<COREWEBVIEW2_DOWNLOAD_STATE> {
    let mut state = COREWEBVIEW2_DOWNLOAD_STATE(0);
    if unsafe { operation.State(&mut state) }.is_ok() {
        Some(state)
    } else {
        None
    }
}

unsafe fn download_interrupt_reason(
    operation: &ICoreWebView2DownloadOperation,
) -> Option<COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON> {
    let mut reason = COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON(0);
    if unsafe { operation.InterruptReason(&mut reason) }.is_ok() {
        Some(reason)
    } else {
        None
    }
}

fn suggested_download_filename(
    operation: &ICoreWebView2DownloadOperation,
    args: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2DownloadStartingEventArgs,
) -> String {
    let mut path = PWSTR::null();
    let result_path = if unsafe { args.ResultFilePath(&mut path) }.is_ok() {
        unsafe { consume_pwstr(path) }
    } else {
        String::new()
    };
    if let Some(name) = Path::new(&result_path).file_name().and_then(|n| n.to_str())
        && !name.is_empty()
    {
        return sanitize_download_filename(name);
    }

    let url = unsafe { download_operation_string(operation, ICoreWebView2DownloadOperation::Uri) };
    if let Some(name) = url
        .split(['?', '#'])
        .next()
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
    {
        return sanitize_download_filename(name);
    }

    "download.bin".to_string()
}

fn sanitize_download_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();
    let trimmed = sanitized.trim_matches([' ', '.']);
    if trimmed.is_empty() {
        "download.bin".to_string()
    } else {
        trimmed.to_string()
    }
}

fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let name = sanitize_download_filename(name);
    let candidate = dir.join(&name);
    if !candidate.exists() {
        return candidate;
    }
    let stem;
    let ext;
    if let Some(dot) = name.rfind('.') {
        stem = &name[..dot];
        ext = &name[dot..];
    } else {
        stem = name.as_str();
        ext = "";
    }
    for n in 1..u32::MAX {
        let candidate = dir.join(format!("{stem}-{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(name)
}

fn download_interrupt_reason_label(reason: COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON) -> String {
    match reason {
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_CANCELED => "user canceled".to_string(),
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_PAUSED => "user paused".to_string(),
        other => format!("reason {}", other.0),
    }
}

pub(super) fn auth_source_for_webview2_basic_auth(
    url: &str,
    download_registry: &Arc<Mutex<WebView2DownloadRegistry>>,
) -> AuthSource {
    let normalized = url.split('#').next().unwrap_or(url);
    let is_download = download_registry
        .lock()
        .ok()
        .map(|registry| {
            registry.by_id.values().any(|entry| {
                let entry_url = entry.url.split('#').next().unwrap_or(&entry.url);
                entry_url == normalized
            })
        })
        .unwrap_or(false);
    if is_download {
        AuthSource::Download
    } else {
        AuthSource::Page
    }
}

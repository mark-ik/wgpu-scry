// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use scrying::UrlSchemeResponse;

pub(crate) fn validate_platform_virtual_host_routing(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    const HOST: &str = "scrying-test.local";
    producer.register_virtual_host_handler(
        HOST,
        Arc::new(|url: &str| -> UrlSchemeResponse {
            let body = format!(
                r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>Scrying Routing Test</title></head>
<body>
    <h1>routing-test</h1>
    <script>
        window.chrome.webview.postMessage("routing-test:ready:{url}");
    </script>
</body>
</html>"#
            );
            UrlSchemeResponse {
                mime_type: "text/html".into(),
                body: body.into_bytes(),
                headers: Vec::new(),
            }
        }),
    )?;

    drain_navigation_events(producer);
    drain_web_messages(producer);
    let url = format!("https://{HOST}/app-shell");
    producer.navigate_to_url(&url, std::time::Duration::from_secs(5))?;
    wait_for_web_message(
        producer,
        &format!("routing-test:ready:{url}"),
        std::time::Duration::from_secs(3),
    )?;
    println!(
        "demo-win: routing-test: PASS - WebResourceRequested virtual host served app-owned content"
    );
    Ok(())
}

pub(crate) fn validate_platform_process_failure_recovery(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    drain_navigation_events(producer);
    drain_web_messages(producer);
    producer.call_devtools_protocol_method("Page.crash", "{}")?;
    wait_for_content_process_terminated(producer, std::time::Duration::from_secs(5))?;
    producer.navigate_to_string(process_recovery_html(), std::time::Duration::from_secs(5))?;
    wait_for_web_message(
        producer,
        "process-test:recovered",
        std::time::Duration::from_secs(3),
    )?;
    println!(
        "demo-win: process-test: PASS - ProcessFailed surfaced and producer recovered with a fresh navigation"
    );
    Ok(())
}

pub(crate) fn validate_platform_downloads(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    const HOST: &str = "scrying-download.local";
    const FILE_NAME: &str = "scrying-win-download.txt";
    const BODY: &[u8] = b"scrying windows download smoke\n";
    let destination = std::env::temp_dir().join(format!(
        "scrying-demo-win-download-{}-{FILE_NAME}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&destination);
    let destination_for_handler = destination.clone();
    producer.set_download_handler(Box::new(move |request| {
        if request.suggested_filename.ends_with(FILE_NAME) {
            scrying::DownloadDecision::AcceptAt(destination_for_handler.clone())
        } else {
            scrying::DownloadDecision::Cancel
        }
    }))?;
    producer.register_virtual_host_handler(
        HOST,
        Arc::new(|_url: &str| -> UrlSchemeResponse {
            UrlSchemeResponse {
                mime_type: "text/plain".into(),
                body: BODY.to_vec(),
                headers: Vec::new(),
            }
            .with_header(
                "Content-Disposition",
                format!("attachment; filename=\"{FILE_NAME}\""),
            )
        }),
    )?;

    drain_navigation_events(producer);
    drain_web_messages(producer);
    producer.load_url(&format!("https://{HOST}/{FILE_NAME}"))?;
    let completed_path = wait_for_download_finished(producer, std::time::Duration::from_secs(8))?;
    if completed_path != destination {
        return Err(format!(
            "download-test: expected destination {:?}, got {:?}",
            destination, completed_path
        )
        .into());
    }
    let bytes = std::fs::read(&destination)?;
    if bytes != BODY {
        return Err(format!(
            "download-test: downloaded bytes mismatch: expected {} bytes, got {}",
            BODY.len(),
            bytes.len()
        )
        .into());
    }
    let _ = std::fs::remove_file(&destination);
    producer.clear_download_handler()?;
    println!(
        "demo-win: download-test: PASS - DownloadStarting destination and completion events verified"
    );
    Ok(())
}

pub(crate) fn validate_platform_basic_auth(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    let (url, server) = spawn_basic_auth_server()?;
    let observed = Arc::new(AtomicBool::new(false));
    let observed_for_handler = observed.clone();
    producer.set_auth_handler(Box::new(move |challenge| {
        if challenge.host.starts_with("127.0.0.1:") {
            observed_for_handler.store(true, Ordering::SeqCst);
            scrying::AuthDisposition::UseCredential {
                username: "user".into(),
                password: "pass".into(),
            }
        } else {
            scrying::AuthDisposition::PerformDefault
        }
    }))?;

    drain_navigation_events(producer);
    drain_web_messages(producer);
    producer.navigate_to_url(&url, std::time::Duration::from_secs(8))?;
    wait_for_web_message(
        producer,
        "auth-test:ready",
        std::time::Duration::from_secs(3),
    )?;
    if !observed.load(Ordering::SeqCst) {
        return Err("auth-test: WebView2 did not invoke the basic-auth handler".into());
    }
    producer.clear_auth_handler()?;
    match server.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(format!("auth-test server failed: {error}").into()),
        Err(_) => return Err("auth-test server thread panicked".into()),
    }
    println!("demo-win: auth-test: PASS - BasicAuthenticationRequested used host credentials");
    Ok(())
}

pub(crate) fn validate_platform_permissions(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    const HOST: &str = "scrying-permission.local";
    let observed = Arc::new(AtomicBool::new(false));
    let observed_for_handler = observed.clone();
    producer.set_permission_handler(Box::new(move |request| {
        if request.kind == scrying::PermissionKind::Microphone {
            observed_for_handler.store(true, Ordering::SeqCst);
            scrying::PermissionDecision::Deny
        } else {
            scrying::PermissionDecision::Prompt
        }
    }))?;
    producer.register_virtual_host_handler(
        HOST,
        Arc::new(|_url: &str| -> UrlSchemeResponse {
            UrlSchemeResponse {
                mime_type: "text/html".into(),
                body: permission_test_html().as_bytes().to_vec(),
                headers: Vec::new(),
            }
        }),
    )?;

    drain_navigation_events(producer);
    drain_web_messages(producer);
    producer.navigate_to_url(
        &format!("https://{HOST}/permission"),
        std::time::Duration::from_secs(5),
    )?;
    let message = wait_for_web_message_prefix(
        producer,
        "permission-test:",
        std::time::Duration::from_secs(5),
    )?;
    if !observed.load(Ordering::SeqCst) {
        return Err(format!(
            "permission-test: permission handler was not invoked; page reported {message:?}"
        )
        .into());
    }
    if message != "permission-test:denied" {
        return Err(format!("permission-test: expected denied message, got {message:?}").into());
    }
    producer.clear_permission_handler()?;
    println!("demo-win: permission-test: PASS - PermissionRequested mapped to host denial");
    Ok(())
}

pub(crate) fn validate_platform_visibility(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    drain_navigation_events(producer);
    drain_web_messages(producer);
    producer.navigate_to_string(visibility_test_html(), std::time::Duration::from_secs(5))?;
    wait_for_web_message(
        producer,
        "visibility-test:ready:visible",
        std::time::Duration::from_secs(3),
    )?;
    producer.set_visible(false)?;
    wait_for_web_message(
        producer,
        "visibility-test:state:hidden",
        std::time::Duration::from_secs(3),
    )?;
    producer.set_visible(true)?;
    wait_for_web_message(
        producer,
        "visibility-test:state:visible",
        std::time::Duration::from_secs(3),
    )?;
    println!("demo-win: visibility-test: PASS - SetIsVisible reached Page Visibility state");
    Ok(())
}

fn process_recovery_html() -> &'static str {
    r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>Scrying Process Recovery Test</title></head>
<body>
    <h1>process recovery</h1>
    <script>window.chrome.webview.postMessage("process-test:recovered");</script>
</body>
</html>"#
}

fn permission_test_html() -> &'static str {
    r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>Scrying Permission Test</title></head>
<body>
    <h1>permission test</h1>
    <script>
        const post = value => window.chrome.webview.postMessage(value);
        (async () => {
            try {
                await navigator.mediaDevices.getUserMedia({ audio: true });
                post("permission-test:granted");
            } catch (error) {
                if (error && error.name === "NotAllowedError") {
                    post("permission-test:denied");
                } else {
                    post("permission-test:error:" + (error && error.name ? error.name : "unknown"));
                }
            }
        })();
    </script>
</body>
</html>"#
}

fn visibility_test_html() -> &'static str {
    r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>Scrying Visibility Test</title></head>
<body>
    <h1>visibility test</h1>
    <script>
        const post = value => window.chrome.webview.postMessage(value);
        const report = prefix => post(prefix + ":" + document.visibilityState);
        document.addEventListener("visibilitychange", () => report("visibility-test:state"));
        report("visibility-test:ready");
    </script>
</body>
</html>"#
}

fn wait_for_content_process_terminated(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_event = String::new();
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        while let Some(event) = producer.poll_navigation_event() {
            match event {
                NavigationEvent::ContentProcessTerminated => return Ok(()),
                other => last_event = format!("{other:?}"),
            }
        }
    }

    Err(format!(
        "timed out waiting for ContentProcessTerminated; last navigation event {last_event:?}"
    )
    .into())
}

fn wait_for_download_finished(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    timeout: std::time::Duration,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    let mut started = None;
    let mut last_event = String::new();
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        while let Some(event) = producer.poll_navigation_event() {
            match event {
                NavigationEvent::DownloadStarted {
                    id,
                    destination_path,
                    ..
                } => {
                    started = Some((id, destination_path));
                }
                NavigationEvent::DownloadFinished {
                    id,
                    destination_path,
                    error,
                } => {
                    if let Some(error) = error {
                        return Err(
                            format!("download-test: download {id:?} failed with {error}").into(),
                        );
                    }
                    return Ok(destination_path);
                }
                NavigationEvent::DownloadCancelled { id, .. } => {
                    return Err(format!("download-test: download {id:?} was cancelled").into());
                }
                other => last_event = format!("{other:?}"),
            }
        }
    }

    Err(format!(
        "timed out waiting for DownloadFinished; started={started:?}; last navigation event {last_event:?}"
    )
    .into())
}

fn spawn_basic_auth_server()
-> Result<(String, std::thread::JoinHandle<Result<(), String>>), Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(false)?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}/secure");
    let handle = std::thread::spawn(move || -> Result<(), String> {
        listener
            .set_nonblocking(false)
            .map_err(|error| error.to_string())?;
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .map_err(|error| error.to_string())?;
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(3)))
                .map_err(|error| error.to_string())?;
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = std::io::Read::read(&mut stream, &mut buffer)
                    .map_err(|error| error.to_string())?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if request.len() > 16 * 1024 {
                    return Err("request headers exceeded 16 KiB".into());
                }
            }
            let request = String::from_utf8_lossy(&request);
            if request.contains("Authorization: Basic dXNlcjpwYXNz") {
                let body = r#"<!doctype html><script>window.chrome.webview.postMessage("auth-test:ready");</script>"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                std::io::Write::write_all(&mut stream, response.as_bytes())
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            let response = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"scrying\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            std::io::Write::write_all(&mut stream, response.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        Err("basic-auth server did not receive authorized retry".into())
    });
    Ok((url, handle))
}

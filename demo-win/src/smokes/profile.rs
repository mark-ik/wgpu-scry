// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use super::super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use scrying::Cookie;

pub(crate) fn validate_platform_profile_store(
    mut producer: scrying::PlatformWebSurfaceProducer,
    parent_hwnd: *mut std::ffi::c_void,
    user_data_dir: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let cookie_name = format!("demo_win_profile_cookie_{}", std::process::id());
    let cookie = Cookie {
        name: cookie_name.clone(),
        value: "shared-profile".into(),
        domain: "example.com".into(),
        path: "/".into(),
        expires_at: Some(4_102_444_800.0),
        is_secure: false,
        is_http_only: false,
        same_site: None,
        partitioned: false,
    };

    let _ = producer.delete_cookie(&cookie.name, &cookie.domain, &cookie.path);
    producer.set_cookie(&cookie)?;
    pump_windows_messages_for(std::time::Duration::from_millis(250));

    let primary_cookies = request_cookies(&mut producer, std::time::Duration::from_secs(3))?;
    require_cookie(
        &primary_cookies,
        &cookie,
        "profile-test: primary producer did not report the cookie after set_cookie",
    )?;
    drop(producer);
    pump_windows_messages_for(std::time::Duration::from_millis(250));

    let secondary_config = scrying::PlatformWebSurfaceConfig::new(
        winit::dpi::PhysicalSize::new(SMOKE_PROBE_WIDTH as u32, SMOKE_PROBE_HEIGHT as u32),
        user_data_dir,
    )
    .with_offset(SMOKE_PROBE_X + SMOKE_PROBE_WIDTH + 24.0, SMOKE_PROBE_Y)
    .with_diagnostic_backdrop((67, 61, 89));
    let mut secondary =
        unsafe { scrying::PlatformWebSurfaceProducer::new(parent_hwnd, secondary_config)? };
    secondary.navigate_to_string(
        &browser_test_html("profile-secondary"),
        std::time::Duration::from_secs(5),
    )?;

    let secondary_cookies = request_cookies(&mut secondary, std::time::Duration::from_secs(3))?;
    require_cookie(
        &secondary_cookies,
        &cookie,
        "profile-test: secondary producer with the same user_data_dir did not see the cookie",
    )?;

    secondary.delete_cookie(&cookie.name, &cookie.domain, &cookie.path)?;
    pump_windows_messages_for(std::time::Duration::from_millis(250));
    let secondary_after_delete =
        request_cookies(&mut secondary, std::time::Duration::from_secs(3))?;
    if contains_cookie(&secondary_after_delete, &cookie) {
        return Err(format!(
            "profile-test: secondary producer still saw {:?} after delete_cookie",
            cookie.name
        )
        .into());
    }

    println!(
        "demo-win: profile-test: PASS - persistent cookie store survived producer recreation with the same user_data_dir"
    );
    Ok(())
}

pub(crate) fn validate_platform_incognito_store(
    mut incognito: scrying::PlatformWebSurfaceProducer,
    parent_hwnd: *mut std::ffi::c_void,
    user_data_dir: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let cookie_name = format!("demo_win_incognito_cookie_{}", std::process::id());
    let cookie = Cookie {
        name: cookie_name.clone(),
        value: "incognito-only".into(),
        domain: "example.com".into(),
        path: "/".into(),
        expires_at: Some(4_102_444_800.0),
        is_secure: false,
        is_http_only: false,
        same_site: None,
        partitioned: false,
    };

    let _ = incognito.delete_cookie(&cookie.name, &cookie.domain, &cookie.path);
    incognito.set_cookie(&cookie)?;
    pump_windows_messages_for(std::time::Duration::from_millis(250));

    let incognito_cookies = request_cookies(&mut incognito, std::time::Duration::from_secs(3))?;
    require_cookie(
        &incognito_cookies,
        &cookie,
        "incognito-test: InPrivate producer did not report the cookie after set_cookie",
    )?;
    drop(incognito);
    pump_windows_messages_for(std::time::Duration::from_millis(250));

    let persistent_config = scrying::PlatformWebSurfaceConfig::new(
        winit::dpi::PhysicalSize::new(SMOKE_PROBE_WIDTH as u32, SMOKE_PROBE_HEIGHT as u32),
        user_data_dir,
    )
    .with_offset(SMOKE_PROBE_X + SMOKE_PROBE_WIDTH + 24.0, SMOKE_PROBE_Y)
    .with_diagnostic_backdrop((59, 92, 72));
    let mut persistent =
        unsafe { scrying::PlatformWebSurfaceProducer::new(parent_hwnd, persistent_config)? };
    persistent.navigate_to_string(
        &browser_test_html("incognito-persistent"),
        std::time::Duration::from_secs(5),
    )?;

    let persistent_cookies = request_cookies(&mut persistent, std::time::Duration::from_secs(3))?;
    if contains_cookie(&persistent_cookies, &cookie) {
        return Err(format!(
            "incognito-test: persistent producer saw InPrivate cookie {:?}",
            cookie.name
        )
        .into());
    }

    println!(
        "demo-win: incognito-test: PASS - InPrivate cookie stayed isolated from the persistent user_data_dir profile"
    );
    Ok(())
}

pub(crate) fn validate_platform_cookie_store(
    producer: &mut scrying::PlatformWebSurfaceProducer,
) -> Result<(), Box<dyn std::error::Error>> {
    let cookie_name = format!("demo_win_cookie_{}", std::process::id());
    let cookie = Cookie {
        name: cookie_name.clone(),
        value: "cookie-test".into(),
        domain: "example.com".into(),
        path: "/".into(),
        expires_at: None,
        is_secure: false,
        is_http_only: true,
        same_site: None,
        partitioned: false,
    };

    let _ = producer.delete_cookie(&cookie.name, &cookie.domain, &cookie.path);
    producer.set_cookie(&cookie)?;
    pump_windows_messages_for(std::time::Duration::from_millis(250));

    let cookies = request_cookies(producer, std::time::Duration::from_secs(3))?;
    let observed = cookies
        .iter()
        .find(|candidate| cookie_identity_matches(candidate, &cookie))
        .ok_or_else(|| {
            format!(
                "cookie-test: cookie {:?} was not visible after set_cookie ({} cookies observed)",
                cookie.name,
                cookies.len()
            )
        })?;

    if observed.value != cookie.value {
        return Err(format!(
            "cookie-test: cookie value mismatch: expected {:?}, got {:?}",
            cookie.value, observed.value
        )
        .into());
    }
    if !observed.is_http_only {
        return Err("cookie-test: HttpOnly flag did not round-trip".into());
    }

    producer.delete_cookie(&cookie.name, &cookie.domain, &cookie.path)?;
    pump_windows_messages_for(std::time::Duration::from_millis(250));
    let cookies_after_delete = request_cookies(producer, std::time::Duration::from_secs(3))?;
    let still_present = contains_cookie(&cookies_after_delete, &cookie);
    if still_present {
        return Err(format!(
            "cookie-test: cookie {:?} was still visible after delete_cookie",
            cookie.name
        )
        .into());
    }

    let set_cookie_pulses = Arc::new(AtomicUsize::new(0));
    let pulses_for_handler = set_cookie_pulses.clone();
    producer.set_cookie_change_handler(Box::new(move || {
        pulses_for_handler.fetch_add(1, Ordering::SeqCst);
    }))?;
    let (set_cookie_url, set_cookie_server) = spawn_set_cookie_server()?;
    producer.navigate_to_url(&set_cookie_url, std::time::Duration::from_secs(5))?;
    wait_for_web_message(
        producer,
        "cookie-test:set-cookie-ready",
        std::time::Duration::from_secs(3),
    )?;
    wait_for_cookie_change_pulse(&set_cookie_pulses, 1, std::time::Duration::from_secs(3))?;
    producer.clear_cookie_change_handler()?;
    set_cookie_server
        .join()
        .map_err(|_| "set-cookie server thread panicked")?
        .map_err(|error| format!("set-cookie server failed: {error}"))?;

    println!(
        "demo-win: cookie-test: PASS - set/read/delete and Set-Cookie observation verified for {}",
        cookie.name
    );
    Ok(())
}

fn require_cookie(
    cookies: &[Cookie],
    expected: &Cookie,
    context: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let observed = cookies
        .iter()
        .find(|candidate| cookie_identity_matches(candidate, expected))
        .ok_or_else(|| format!("{context} ({} cookies observed)", cookies.len()))?;
    if observed.value != expected.value {
        return Err(format!(
            "{context}: cookie value mismatch: expected {:?}, got {:?}",
            expected.value, observed.value
        )
        .into());
    }
    Ok(())
}

fn contains_cookie(cookies: &[Cookie], expected: &Cookie) -> bool {
    cookies
        .iter()
        .any(|candidate| cookie_identity_matches(candidate, expected))
}

fn cookie_identity_matches(candidate: &Cookie, expected: &Cookie) -> bool {
    candidate.name == expected.name
        && candidate.domain.trim_start_matches('.') == expected.domain
        && candidate.path == expected.path
}

fn spawn_set_cookie_server()
-> Result<(String, std::thread::JoinHandle<Result<(), String>>), Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(false)?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}/set-cookie");
    let handle = std::thread::spawn(move || -> Result<(), String> {
        let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .map_err(|error| error.to_string())?;
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let read =
                std::io::Read::read(&mut stream, &mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let body = r#"<!doctype html><script>window.chrome.webview.postMessage("cookie-test:set-cookie-ready");</script>"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: scrying_set_cookie_observed=1; Path=/; Max-Age=60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        std::io::Write::write_all(&mut stream, response.as_bytes())
            .map_err(|error| error.to_string())?;
        Ok(())
    });
    Ok((url, handle))
}

fn wait_for_cookie_change_pulse(
    counter: &AtomicUsize,
    expected_at_least: usize,
    timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        if counter.load(Ordering::SeqCst) >= expected_at_least {
            return Ok(());
        }
    }
    Err(format!(
        "timed out waiting for cookie-change pulse; observed {} expected at least {expected_at_least}",
        counter.load(Ordering::SeqCst)
    )
    .into())
}

fn request_cookies(
    producer: &mut scrying::PlatformWebSurfaceProducer,
    timeout: std::time::Duration,
) -> Result<Vec<Cookie>, Box<dyn std::error::Error>> {
    while producer.poll_cookies().is_some() {}
    producer.request_all_cookies()?;

    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        pump_windows_messages_for(std::time::Duration::from_millis(16));
        if let Some(cookies) = producer.poll_cookies() {
            return Ok(cookies);
        }
    }

    Err(format!("timed out waiting {timeout:?} for cookie query result").into())
}

// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Minimal Linux runtime probe for scrying's WPEWebKit / WPEPlatform
//! headless producer (Phase 4c.6).
//!
//! Sibling to [`demo-linux`] (WebKitGTK 4.1, CpuRgba snapshot) and
//! [`demo-linux6`] (WebKitGTK 6.0, CpuRgba snapshot). This binary
//! targets the `wpe_producer` module behind the `wpe` feature on
//! scrying, which produces DMABUF frames — not CPU pixels — so the
//! probe shape is "navigate → acquire one DMABUF frame → print plane
//! metadata → close plane fds → exit" rather than "save a PNG."
//!
//! Empirical: `WpeProducer::new` constructs a headless WPEDisplay,
//! which needs a working GPU + Wayland session. On a tty / over plain
//! SSH the constructor fails cleanly — the probe reports the error
//! and exits 1 rather than panicking. This is why the integration
//! smoke (`tests/wpe_input.rs`) is `#[ignore]` by default and why the
//! demo is invoked manually by a human who knows their environment.
//!
//! ```sh
//! cargo run -p demo-wpe                              # default HTML → one DMABUF frame
//! cargo run -p demo-wpe -- --probe-only              # capability probe + exit
//! cargo run -p demo-wpe -- --snapshot-test           # exit-1 if no frame within ~10 s
//! cargo run -p demo-wpe -- --url https://example.com # real-page → one DMABUF frame
//! ```

#![cfg(target_os = "linux")]

use std::process::ExitCode;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use scrying::wpe_producer::{WpeProducer, WpeProducerConfig};
use scrying::{NativeFrame, WebSurfaceCapabilities, WebSurfaceFrame, WebSurfaceProducer};

const DEFAULT_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>scrying wpe smoke</title></head>
<body style="margin:0;display:flex;align-items:center;justify-content:center;
height:100vh;background:linear-gradient(135deg,#0f172a,#7c3aed);color:#fde68a;
font:bold 64px system-ui,sans-serif">scrying &middot; wpe &middot; dmabuf</body></html>"#;

const FRAME_ACQUIRE_BUDGET: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("demo-wpe: {err}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    url: Option<String>,
    snapshot_test: bool,
    probe_only: bool,
    width: u32,
    height: u32,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut out = Args {
            url: None,
            snapshot_test: false,
            probe_only: false,
            width: 800,
            height: 600,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--url" => out.url = Some(args.next().ok_or("--url needs a value")?),
                "--width" => {
                    out.width = args
                        .next()
                        .ok_or("--width needs a value")?
                        .parse()
                        .map_err(|e| format!("invalid --width: {e}"))?;
                }
                "--height" => {
                    out.height = args
                        .next()
                        .ok_or("--height needs a value")?
                        .parse()
                        .map_err(|e| format!("invalid --height: {e}"))?;
                }
                "--snapshot-test" => out.snapshot_test = true,
                "--probe-only" => out.probe_only = true,
                "--help" | "-h" => {
                    println!(
                        "demo-wpe — WPEWebKit / WPEPlatform headless runtime probe for scrying\n\n\
                         USAGE: demo-wpe [--url URL] [--width N] [--height N]\n\
                                         [--snapshot-test] [--probe-only]\n\n\
                         Default: navigate to an inline HTML page, acquire one DMABUF\n\
                         frame, print plane metadata, close plane fds, exit 0.\n\
                         --probe-only       print WebSurfaceCapabilities + exit 0\n\
                         --snapshot-test    exit 1 if no frame arrives within ~10 s\n\
                         --url URL          navigate to URL instead of inline HTML"
                    );
                    std::process::exit(0);
                }
                _ => return Err(format!("unknown arg: {arg}")),
            }
        }
        Ok(out)
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // 1. Capability probe — same surface demo-linux6 prints. WPE's
    //    capabilities are static (not host-dependent), so passing
    //    `None` is fine.
    let caps = WebSurfaceCapabilities::probe(None);
    println!("backend: {:?}", caps.backend);
    println!("preferred mode: {:?}", caps.preferred_mode);
    println!("CPU snapshot: {:?}", caps.cpu_snapshot);
    println!("supported native frames: {:?}", caps.supported_frames);
    println!("reason: {}", caps.reason);
    if args.probe_only {
        return Ok(());
    }

    // 2. Construct the headless WPE producer. This is the step that
    //    fails on a no-GPU / no-Wayland shell; map any failure to a
    //    clean error rather than letting it panic.
    let data_dir = std::env::temp_dir().join("scrying-demo-wpe-data");
    let config = WpeProducerConfig::new(PhysicalSize::new(args.width, args.height), &data_dir);
    let mut producer = WpeProducer::new(config).map_err(|e| {
        format!(
            "WpeProducer::new failed (needs a working GPU + Wayland session): {e}"
        )
    })?;

    // 3. Navigate. demo-linux6 uses a 5 s nav timeout — match that.
    let nav_timeout = Duration::from_secs(5);
    match &args.url {
        Some(url) => {
            println!("navigating to {url}");
            producer.navigate_to_url(url, nav_timeout)?;
        }
        None => {
            println!("navigating to inline HTML");
            producer.navigate_to_string(DEFAULT_HTML, nav_timeout)?;
        }
    }
    // Drain any queued navigation events so they don't leak across
    // probe runs sharing the same temp data dir.
    while producer.poll_navigation_event().is_some() {}

    // 4. Acquire one DMABUF frame, pumping the GLib main context
    //    between retries (the `buffer-rendered` signal fires on that
    //    context — same shape as `tests/wpe_input.rs::acquire_with_pump`).
    let frame_result = acquire_with_pump(&mut producer, FRAME_ACQUIRE_BUDGET);
    let frame = match frame_result {
        Ok(f) => f,
        Err(e) => {
            if args.snapshot_test {
                return Err(format!("FAIL: no DMABUF frame within {FRAME_ACQUIRE_BUDGET:?}: {e}").into());
            }
            return Err(format!("no DMABUF frame within {FRAME_ACQUIRE_BUDGET:?}: {e}").into());
        }
    };

    // 5. Print plane metadata, then close every fd this frame owns.
    //    `DmaBufImage` has no `Drop`; fd ownership transfers with the
    //    frame and the consumer (us) is responsible for releasing them.
    match &frame {
        WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img)) => {
            println!(
                "DMABUF frame: {}x{} fmt={:?} drm_fourcc=0x{:08x} modifier=0x{:016x} gen={}",
                img.size.width,
                img.size.height,
                img.format,
                img.drm_format,
                img.drm_modifier,
                img.generation,
            );
            println!("planes: {}", img.planes.len());
            for (i, plane) in img.planes.iter().enumerate() {
                println!(
                    "  plane[{i}]: fd={} offset={} stride={}",
                    plane.fd, plane.offset, plane.stride
                );
            }
            if let Some(fd) = img.semaphore_fd {
                println!("producer semaphore fd: {fd}");
            }
            if args.snapshot_test {
                if img.size.width == 0 || img.size.height == 0 {
                    close_frame_fds_if_dmabuf(&frame);
                    return Err("FAIL: zero-sized DMABUF frame".into());
                }
                if img.planes.is_empty() {
                    close_frame_fds_if_dmabuf(&frame);
                    return Err("FAIL: DMABUF frame had no planes".into());
                }
                println!("PASS: DMABUF frame is non-empty");
            }
        }
        other => {
            // Don't leak fds even on the unexpected-variant path,
            // though by construction WpeProducer only emits DmaBufImage.
            close_frame_fds_if_dmabuf(&frame);
            return Err(format!(
                "FAIL: expected DmaBufImage frame, got mode {:?}",
                other.mode()
            )
            .into());
        }
    }
    close_frame_fds_if_dmabuf(&frame);
    Ok(())
}

/// Block-pump `glib::MainContext::default()` until `acquire_frame`
/// returns a frame or `budget` elapses. Mirrors
/// `scrying/tests/wpe_input.rs::acquire_with_pump`, with the timeout
/// surfaced as a parameter (the test hard-codes 5 s; the demo gives
/// itself 10 s for the first frame on a cold start).
fn acquire_with_pump(
    producer: &mut WpeProducer,
    budget: Duration,
) -> Result<WebSurfaceFrame, String> {
    let ctx = glib::MainContext::default();
    let deadline = Instant::now() + budget;
    loop {
        match producer.acquire_frame() {
            Ok(f) => return Ok(f),
            Err(_) if Instant::now() < deadline => {
                ctx.iteration(false);
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(e) => return Err(format!("acquire_frame timed out: {e}")),
        }
    }
}

/// Close every plane fd (and the optional producer-sync semaphore fd)
/// owned by a DMABUF frame. Mirrors the `close_frame_fds_if_dmabuf`
/// helper in `scrying/tests/wpe_input.rs`.
fn close_frame_fds_if_dmabuf(frame: &WebSurfaceFrame) {
    if let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img)) = frame {
        for plane in &img.planes {
            // SAFETY: producer-transferred DMABUF fd, not yet handed
            // off to any importer — we own it for the duration of
            // this frame and `close` is the documented release path.
            unsafe {
                libc::close(plane.fd);
            }
        }
        if let Some(fd) = img.semaphore_fd {
            // SAFETY: producer-transferred opaque-fd semaphore, same
            // ownership contract as the plane fds above.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

//! End-to-end WPE → Vulkan round-trip integration test.
//!
//! Independent integration binary (separate process from the unit-test
//! smoke) so it can stand up its own headless WebKit without colliding
//! with the unit-test producer — see
//! `scrying/src/wpe_producer/headless.rs`'s module doc for the
//! one-WPE-per-process discipline.
//!
//! Run with:
//!   cargo test -p scrying --features wpe --test wpe_to_vulkan_roundtrip \
//!     -- --ignored --nocapture
//!
//! SKIP semantics mirror `dmabuf_roundtrip.rs`: when a prerequisite is
//! missing (no display, GPU, WPE refusing to construct), print
//! `SKIP: ...` and return — cargo records the test as pass. A genuine
//! `import_frame` Err is NOT a skip (added in Task 3); it fails loudly.

#![cfg(all(target_os = "linux", feature = "wpe"))]

use dpi::PhysicalSize;
use scrying::wpe_producer::{WpeProducer, WpeProducerConfig};
use scrying::{
    DmaBufImage, NativeFrame, NavigationEvent, SyncMechanism, WebSurfaceFrame, WebSurfaceProducer,
};

#[test]
#[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
fn wpe_to_vulkan_round_trip() {
    // --- 1. Stand up the WPE producer ---
    let config = WpeProducerConfig::new(PhysicalSize::new(256, 256), std::env::temp_dir());
    let mut producer = match WpeProducer::new(config) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: WpeProducer::new failed (no display / GPU?): {e}");
            return;
        }
    };

    // --- 2. Navigate to inline HTML; verify load completion event ---
    if let Err(e) = producer.navigate_to_string(
        "<body style='margin:0;background:#1e90ff'></body>",
        std::time::Duration::from_secs(5),
    ) {
        eprintln!("SKIP: navigate_to_string failed: {e}");
        return;
    }
    let mut nav_events = Vec::new();
    while let Some(e) = producer.poll_navigation_event() {
        nav_events.push(e);
    }
    assert!(
        nav_events
            .iter()
            .any(|e| matches!(e, NavigationEvent::Completed { success: true, .. })),
        "expected a successful Completed event; got {:?}",
        nav_events
    );

    // --- 3. Acquire one DMABUF frame ---
    //
    // `wait_for_load` (inside navigate_to_string) returns when load-changed
    // FINISHED fires; the first `buffer-rendered` arrives slightly later
    // on the same MainContext. Retry briefly, pumping the GLib main context
    // each iteration so the pending buffer-rendered signal can dispatch.
    // We can't reach `producer.handles.main_context` from outside the crate
    // (it's `pub(super)`), but `glib::MainContext::default()` is the same
    // context the WPE producer uses (it was created with
    // `glib::MainContext::default()` in producer.rs), so iterating it here
    // delivers any pending callbacks including `buffer-rendered`.
    let frame = {
        let ctx = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match producer.acquire_frame() {
                Ok(f) => break f,
                Err(_) if std::time::Instant::now() < deadline => {
                    ctx.iteration(false); // dispatch pending GLib events (non-blocking)
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                Err(e) => panic!("FAIL: acquire_frame timed out after navigate: {e}"),
            }
        }
    };
    let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(image)) = frame else {
        panic!("FAIL: expected a DMABUF frame, got something else");
    };

    // WPE-side sanity (always-on assertions).
    assert!(image.size.width > 0 && image.size.height > 0, "non-zero size");
    assert!(!image.planes.is_empty(), "at least one plane");
    assert!(image.planes[0].fd >= 0, "valid dup'd fd");
    assert_eq!(image.producer_sync, SyncMechanism::None);
    eprintln!(
        "wpe→vk: {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}",
        image.size.width,
        image.size.height,
        image.drm_format,
        image.drm_modifier,
        image.planes.len()
    );

    // --- 4. (Task 3 hands this to the importer; Task 1 closes fds.) ---
    close_producer_fds(&image);
}

/// Close producer-owned dup'd fds on a `DmaBufImage` that was never
/// handed to the importer. Task 3 narrows this to the SKIP branches
/// once the importer takes ownership on success.
fn close_producer_fds(image: &DmaBufImage) {
    for plane in &image.planes {
        // SAFETY: producer-owned dup'd fd not yet transferred to the importer.
        unsafe { libc::close(plane.fd); }
    }
    if let Some(fd) = image.semaphore_fd {
        unsafe { libc::close(fd); }
    }
}

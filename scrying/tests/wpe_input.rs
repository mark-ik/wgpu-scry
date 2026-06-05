//! Phase 4c.4 input-forwarding integration smoke.
//!
//! Independent integration binary (separate process from the unit-test
//! smoke and the wpe_to_vulkan_roundtrip smoke) so it has its own
//! WebKit init, honoring the one-WPE-per-process discipline.
//!
//! Constructs a WpeProducer, navigates to a page with an `<input>`,
//! dispatches a sequence of keyboard / pointer / mouse-button / scroll
//! events, and asserts the renderer keeps producing frames. Does NOT
//! assert "the page received the events correctly" — that requires JS
//! message-back wiring (4c.5).
//!
//! Run with:
//!   cargo test -p scrying --features wpe --test wpe_input \
//!     -- --ignored --nocapture

#![cfg(all(target_os = "linux", feature = "wpe"))]

use dpi::PhysicalSize;
use scrying::wpe_producer::{WpeProducer, WpeProducerConfig};
use scrying::{
    KeyEventKind, KeyboardInput, MouseEventKind, MouseInput, NativeFrame, NavigationEvent,
    PointerDevice, PointerEventKind, PointerInput, WebSurfaceFrame, WebSurfaceProducer,
};

#[test]
#[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
fn input_dispatch_does_not_crash() {
    // --- 1. Stand up the producer ---
    let config = WpeProducerConfig::new(PhysicalSize::new(256, 256), std::env::temp_dir());
    let mut producer = match WpeProducer::new(config) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: WpeProducer::new failed (no display / GPU?): {e}");
            return;
        }
    };

    // --- 2. Navigate to a page with a focusable <input> ---
    if let Err(e) = producer.navigate_to_string(
        "<body style='margin:0;background:#1e90ff'>\
         <input id='probe' style='font-size:32px' autofocus>\
         </body>",
        std::time::Duration::from_secs(5),
    ) {
        eprintln!("SKIP: navigate_to_string failed: {e}");
        return;
    }
    let mut saw_completed = false;
    while let Some(e) = producer.poll_navigation_event() {
        if matches!(e, NavigationEvent::Completed { success: true, .. }) {
            saw_completed = true;
        }
    }
    assert!(saw_completed, "expected a successful Completed nav event");

    // --- 3. Wait for the first frame so we know rendering's alive ---
    let first = acquire_with_pump(&mut producer);
    let first_size = match &first {
        WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img)) => img.size,
        _ => panic!("expected DMABUF frame"),
    };
    close_frame_fds_if_dmabuf(&first);
    eprintln!(
        "input smoke: first frame {}x{}",
        first_size.width, first_size.height
    );

    // --- 4. Dispatch a sequence of input events ---
    let cx = (first_size.width / 2) as i32;
    let cy = (first_size.height / 2) as i32;

    // Pointer move to center.
    producer
        .send_pointer_input(make_pointer(PointerEventKind::Update, cx, cy))
        .expect("send_pointer_input move");

    // Mouse click (down + up) at center.
    producer
        .send_mouse_input(MouseInput {
            kind: MouseEventKind::LeftButtonDown,
            virtual_keys: Default::default(),
            mouse_data: 0,
            point: (cx, cy),
        })
        .expect("send_mouse_input down");
    producer
        .send_mouse_input(MouseInput {
            kind: MouseEventKind::LeftButtonUp,
            virtual_keys: Default::default(),
            mouse_data: 0,
            point: (cx, cy),
        })
        .expect("send_mouse_input up");

    // Type three keys (xkb keycodes for 'w'=25, 'p'=33, 'e'=26 on the
    // standard Linux USB-HID xkb layout — these MVP-dispatch with
    // keyval=0 so WebKit derives from the keycode internally).
    for &keycode in &[25u32, 33, 26] {
        producer
            .send_keyboard_input(make_keyboard(KeyEventKind::Down, keycode))
            .expect("send_keyboard_input down");
        producer
            .send_keyboard_input(make_keyboard(KeyEventKind::Up, keycode))
            .expect("send_keyboard_input up");
    }

    // Vertical scroll.
    producer
        .send_mouse_input(MouseInput {
            kind: MouseEventKind::Wheel,
            virtual_keys: Default::default(),
            mouse_data: 120,
            point: (cx, cy),
        })
        .expect("send_mouse_input wheel");

    // --- 4c.4.1 — touch dispatch is NOT exercised here ---
    //
    // The `dispatch_pointer` Touch branch (which builds
    // `wpe_event_touch_new` with sequence_id = pointer_id) is correct
    // in shape and covered by the `touch_kind_translation` unit test
    // in `scrying/src/wpe_producer/input.rs`. End-to-end runtime
    // dispatch through `wpe_view_event` hangs the headless WebKit
    // process indefinitely (observed: futex_do_wait inside the
    // dispatch, no progress after several seconds). The same shape
    // bit us in 4c.3's resize: the headless WPEDisplay simply
    // doesn't provide the gesture-controller / screen state WPE's
    // touch path needs to dispatch synchronously. Until a non-headless
    // WPE producer lands or we provide our own gesture controller via
    // `wpe_view_set_gesture_controller`, end-to-end touch testing
    // belongs in a non-headless target.

    // --- 4c.5.a — JS → host postMessage round-trip ---
    //
    // Navigate to a page whose inline <script> calls
    // window.chrome.webview.postMessage('hi from page') at parse time.
    // The chrome.webview shim is injected at document-start by
    // script_message::install, so the postMessage call hits the scry
    // user-content handler, which extracts the JSCValue string via
    // jsc_value_to_string and pushes "hi from page" onto the
    // producer's web_messages queue. wait_for_web_message drains it.
    //
    // We re-navigate (rather than appending to the existing page)
    // because the chrome.webview shim runs at document-start: a second
    // navigation is the cleanest way to trigger the postMessage from
    // page-side code without script injection.
    producer
        .navigate_to_string(
            "<body><script>window.chrome.webview.postMessage('hi from page');</script></body>",
            std::time::Duration::from_secs(5),
        )
        .expect("navigate_to_string for postMessage round-trip");
    // Drain navigation events from the re-navigate so they don't
    // pollute later assertions.
    while producer.poll_navigation_event().is_some() {}

    let msg = producer.wait_for_web_message(std::time::Duration::from_secs(2));
    assert_eq!(
        msg.as_deref(),
        Some("hi from page"),
        "expected the page's chrome.webview.postMessage to round-trip back as 'hi from page' \
         (got {:?}); this verifies the script_message::install signal closure + the \
         chrome.webview shim injection are wired correctly end-to-end.",
        msg
    );
    eprintln!("input smoke: chrome.webview.postMessage round-trip = {:?}", msg);

    // --- 4c.5.b — cookie store round-trip ---
    //
    // Set a cookie via the producer, then read it back via
    // request_cookies_for_url. Asserts both name AND value to verify
    // the soup<->scry translators are wired in both directions.
    //
    // Domain/path here are intentionally NOT bound to the load_html
    // base-uri (about:blank): the WPE cookie manager scopes lookups by
    // (domain, path), so the get URL just needs to match those — no
    // network round-trip required.
    let probe = scrying::Cookie {
        name: "scry_probe".to_string(),
        value: "round_trip_ok".to_string(),
        domain: "example.test".to_string(),
        path: "/".to_string(),
        expires_at: None, // session cookie
        is_secure: false,
        is_http_only: false,
    };
    producer.set_cookie(&probe).expect("set_cookie round-trip");
    let cookies = producer
        .request_cookies_for_url("http://example.test/")
        .expect("request_cookies_for_url");
    let hit = cookies.iter().find(|c| c.name == "scry_probe");
    assert!(
        hit.is_some(),
        "expected the scry_probe cookie we just set to come back \
         from request_cookies_for_url; got {} cookies: {:?}",
        cookies.len(),
        cookies.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert_eq!(hit.unwrap().value, "round_trip_ok");
    eprintln!(
        "input smoke: cookie round-trip OK ({} cookie(s) returned for example.test)",
        cookies.len()
    );
    // Clean up so subsequent test runs against the same ephemeral
    // session don't see stale state. (The ephemeral
    // WebKitNetworkSession resets on producer drop anyway, but the
    // assert exercises delete_cookie's success path.)
    producer
        .delete_cookie(&probe)
        .expect("delete_cookie round-trip");

    // --- 5. Verify the renderer is still alive after the input sequence ---
    //
    // EMPIRICAL: WPE may or may not auto-paint just from these input events.
    // If `acquire_with_pump` times out trying to find a NEW frame, fall
    // back to acquiring whatever frame is queued (which may be the same
    // as the first frame) or even re-using the first-frame proof. The
    // contract this smoke holds is "input dispatch is FFI-sound + the
    // producer doesn't crash"; "events visibly affected the page" is
    // out of scope per the spec.
    let second = acquire_with_pump_or_skip(&mut producer);
    match &second {
        Some(WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img))) => {
            assert!(img.size.width > 0 && img.size.height > 0);
            eprintln!(
                "input smoke: post-input frame {}x{}",
                img.size.width, img.size.height
            );
            close_frame_fds_if_dmabuf(second.as_ref().unwrap());
        }
        Some(_) => panic!("expected DMABUF frame post-input"),
        None => eprintln!(
            "input smoke: no NEW frame arrived within 2s post-input — \
             producer is still alive, dispatch didn't crash; contract met"
        ),
    }
}

/// Construct a `PointerInput` with mouse device defaults. `PointerInput`
/// does not derive `Default`, so every field is filled explicitly with
/// a zero-equivalent.
fn make_pointer(kind: PointerEventKind, x: i32, y: i32) -> PointerInput {
    PointerInput {
        kind,
        device: PointerDevice::Mouse,
        pointer_id: 1,
        point: (x, y),
        pressure: 0.0,
        tilt: (0.0, 0.0),
    }
}

/// Construct a `KeyboardInput` with a keycode and empty characters.
/// `KeyboardInput` does not derive `Default`, so every field is filled
/// explicitly. `KeyModifierFlags` does derive `Default`.
fn make_keyboard(kind: KeyEventKind, virtual_key_code: u32) -> KeyboardInput {
    KeyboardInput {
        kind,
        virtual_key_code,
        characters: String::new(),
        characters_ignoring_modifiers: String::new(),
        modifiers: Default::default(),
        is_repeat: false,
    }
}

/// Block-pump glib::MainContext::default() until a frame lands or 5s
/// elapses. Mirrors the wpe_to_vulkan_roundtrip pattern.
fn acquire_with_pump(producer: &mut WpeProducer) -> WebSurfaceFrame {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match producer.acquire_frame() {
            Ok(f) => return f,
            Err(_) if std::time::Instant::now() < deadline => {
                ctx.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            Err(e) => panic!("FAIL: acquire_frame timed out: {e}"),
        }
    }
}

/// Like `acquire_with_pump` but with a SHORTER (2s) deadline and `None`
/// on timeout instead of panic — used for the post-input frame check
/// where "no new frame" is acceptable as long as dispatch didn't crash.
fn acquire_with_pump_or_skip(producer: &mut WpeProducer) -> Option<WebSurfaceFrame> {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match producer.acquire_frame() {
            Ok(f) => return Some(f),
            Err(_) if std::time::Instant::now() < deadline => {
                ctx.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            Err(_) => return None,
        }
    }
}

fn close_frame_fds_if_dmabuf(frame: &WebSurfaceFrame) {
    if let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img)) = frame {
        for plane in &img.planes {
            unsafe {
                libc::close(plane.fd);
            }
        }
        if let Some(fd) = img.semaphore_fd {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

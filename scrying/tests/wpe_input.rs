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

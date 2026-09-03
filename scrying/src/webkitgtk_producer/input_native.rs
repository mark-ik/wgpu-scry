// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Native `GdkEvent` synthesis + dispatch via `gtk_main_do_event`.
//!
//! Closes the `isTrusted` gap that the JS-event path
//! ([`super::input`]) cannot — synthesized DOM events that arrive
//! through WebKit's normal input pipeline have `event.isTrusted ===
//! true`, fire native click side-effects, and let IME / shortcut
//! gating work.
//!
//! The WebKitWebView is hosted inside a producer-owned
//! `GtkOffscreenWindow`. Its `GdkWindow` is realized once
//! `show_all` runs, even though the window is never on screen, so
//! `gtk_main_do_event` can route events to it via the standard GTK
//! routing path.
//!
//! ## Refcount discipline
//!
//! `gdk_event_free` walks owned fields (window, device) and unrefs
//! them. We ref-bump before assigning so the producer's
//! `widget.window()` / `seat.pointer()` references survive the event
//! drop. Done via `g_object_ref` on the raw pointer between
//! `to_glib_none()` and the struct assignment.

use gdk::ffi as gdk_sys;
use gdk::prelude::*;
use gdk::{Display, EventType, ModifierType};
use glib::translate::ToGlibPtr;
use webkit2gtk::WebView;

use crate::{
    KeyEventKind, KeyboardInput, MouseEventKind, MouseInput, MouseVirtualKeys, PointerDevice,
    PointerEventKind, PointerInput, WebSurfaceError,
};

/// Synthesize + dispatch a [`MouseInput`] as a native `GdkEvent` so
/// pages observe `event.isTrusted === true`.
pub(crate) fn dispatch_mouse(webview: &WebView, input: MouseInput) -> Result<(), WebSurfaceError> {
    let ctx = NativeDispatchContext::for_webview(webview)?;
    let state = modifier_flags_from_vk(&input.virtual_keys);
    let time = gtk::current_event_time();
    let (x, y) = (input.point.0 as f64, input.point.1 as f64);

    let mut event = build_mouse_event(input, &ctx, state, time, x, y)?;
    // `gdk_event_set_device` handles every event-type-specific
    // device-storage detail (some types carry it inline, some in a
    // private side table) — preferred over poking the raw struct
    // fields directly.
    if let Some(device) = ctx.device.as_ref() {
        event.set_device(Some(device));
    }
    gtk::main_do_event(&mut event);
    Ok(())
}

fn build_mouse_event(
    input: MouseInput,
    ctx: &NativeDispatchContext,
    state: ModifierType,
    time: u32,
    x: f64,
    y: f64,
) -> Result<gdk::Event, WebSurfaceError> {
    match input.kind {
        MouseEventKind::Leave => {
            let mut event = gdk::Event::new(EventType::LeaveNotify);
            {
                let crossing = event
                    .downcast_mut::<gdk::EventCrossing>()
                    .ok_or_else(|| platform("LeaveNotify downcast failed"))?;
                let raw = crossing.as_mut();
                raw.x = x;
                raw.y = y;
                raw.x_root = x;
                raw.y_root = y;
                raw.time = time;
                raw.state = state.bits();
                raw.mode = gdk_sys::GDK_CROSSING_NORMAL;
                raw.detail = gdk_sys::GDK_NOTIFY_NONLINEAR;
                raw.focus = 0;
                assign_window_ref(&mut raw.window, ctx);
            }
            Ok(event)
        }
        MouseEventKind::Wheel | MouseEventKind::HorizontalWheel => {
            let mut event = gdk::Event::new(EventType::Scroll);
            {
                let scroll = event
                    .downcast_mut::<gdk::EventScroll>()
                    .ok_or_else(|| platform("Scroll downcast failed"))?;
                let raw = scroll.as_mut();
                raw.x = x;
                raw.y = y;
                raw.x_root = x;
                raw.y_root = y;
                raw.time = time;
                raw.state = state.bits();
                let delta = input.mouse_data as f64;
                let (dx, dy) = if matches!(input.kind, MouseEventKind::HorizontalWheel) {
                    (delta * 16.0 / 120.0, 0.0)
                } else {
                    (0.0, -delta * 16.0 / 120.0)
                };
                raw.direction = gdk_sys::GDK_SCROLL_SMOOTH;
                raw.delta_x = dx;
                raw.delta_y = dy;
                raw.is_stop = 0;
                assign_window_ref(&mut raw.window, ctx);
            }
            Ok(event)
        }
        MouseEventKind::Move => {
            let mut event = gdk::Event::new(EventType::MotionNotify);
            {
                let motion = event
                    .downcast_mut::<gdk::EventMotion>()
                    .ok_or_else(|| platform("MotionNotify downcast failed"))?;
                let raw = motion.as_mut();
                raw.x = x;
                raw.y = y;
                raw.x_root = x;
                raw.y_root = y;
                raw.time = time;
                raw.state = state.bits();
                raw.is_hint = 0;
                assign_window_ref(&mut raw.window, ctx);
            }
            Ok(event)
        }
        _ => {
            let button = mouse_button_index(input.kind);
            let mut event = gdk::Event::new(mouse_event_type(input.kind));
            {
                let button_evt = event
                    .downcast_mut::<gdk::EventButton>()
                    .ok_or_else(|| platform("Button downcast failed"))?;
                let raw = button_evt.as_mut();
                raw.x = x;
                raw.y = y;
                raw.x_root = x;
                raw.y_root = y;
                raw.time = time;
                raw.state = state.bits();
                raw.button = button;
                assign_window_ref(&mut raw.window, ctx);
            }
            Ok(event)
        }
    }
}

/// Pointer events go through GDK's touch path. We map our
/// [`PointerInput`] onto `MotionNotify` / `ButtonPress` /
/// `ButtonRelease` for now — true `EventTouch` requires per-sequence
/// tracking we don't carry yet.
pub(crate) fn dispatch_pointer(
    webview: &WebView,
    input: PointerInput,
) -> Result<(), WebSurfaceError> {
    // Re-route through the mouse path: pointer kind → mouse kind,
    // with the same coordinate / button / state plumbing.
    let kind = match input.kind {
        PointerEventKind::Down | PointerEventKind::Activate => match input.device {
            PointerDevice::Touch | PointerDevice::Pen | PointerDevice::Mouse => {
                MouseEventKind::LeftButtonDown
            }
        },
        PointerEventKind::Up => MouseEventKind::LeftButtonUp,
        PointerEventKind::Update => MouseEventKind::Move,
        PointerEventKind::Enter => MouseEventKind::Move,
        PointerEventKind::Leave => MouseEventKind::Leave,
        PointerEventKind::CaptureChanged => return Ok(()), // no-op
    };
    dispatch_mouse(
        webview,
        MouseInput {
            kind,
            virtual_keys: MouseVirtualKeys::default(),
            mouse_data: 0,
            point: input.point,
        },
    )
}

/// Synthesize + dispatch a [`KeyboardInput`] as a `GdkEventKey`.
pub(crate) fn dispatch_keyboard(
    webview: &WebView,
    input: KeyboardInput,
) -> Result<(), WebSurfaceError> {
    let event_type = match input.kind {
        KeyEventKind::Down => EventType::KeyPress,
        KeyEventKind::Up => EventType::KeyRelease,
        KeyEventKind::ModifiersChanged => return Ok(()),
    };
    let ctx = NativeDispatchContext::for_webview(webview)?;
    let state = modifier_flags_from_kb(&input);
    let time = gtk::current_event_time();
    let keyval = keyval_from_input(&input);
    let mut event = gdk::Event::new(event_type);
    {
        let key_evt = event
            .downcast_mut::<gdk::EventKey>()
            .ok_or_else(|| platform("Key downcast failed"))?;
        let raw = key_evt.as_mut();
        raw.time = time;
        raw.state = state.bits();
        raw.keyval = keyval;
        raw.length = 0;
        raw.string = std::ptr::null_mut();
        raw.hardware_keycode = input.virtual_key_code as u16;
        raw.group = 0;
        raw.is_modifier = 0;
        assign_window_ref(&mut raw.window, &ctx);
    }
    if let Some(device) = ctx.device.as_ref() {
        event.set_device(Some(device));
    }
    gtk::main_do_event(&mut event);
    Ok(())
}

/// Move keyboard focus into the WebView via the GTK widget hierarchy.
pub(crate) fn focus(webview: &WebView) -> Result<(), WebSurfaceError> {
    use gtk::prelude::WidgetExt;
    webview.grab_focus();
    Ok(())
}

/// Pre-resolved handles needed for a single event dispatch — looked
/// up once per call so a missing `WebView::window()` (unrealized) or
/// missing pointer device surfaces as a typed error rather than a
/// segfault from null-FFI.
struct NativeDispatchContext {
    window: gdk::Window,
    device: Option<gdk::Device>,
}

impl NativeDispatchContext {
    fn for_webview(webview: &WebView) -> Result<Self, WebSurfaceError> {
        use gtk::prelude::WidgetExt;
        let window = webview
            .window()
            .ok_or_else(|| platform("WebView has no GdkWindow (not yet realized)"))?;
        let device = Display::default()
            .and_then(|d| d.default_seat())
            .and_then(|s| s.pointer());
        Ok(Self { window, device })
    }
}

/// Ref-bump the GdkWindow pointer and store into the event field.
/// `gdk_event_free` will eventually unref it when the event drops.
fn assign_window_ref(slot: &mut *mut gdk_sys::GdkWindow, ctx: &NativeDispatchContext) {
    let ptr: *mut gdk_sys::GdkWindow = ctx.window.to_glib_none().0;
    unsafe {
        glib::gobject_ffi::g_object_ref(ptr as *mut _);
        *slot = ptr;
    }
}

fn mouse_event_type(kind: MouseEventKind) -> EventType {
    match kind {
        MouseEventKind::LeftButtonDown
        | MouseEventKind::MiddleButtonDown
        | MouseEventKind::RightButtonDown
        | MouseEventKind::XButtonDown => EventType::ButtonPress,
        MouseEventKind::LeftButtonUp
        | MouseEventKind::MiddleButtonUp
        | MouseEventKind::RightButtonUp
        | MouseEventKind::XButtonUp => EventType::ButtonRelease,
        MouseEventKind::LeftButtonDoubleClick
        | MouseEventKind::MiddleButtonDoubleClick
        | MouseEventKind::RightButtonDoubleClick
        | MouseEventKind::XButtonDoubleClick => EventType::DoubleButtonPress,
        MouseEventKind::Move => EventType::MotionNotify,
        MouseEventKind::Wheel | MouseEventKind::HorizontalWheel => EventType::Scroll,
        MouseEventKind::Leave => EventType::LeaveNotify,
    }
}

fn mouse_button_index(kind: MouseEventKind) -> u32 {
    match kind {
        MouseEventKind::LeftButtonDown
        | MouseEventKind::LeftButtonUp
        | MouseEventKind::LeftButtonDoubleClick => 1,
        MouseEventKind::MiddleButtonDown
        | MouseEventKind::MiddleButtonUp
        | MouseEventKind::MiddleButtonDoubleClick => 2,
        MouseEventKind::RightButtonDown
        | MouseEventKind::RightButtonUp
        | MouseEventKind::RightButtonDoubleClick => 3,
        MouseEventKind::XButtonDown
        | MouseEventKind::XButtonUp
        | MouseEventKind::XButtonDoubleClick => 8,
        _ => 1,
    }
}

fn modifier_flags_from_vk(vk: &MouseVirtualKeys) -> ModifierType {
    let mut m = ModifierType::empty();
    if vk.shift {
        m |= ModifierType::SHIFT_MASK;
    }
    if vk.control {
        m |= ModifierType::CONTROL_MASK;
    }
    if vk.left_button {
        m |= ModifierType::BUTTON1_MASK;
    }
    if vk.middle_button {
        m |= ModifierType::BUTTON2_MASK;
    }
    if vk.right_button {
        m |= ModifierType::BUTTON3_MASK;
    }
    m
}

fn modifier_flags_from_kb(input: &KeyboardInput) -> ModifierType {
    let mut m = ModifierType::empty();
    if input.modifiers.shift {
        m |= ModifierType::SHIFT_MASK;
    }
    if input.modifiers.control {
        m |= ModifierType::CONTROL_MASK;
    }
    if input.modifiers.alt {
        m |= ModifierType::MOD1_MASK;
    }
    if input.modifiers.meta {
        m |= ModifierType::META_MASK;
    }
    m
}

/// Map a [`KeyboardInput`] to a GDK keysym. Primary source is the
/// character payload (best for ASCII / Latin / Unicode key labels);
/// falls back to interpreting `virtual_key_code` as the X11 keycode
/// the host already produced, and finally a sentinel `VoidSymbol` if
/// the character is empty.
fn keyval_from_input(input: &KeyboardInput) -> u32 {
    if let Some(c) = input.characters.chars().next() {
        let keyval = unsafe { gdk_sys::gdk_unicode_to_keyval(c as u32) };
        if keyval != 0xFFFFFF
        /* GDK_KEY_VoidSymbol */
        {
            return keyval;
        }
    }
    // If unicode mapping failed, treat virtual_key_code as a keysym
    // directly (some hosts feed keysyms in this slot).
    if input.virtual_key_code != 0 {
        return input.virtual_key_code;
    }
    0xFFFFFF
}

fn platform(msg: &'static str) -> WebSurfaceError {
    WebSurfaceError::Platform(msg.to_string())
}

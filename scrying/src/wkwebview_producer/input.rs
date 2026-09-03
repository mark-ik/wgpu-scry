// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Mouse / scroll-wheel event synthesis. Pure functions: take a
//! [`MouseInput`] from the public API and emit an `NSEvent` keyed
//! to a [`MouseTarget`] (the `NSResponder` slot the producer should
//! invoke). No producer state is captured here — the caller owns
//! the dispatch.

use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSWindow};
use objc2_core_graphics::{CGEvent, CGScrollEventUnit};
use objc2_foundation::NSPoint;
use objc2_web_kit::WKWebView;

use crate::{MouseEventKind, MouseInput, MouseVirtualKeys, WebSurfaceError};

/// Which `NSResponder` method should receive the synthesized event.
#[derive(Clone, Copy)]
pub(super) enum MouseTarget {
    MouseDown,
    MouseUp,
    MouseDragged,
    MouseMoved,
    RightMouseDown,
    RightMouseUp,
    RightMouseDragged,
    OtherMouseDown,
    OtherMouseUp,
    OtherMouseDragged,
    MouseExited,
    ScrollWheel,
}

pub(super) struct MouseDispatch {
    pub(super) event: Retained<NSEvent>,
    pub(super) target: MouseTarget,
}

/// Translate a `MouseInput` into an `NSEvent` to fire at the WKWebView.
///
/// Coordinates: `event.point` is "physical pixels relative to the
/// WebView's top-left." AppKit needs window-coordinates in points with
/// a bottom-left origin. The conversion is:
///
/// 1. Divide by the parent window's `backingScaleFactor` to get points.
/// 2. Flip Y around the WebView's local height to bottom-left origin.
/// 3. `convertPoint_toView(None)` to lift into window space.
pub(super) fn synthesize_mouse_event(
    webview: &WKWebView,
    window: &NSWindow,
    event: MouseInput,
) -> Result<MouseDispatch, WebSurfaceError> {
    let scale = window.backingScaleFactor().max(1.0);
    let bounds = webview.bounds();
    let x_local = f64::from(event.point.0) / scale;
    let y_local_top_left = f64::from(event.point.1) / scale;
    let y_local_bottom_left = bounds.size.height - y_local_top_left;
    let local_pt = NSPoint::new(x_local, y_local_bottom_left);
    let window_pt = webview.convertPoint_toView(local_pt, None);

    let modifier_flags = modifier_flags_from_virtual_keys(event.virtual_keys);
    let window_number = window.windowNumber();

    let (event_type, target, click_count, button_number, pressure) = match event.kind {
        MouseEventKind::LeftButtonDown => (
            NSEventType::LeftMouseDown,
            MouseTarget::MouseDown,
            1,
            0,
            1.0,
        ),
        MouseEventKind::LeftButtonUp => (NSEventType::LeftMouseUp, MouseTarget::MouseUp, 1, 0, 0.0),
        MouseEventKind::LeftButtonDoubleClick => (
            NSEventType::LeftMouseDown,
            MouseTarget::MouseDown,
            2,
            0,
            1.0,
        ),
        MouseEventKind::RightButtonDown => (
            NSEventType::RightMouseDown,
            MouseTarget::RightMouseDown,
            1,
            0,
            1.0,
        ),
        MouseEventKind::RightButtonUp => (
            NSEventType::RightMouseUp,
            MouseTarget::RightMouseUp,
            1,
            0,
            0.0,
        ),
        MouseEventKind::RightButtonDoubleClick => (
            NSEventType::RightMouseDown,
            MouseTarget::RightMouseDown,
            2,
            0,
            1.0,
        ),
        MouseEventKind::MiddleButtonDown => (
            NSEventType::OtherMouseDown,
            MouseTarget::OtherMouseDown,
            1,
            2,
            1.0,
        ),
        MouseEventKind::MiddleButtonUp => (
            NSEventType::OtherMouseUp,
            MouseTarget::OtherMouseUp,
            1,
            2,
            0.0,
        ),
        MouseEventKind::MiddleButtonDoubleClick => (
            NSEventType::OtherMouseDown,
            MouseTarget::OtherMouseDown,
            2,
            2,
            1.0,
        ),
        MouseEventKind::XButtonDown => (
            NSEventType::OtherMouseDown,
            MouseTarget::OtherMouseDown,
            1,
            event.mouse_data.max(3),
            1.0,
        ),
        MouseEventKind::XButtonUp => (
            NSEventType::OtherMouseUp,
            MouseTarget::OtherMouseUp,
            1,
            event.mouse_data.max(3),
            0.0,
        ),
        MouseEventKind::XButtonDoubleClick => (
            NSEventType::OtherMouseDown,
            MouseTarget::OtherMouseDown,
            2,
            event.mouse_data.max(3),
            1.0,
        ),
        MouseEventKind::Move => {
            // If a button is held, AppKit reports a `*MouseDragged`
            // event instead of `MouseMoved`. Match that — WKWebView
            // gates pointer-move handling on this distinction.
            if event.virtual_keys.left_button {
                (
                    NSEventType::LeftMouseDragged,
                    MouseTarget::MouseDragged,
                    0,
                    0,
                    0.0,
                )
            } else if event.virtual_keys.right_button {
                (
                    NSEventType::RightMouseDragged,
                    MouseTarget::RightMouseDragged,
                    0,
                    0,
                    0.0,
                )
            } else if event.virtual_keys.middle_button {
                (
                    NSEventType::OtherMouseDragged,
                    MouseTarget::OtherMouseDragged,
                    0,
                    2,
                    0.0,
                )
            } else {
                (NSEventType::MouseMoved, MouseTarget::MouseMoved, 0, 0, 0.0)
            }
        }
        MouseEventKind::Leave => (
            NSEventType::MouseExited,
            MouseTarget::MouseExited,
            0,
            0,
            0.0,
        ),
        MouseEventKind::Wheel | MouseEventKind::HorizontalWheel => {
            // Scroll wheel events have no `NSEvent` factory — build a
            // CGEvent and round-trip through `eventWithCGEvent:`.
            // Pass through the webview + window so the scroll
            // event carries a real screen-space location and the
            // host's modifier-key state — without these, WebKit
            // can't attribute the scroll to the WKWebView nor
            // honor cmd-scroll-to-zoom etc.
            return synthesize_scroll_wheel_event(webview, window, event);
        }
    };

    // `NSEvent::mouseEventWithType:` does not expose `buttonNumber`
    // directly — Apple infers the button from the event type. So
    // X-button slots and middle-vs-other-mouse distinctions ride on
    // the kind of event we synthesize, not on `mouse_data`. Setting
    // a synthetic per-button `buttonNumber` (so JS observes
    // `event.button == 3/4` for X-buttons) requires the CGEvent path
    // and is deferred along with scroll-wheel.
    let _ = button_number;

    let ns_event = if matches!(event_type, NSEventType::MouseExited) {
        // SAFETY: `userData` is allowed to be null when no tracking
        // area is associated with the synthesized event.
        unsafe {
            NSEvent::enterExitEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_trackingNumber_userData(
                event_type,
                window_pt,
                modifier_flags,
                0.0,
                window_number,
                None,
                0,
                0,
                std::ptr::null_mut(),
            )
        }
    } else {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            event_type,
            window_pt,
            modifier_flags,
            0.0,
            window_number,
            None,
            0,
            click_count,
            pressure,
        )
    };

    let ns_event = ns_event.ok_or_else(|| {
        WebSurfaceError::Platform(
            "NSEvent factory returned nil for the synthesized mouse event".into(),
        )
    })?;

    Ok(MouseDispatch {
        event: ns_event,
        target,
    })
}

/// Build a synthetic ScrollWheel `NSEvent` via `CGEventCreateScrollWheelEvent2`.
///
/// `event.mouse_data` carries the wheel delta. Sign convention matches
/// AppKit: positive = up / right. Pixel units (not lines) so the
/// consumer's host-side scroll-amount accounting maps directly to
/// pixel deltas.
///
/// Sets the CGEvent's location and modifier flags so:
/// - WebKit attributes the scroll to the WKWebView under that
///   screen point (rather than wherever the cursor happens to be,
///   or "nowhere" → first-responder fallback).
/// - cmd-scroll-to-zoom / shift-scroll-for-horizontal / etc.
///   honor the host's modifier-key state.
pub(super) fn synthesize_scroll_wheel_event(
    webview: &WKWebView,
    window: &NSWindow,
    event: MouseInput,
) -> Result<MouseDispatch, WebSurfaceError> {
    let (wheel_count, wheel1, wheel2) = match event.kind {
        MouseEventKind::Wheel => (1u32, event.mouse_data, 0i32),
        MouseEventKind::HorizontalWheel => (2u32, 0i32, event.mouse_data),
        _ => unreachable!("synthesize_scroll_wheel_event called with non-wheel kind"),
    };
    let cg_event = CGEvent::new_scroll_wheel_event2(
        None,
        CGScrollEventUnit::Pixel,
        wheel_count,
        wheel1,
        wheel2,
        0,
    )
    .ok_or_else(|| {
        WebSurfaceError::Platform("CGEventCreateScrollWheelEvent2 returned nil".into())
    })?;

    // Compute the event location in CGEvent's coordinate system:
    // global display points, top-left origin. We start from
    // `event.point` (physical pixels, webview-local, top-left),
    // walk through webview → window → screen → display.
    let scale = window.backingScaleFactor().max(1.0);
    let x_local_pt = f64::from(event.point.0) / scale;
    let y_local_top_pt = f64::from(event.point.1) / scale;
    let bounds = webview.bounds();
    // Webview-local: AppKit is bottom-left, so flip Y here too.
    let local_pt = NSPoint::new(x_local_pt, bounds.size.height - y_local_top_pt);
    // Lift through the webview's view chain to window coords
    // (still bottom-left), then to screen coords (still
    // bottom-left, but in NSScreen-relative space).
    let window_pt = webview.convertPoint_toView(local_pt, None);
    let screen_pt = window.convertPointToScreen(window_pt);
    // CGEvent uses top-left origin in global display points. The
    // primary screen sits at `(0, primary_height)` in
    // bottom-left NSScreen coords; flip Y around its height.
    let primary_screen_height = objc2_app_kit::NSScreen::mainScreen(
        objc2_foundation::MainThreadMarker::new()
            .expect("synthesize_scroll_wheel_event must run on main thread"),
    )
    .map(|s| s.frame().size.height)
    .unwrap_or(0.0);
    let cg_location = objc2_core_foundation::CGPoint {
        x: screen_pt.x,
        y: primary_screen_height - screen_pt.y,
    };
    CGEvent::set_location(Some(&cg_event), cg_location);

    // Map host modifier-key state to CGEventFlags so cmd-scroll
    // / shift-scroll / etc. behave correctly.
    let mut cg_flags = objc2_core_graphics::CGEventFlags::empty();
    if event.virtual_keys.shift {
        cg_flags |= objc2_core_graphics::CGEventFlags::MaskShift;
    }
    if event.virtual_keys.control {
        cg_flags |= objc2_core_graphics::CGEventFlags::MaskControl;
    }
    CGEvent::set_flags(Some(&cg_event), cg_flags);

    let ns_event = NSEvent::eventWithCGEvent(&cg_event).ok_or_else(|| {
        WebSurfaceError::Platform("NSEvent::eventWithCGEvent returned nil".into())
    })?;
    Ok(MouseDispatch {
        event: ns_event,
        target: MouseTarget::ScrollWheel,
    })
}

pub(super) fn modifier_flags_from_virtual_keys(keys: MouseVirtualKeys) -> NSEventModifierFlags {
    let mut flags = NSEventModifierFlags::empty();
    if keys.shift {
        flags |= NSEventModifierFlags::Shift;
    }
    if keys.control {
        flags |= NSEventModifierFlags::Control;
    }
    flags
}

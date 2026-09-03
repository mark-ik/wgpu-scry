// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Producer-internal utility functions: backing-scale lookup,
//! `NSRect` construction in mixed pixels/points, cursor-shape
//! fingerprinting, key-modifier flag translation, JavaScript string
//! escaping, deterministic profile-UUID derivation, and the
//! re-entrancy-aware `pump_until` run-loop pump.

use std::path::Path;
use std::time::Instant;

use dpi::PhysicalSize;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_app_kit::{NSCursor, NSEventModifierFlags, NSView};
use objc2_foundation::{
    MainThreadMarker, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSSize, NSString,
    NSUUID,
};

use crate::CursorShape;

/// Derive a stable [`NSUUID`] from a profile-storage path so that the
/// same `config.data_dir` always resolves to the same
/// [`objc2_web_kit::WKWebsiteDataStore`] across runs.
///
/// Uses FNV-1a 128 over the path's encoded bytes, then formats as a
/// version-8 UUID string (variant bits `10` to satisfy
/// `NSUUID::initWithUUIDString:` parser strictness). Version-8 is the
/// "custom-content" variant — RFC 9562 — which is the right marker for
/// "these bits are not derived from a recognised algorithm" (we're
/// using FNV-1a, not the SHA-1 / SHA-256 the named UUID versions
/// require).
pub(super) fn profile_uuid_for_path(path: &Path, _mtm: MainThreadMarker) -> Retained<NSUUID> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut h = profile_uuid_helpers::FNV1A_128_OFFSET;
    for &b in bytes {
        h ^= b as u128;
        h = h.wrapping_mul(profile_uuid_helpers::FNV1A_128_PRIME);
    }
    let mut out = h.to_be_bytes();
    out[6] = (out[6] & 0x0F) | 0x80;
    out[8] = (out[8] & 0x3F) | 0x80;
    let formatted = format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        out[0],
        out[1],
        out[2],
        out[3],
        out[4],
        out[5],
        out[6],
        out[7],
        out[8],
        out[9],
        out[10],
        out[11],
        out[12],
        out[13],
        out[14],
        out[15],
    );
    let ns_string = NSString::from_str(&formatted);
    NSUUID::initWithUUIDString(NSUUID::alloc(), &ns_string)
        .expect("FNV-1a UUID string should always parse as a valid NSUUID")
}

mod profile_uuid_helpers {
    pub(super) const FNV1A_128_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    pub(super) const FNV1A_128_PRIME: u128 = 0x0000000001000000000000000000013b;
}

/// Backing scale of the screen the parent view is on, falling back to
/// the parent window's `backingScaleFactor`, then to 1.0 for views that
/// haven't been placed in a window yet.
pub(super) fn backing_scale_for(parent_view: &NSView) -> objc2_core_foundation::CGFloat {
    if let Some(window) = parent_view.window() {
        return window.backingScaleFactor();
    }
    1.0
}

pub(super) fn ns_rect_from_pixels(
    offset_pixels: (f32, f32),
    size_pixels: PhysicalSize<u32>,
    backing_scale: objc2_core_foundation::CGFloat,
) -> NSRect {
    // Both offset and size are documented as physical pixels (the
    // unit the trait's `set_offset` / `resize` use). AppKit wants
    // points, so divide by `backing_scale`. Pre-fix: only `size`
    // was divided, while the offset was passed through as-if it
    // were already points — so `with_offset(200, …)` and
    // `set_offset(200, …)` (the latter divides) landed at
    // different positions on Retina.
    let origin = NSPoint::new(
        f64::from(offset_pixels.0) / backing_scale,
        f64::from(offset_pixels.1) / backing_scale,
    );
    let size = NSSize::new(
        f64::from(size_pixels.width) / backing_scale,
        f64::from(size_pixels.height) / backing_scale,
    );
    NSRect::new(origin, size)
}

/// Translate `NSCursor.currentSystemCursor` to a [`CursorShape`].
///
/// macOS exposes built-in cursors as singleton instances —
/// `NSCursor.iBeamCursor()` returns the same object on every call —
/// so we compare retained pointers via `Eq`. Any cursor we don't
/// recognize falls through to [`CursorShape::Default`]; future slices
/// could plumb `image()` + `name` to surface custom cursors via the
/// [`CursorShape::Custom`] variant.
///
/// `currentSystemCursor` and the `resizeUpDown` / `resizeLeftRight`
/// singletons are deprecated in macOS 15+ in favor of
/// per-direction frame-resize variants. The deprecated forms still
/// return the canonical singletons we need for pointer-comparison
/// fingerprinting; switching to the new APIs (which take direction
/// vectors) would force us to enumerate every direction-tuple
/// permutation. Allow the deprecation here.
#[allow(deprecated)]
pub(super) fn current_cursor_shape() -> CursorShape {
    let Some(current) = NSCursor::currentSystemCursor() else {
        return CursorShape::Default;
    };
    let candidates: [(CursorShape, Retained<NSCursor>); 13] = [
        (CursorShape::Default, NSCursor::arrowCursor()),
        (CursorShape::Text, NSCursor::IBeamCursor()),
        (CursorShape::Pointer, NSCursor::pointingHandCursor()),
        (CursorShape::Crosshair, NSCursor::crosshairCursor()),
        (CursorShape::Grab, NSCursor::openHandCursor()),
        (CursorShape::Grabbing, NSCursor::closedHandCursor()),
        (
            CursorShape::NotAllowed,
            NSCursor::operationNotAllowedCursor(),
        ),
        (CursorShape::Help, NSCursor::contextualMenuCursor()),
        (CursorShape::ResizeNs, NSCursor::resizeUpDownCursor()),
        (CursorShape::ResizeEw, NSCursor::resizeLeftRightCursor()),
        (CursorShape::Move, NSCursor::dragCopyCursor()),
        (CursorShape::Pointer, NSCursor::dragLinkCursor()),
        (CursorShape::Wait, NSCursor::disappearingItemCursor()),
    ];
    for (shape, candidate) in &candidates {
        if &*current == candidate.as_ref() {
            return shape.clone();
        }
    }
    CursorShape::Default
}

pub(super) fn key_modifier_flags(keys: crate::KeyModifierFlags) -> NSEventModifierFlags {
    let mut flags = NSEventModifierFlags::empty();
    if keys.shift {
        flags |= NSEventModifierFlags::Shift;
    }
    if keys.control {
        flags |= NSEventModifierFlags::Control;
    }
    if keys.alt {
        flags |= NSEventModifierFlags::Option;
    }
    if keys.meta {
        flags |= NSEventModifierFlags::Command;
    }
    if keys.caps_lock {
        flags |= NSEventModifierFlags::CapsLock;
    }
    flags
}

/// Encode a Rust string as a JSON-style JavaScript string literal,
/// including the surrounding quotes. The output is safe to splice
/// into a JS expression — control characters, quotes, backslashes,
/// and the problematic `U+2028`/`U+2029` line separators are all
/// escaped.
pub(super) fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Pump the main run loop in 16ms slices until `predicate` returns
/// `Some(value)` or `timeout` elapses.
///
/// Returns `Ok(value)` on resolution, `Err(())` on timeout.
///
/// # ⚠️ Re-entrancy hazard
///
/// This function calls `NSRunLoop::runMode_beforeDate` which dispatches
/// AppKit events from the main run loop. If the call site is itself
/// inside a host event-loop callback (winit's `resumed` /
/// `window_event`), AppKit may dispatch a window event that re-enters
/// the host's handler and trips its "no nested event handling" guard
/// → panic. Every public method that ultimately calls `pump_until`
/// is doc-warned; new callers should add the same warning. Prefer
/// completion-block + `Mutex<...>` slot + `poll_*` patterns
/// (e.g. `request_snapshot` / `poll_snapshot`,
/// `start_capture_async` / `capture_status`) for new APIs that need
/// to bridge between async AppKit callbacks and the consumer.
pub(super) fn pump_until<T>(
    timeout: std::time::Duration,
    mut predicate: impl FnMut() -> Option<T>,
) -> Result<T, ()> {
    let start = Instant::now();
    let run_loop = NSRunLoop::currentRunLoop();
    loop {
        if let Some(value) = predicate() {
            return Ok(value);
        }
        if start.elapsed() >= timeout {
            return Err(());
        }
        let until = NSDate::dateWithTimeIntervalSinceNow(0.016);
        let _ = run_loop.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &until);
    }
}

#[cfg(test)]
mod tests {
    use super::js_string_literal;

    #[test]
    fn js_string_literal_escapes_quotes_and_backslashes() {
        assert_eq!(js_string_literal("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn js_string_literal_escapes_short_form_control_chars() {
        assert_eq!(js_string_literal("a\nb\tc\rd"), "\"a\\nb\\tc\\rd\"");
    }

    #[test]
    fn js_string_literal_escapes_generic_control_chars() {
        assert_eq!(js_string_literal("\x07"), "\"\\u0007\"");
    }

    #[test]
    fn js_string_literal_escapes_line_separators() {
        assert_eq!(js_string_literal("\u{2028}"), "\"\\u2028\"");
        assert_eq!(js_string_literal("\u{2029}"), "\"\\u2029\"");
    }

    #[test]
    fn js_string_literal_passes_unicode() {
        assert_eq!(js_string_literal("héllo 🦀"), "\"héllo 🦀\"");
    }
}

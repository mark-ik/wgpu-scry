// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Cursor-shape reporting via `WebKitWebView::mouse-target-changed`.
//!
//! WebKitGTK fires `mouse-target-changed` whenever the hovered DOM
//! element changes, with a `HitTestResult` carrying flags
//! (link/image/media/editable/scrollbar/selection). We map that to
//! the cross-platform [`crate::CursorShape`] enum and stash it on
//! the producer for `poll_cursor_shape` to drain.
//!
//! `GdkWindow::notify::cursor` would surface the engine's chosen
//! GdkCursor more faithfully (resize handles, wait, etc.), but in
//! the offscreen-window setup the WebView's internal GdkWindow
//! cursor doesn't always propagate to our `widget.window()`. The
//! semantic hit-test path is more robust.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use webkit2gtk::{HitTestResultExt, WebView, WebViewExt};

use crate::CursorShape;

use super::producer::WebKitGtkProducer;

// WebKitGTK hit-test context bitmask values (mirrors
// `WebKitHitTestResultContext` in webkit2/WebKitHitTestResult.h).
const HIT_TEST_CONTEXT_DOCUMENT: u32 = 1 << 1;
const HIT_TEST_CONTEXT_LINK: u32 = 1 << 2;
const HIT_TEST_CONTEXT_IMAGE: u32 = 1 << 3;
const HIT_TEST_CONTEXT_MEDIA: u32 = 1 << 4;
const HIT_TEST_CONTEXT_EDITABLE: u32 = 1 << 5;
const HIT_TEST_CONTEXT_SCROLLBAR: u32 = 1 << 6;
const HIT_TEST_CONTEXT_SELECTION: u32 = 1 << 7;

pub(crate) fn install(webview: &WebView, cursor_slot: &Rc<RefCell<Option<CursorShape>>>) {
    let slot = cursor_slot.clone();
    let last = Rc::new(Cell::new(0u32));
    webview.connect_mouse_target_changed(move |_view, hit_test, _modifiers| {
        let context = hit_test.context();
        if context == last.get() {
            return;
        }
        last.set(context);
        let shape = shape_from_hit_test(context);
        *slot.borrow_mut() = Some(shape);
    });
}

impl WebKitGtkProducer {
    /// Pump the GTK main loop until a cursor-shape matching
    /// `predicate` is observed, or `timeout` elapses. Useful for
    /// runtime smokes that need to assert on a specific shape after
    /// a mouse-move; non-blocking hosts should use
    /// [`crate::WebSurfaceProducer::poll_cursor_shape`].
    pub fn wait_for_cursor_shape<F: Fn(&CursorShape) -> bool>(
        &self,
        timeout: Duration,
        predicate: F,
    ) -> Option<CursorShape> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(shape) = self.cursor_shape.borrow_mut().take() {
                if predicate(&shape) {
                    return Some(shape);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            gtk::main_iteration_do(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn shape_from_hit_test(context: u32) -> CursorShape {
    // Precedence order: editable > link > scrollbar > selection >
    // image / media > document. Page-author intent is captured well
    // enough by the most-specific match — these flags don't combine
    // in ways that confuse the precedence.
    if context & HIT_TEST_CONTEXT_EDITABLE != 0 {
        CursorShape::Text
    } else if context & HIT_TEST_CONTEXT_LINK != 0 {
        CursorShape::Pointer
    } else if context & HIT_TEST_CONTEXT_SCROLLBAR != 0 {
        CursorShape::Default
    } else if context & HIT_TEST_CONTEXT_SELECTION != 0 {
        CursorShape::Text
    } else if context & (HIT_TEST_CONTEXT_IMAGE | HIT_TEST_CONTEXT_MEDIA) != 0 {
        CursorShape::Default
    } else if context & HIT_TEST_CONTEXT_DOCUMENT != 0 {
        CursorShape::Default
    } else {
        CursorShape::Default
    }
}

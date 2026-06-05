//! Cursor-shape reporting via `WebKitWebView::mouse-target-changed`.
//!
//! Port of [`crate::webkitgtk_producer::cursor`] adapted to the
//! `webkit6 = "0.6"` + `glib 0.22` (GTK 4 era) binding set. WebKitGTK
//! 6.0 keeps the same `mouse-target-changed` signal + `HitTestResult`
//! context bitmask as the GTK 3 line, so the conceptual surface and
//! precedence mapping transfer verbatim. The auto-generated
//! `connect_mouse_target_changed` accepts a plain
//! `Fn(&WebView, &HitTestResult, u32)` closure — no hand-rolled
//! `RustClosure` plumbing needed (unlike the WPE port, where
//! `HitTestResult` is a hand-bound GObject that doesn't implement
//! `glib::ValueType`).
//!
//! Like the GTK 3 precedent, we de-dup on the raw context bitmask so
//! a stream of mouse-moves over the same DOM element doesn't churn
//! the slot. The translated [`crate::CursorShape`] lands in the
//! producer's `cursor_shape` slot for
//! [`crate::WebSurfaceProducer::poll_cursor_shape`] to drain.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use webkit6::glib;
use webkit6::prelude::*;
use webkit6::{HitTestResult, WebView};

use crate::CursorShape;

use super::producer::WebKit6Producer;

// WebKitGTK hit-test context bitmask values (mirrors
// `WebKitHitTestResultContext` in WebKit2/webkit/WebKitHitTestResult.h).
// Identical across the GTK 3 / GTK 4 / WPE backends of WebKit2 — the
// header bits never moved.
const HIT_TEST_CONTEXT_DOCUMENT: u32 = 1 << 1;
const HIT_TEST_CONTEXT_LINK: u32 = 1 << 2;
const HIT_TEST_CONTEXT_IMAGE: u32 = 1 << 3;
const HIT_TEST_CONTEXT_MEDIA: u32 = 1 << 4;
const HIT_TEST_CONTEXT_EDITABLE: u32 = 1 << 5;
const HIT_TEST_CONTEXT_SCROLLBAR: u32 = 1 << 6;
const HIT_TEST_CONTEXT_SELECTION: u32 = 1 << 7;

/// Connect `mouse-target-changed` on `webview` and route the
/// translated [`CursorShape`] into `cursor_slot`. De-duplicates by
/// remembering the last raw context bitmask.
pub(crate) fn install(webview: &WebView, cursor_slot: &Rc<RefCell<Option<CursorShape>>>) {
    let slot = cursor_slot.clone();
    let last = Rc::new(Cell::new(0u32));
    webview.connect_mouse_target_changed(move |_view, hit_test: &HitTestResult, _modifiers| {
        let context = hit_test.context();
        if context == last.get() {
            return;
        }
        last.set(context);
        let shape = shape_from_hit_test(context);
        *slot.borrow_mut() = Some(shape);
    });
}

impl WebKit6Producer {
    /// Pump the GTK main loop until a cursor-shape matching
    /// `predicate` is observed, or `timeout` elapses. Useful for
    /// runtime smokes that need to assert on a specific shape after
    /// a mouse-move; non-blocking hosts should use
    /// [`crate::WebSurfaceProducer::poll_cursor_shape`].
    ///
    /// GTK 4 removed `gtk_main_iteration_do`; the replacement is
    /// `glib::MainContext::iteration(false)` (same pattern as
    /// [`super::helpers::pump_until`] and
    /// [`Self::wait_for_web_message`]).
    pub fn wait_for_cursor_shape<F: Fn(&CursorShape) -> bool>(
        &self,
        timeout: Duration,
        predicate: F,
    ) -> Option<CursorShape> {
        let ctx = glib::MainContext::default();
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
            ctx.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Map a `WebKitHitTestResultContext` bitmask to a [`CursorShape`].
/// Precedence order: editable > link > scrollbar > selection >
/// image/media > document. Mirrors the GTK 3 + WPE precedents — the
/// WebKit hit-test context bits are identical across backends.
pub(crate) fn shape_from_hit_test(context: u32) -> CursorShape {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_wins_over_link() {
        let ctx = HIT_TEST_CONTEXT_EDITABLE | HIT_TEST_CONTEXT_LINK;
        assert_eq!(shape_from_hit_test(ctx), CursorShape::Text);
    }

    #[test]
    fn link_maps_to_pointer() {
        assert_eq!(
            shape_from_hit_test(HIT_TEST_CONTEXT_LINK),
            CursorShape::Pointer
        );
    }

    #[test]
    fn selection_maps_to_text() {
        assert_eq!(
            shape_from_hit_test(HIT_TEST_CONTEXT_SELECTION),
            CursorShape::Text
        );
    }

    #[test]
    fn bare_document_maps_to_default() {
        assert_eq!(
            shape_from_hit_test(HIT_TEST_CONTEXT_DOCUMENT),
            CursorShape::Default
        );
    }

    #[test]
    fn empty_bitmask_maps_to_default() {
        assert_eq!(shape_from_hit_test(0), CursorShape::Default);
    }

    #[test]
    fn image_maps_to_default_over_document() {
        let ctx = HIT_TEST_CONTEXT_IMAGE | HIT_TEST_CONTEXT_DOCUMENT;
        assert_eq!(shape_from_hit_test(ctx), CursorShape::Default);
    }
}

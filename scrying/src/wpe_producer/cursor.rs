//! Cursor-shape reporting via `WebKitWebView::mouse-target-changed`.
//!
//! Port of `webkitgtk_producer/cursor.rs` — WPEWebKit 2.52.3 exposes
//! the same `mouse-target-changed` signal + `WebKitHitTestResult`
//! context bitmask as WebKitGTK, so the precedent transfers
//! verbatim. We connect the signal on the WebView, extract the
//! context flags, map them through [`shape_from_hit_test`] to the
//! cross-platform [`crate::CursorShape`] enum, and stash the result
//! in the producer's slot for `poll_cursor_shape` to drain.
//!
//! Signal-arg note: `WebKitHitTestResult` is a GObject-derived final
//! type; the `glib::closure_local!` macro panics on non-registered
//! `glib::ValueType` args (see the 4c.3 / 4c.5.a lesson). We follow
//! the `script_message::install` pattern instead — a hand-rolled
//! `RustClosure::new_local` over `&[glib::Value]`, pulling the
//! HitTestResult out as a plain `glib::Object` and casting its raw
//! GObject pointer to `*mut ffi::WebKitHitTestResult` for the
//! `_get_context` call.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::CursorShape;

use super::ffi;
use super::producer::WpeProducer;

// WebKit hit-test context bitmask values — verified against
// /usr/include/wpe-webkit-2.0/wpe/WebKitHitTestResult.h. Identical to
// the WebKitGTK header constants the GTK precedent copies; kept here
// (rather than re-exported from `webkitgtk_producer::cursor`) because
// that module is gated on `target_os = "linux"` + webkit2gtk and we
// don't want a cross-producer dependency.
const HIT_TEST_CONTEXT_DOCUMENT: u32 = 1 << 1;
const HIT_TEST_CONTEXT_LINK: u32 = 1 << 2;
const HIT_TEST_CONTEXT_IMAGE: u32 = 1 << 3;
const HIT_TEST_CONTEXT_MEDIA: u32 = 1 << 4;
const HIT_TEST_CONTEXT_EDITABLE: u32 = 1 << 5;
const HIT_TEST_CONTEXT_SCROLLBAR: u32 = 1 << 6;
const HIT_TEST_CONTEXT_SELECTION: u32 = 1 << 7;

/// Connect `mouse-target-changed` on the WebView and route the
/// translated [`CursorShape`] into `cursor_slot`. De-duplicates by
/// remembering the last raw context bitmask so a stream of moves
/// over the same element doesn't churn the slot.
pub(super) fn install(
    webview: &glib::Object,
    cursor_slot: &Rc<RefCell<Option<CursorShape>>>,
) {
    use glib::prelude::*;
    use glib::translate::ToGlibPtr;

    let slot = cursor_slot.clone();
    let last = Rc::new(Cell::new(0u32));
    let closure = glib::RustClosure::new_local(move |values| {
        // values: [view: GObject, hit_test: WebKitHitTestResult (GObject),
        //          modifiers: guint]
        let Some(hit_value) = values.get(1) else { return None; };
        let raw_hit: *mut ffi::WebKitHitTestResult = match hit_value.get::<glib::Object>() {
            Ok(obj) => {
                ToGlibPtr::<*mut glib::gobject_ffi::GObject>::to_glib_none(&obj).0
                    as *mut ffi::WebKitHitTestResult
            }
            Err(_) => std::ptr::null_mut(),
        };
        if raw_hit.is_null() {
            return None;
        }
        // SAFETY: `raw_hit` is a borrowed (transfer-none) GObject
        // pointer valid for the duration of this closure body;
        // `_get_context` is a pure accessor returning the cached
        // bitmask the hit-test was constructed with.
        let context = unsafe { ffi::webkit_hit_test_result_get_context(raw_hit) };
        if context == last.get() {
            return None;
        }
        last.set(context);
        let shape = shape_from_hit_test(context);
        *slot.borrow_mut() = Some(shape);
        None
    });
    webview.connect_closure("mouse-target-changed", false, closure);
}

impl WpeProducer {
    /// Pump the producer's main context until a cursor shape matching
    /// `predicate` lands in the slot, or `timeout` elapses. Mirrors
    /// the GTK precedent — useful for runtime smokes that need to
    /// assert on a specific shape after a synthesized mouse-move.
    /// Non-blocking hosts should use the trait-level
    /// `poll_cursor_shape` instead.
    #[cfg(feature = "wpe")]
    pub fn wait_for_cursor_shape<F: Fn(&CursorShape) -> bool>(
        &self,
        timeout: Duration,
        predicate: F,
    ) -> Option<CursorShape> {
        let ctx = &self.handles.main_context;
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
/// image/media > document. Mirrors the GTK precedent — the
/// WebKitHitTestResult context bits are identical across GTK and
/// WPE backends of WebKit2.
pub(super) fn shape_from_hit_test(context: u32) -> CursorShape {
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

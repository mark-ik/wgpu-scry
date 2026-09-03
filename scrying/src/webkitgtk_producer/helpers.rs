// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Process-wide GTK initialization gate and main-loop pump helper.

use std::time::{Duration, Instant};

use crate::WebSurfaceError;

/// Initialize GTK if it isn't already. Safe to call from multiple
/// producers — gtk-rs's `init()` is idempotent on the main thread.
///
/// Returns an error if GTK was initialized on a different thread (which
/// would mean the producer is being built off the GTK main thread, and
/// every subsequent webview call would panic).
pub(crate) fn ensure_gtk_init() -> Result<(), WebSurfaceError> {
    if gtk::is_initialized() {
        if !gtk::is_initialized_main_thread() {
            return Err(WebSurfaceError::Platform(
                "GTK is initialized on a different thread; the WebKitGTK producer can only be constructed on the GTK main thread".into(),
            ));
        }
        return Ok(());
    }
    gtk::init().map_err(|e| {
        WebSurfaceError::Platform(format!(
            "gtk::init() failed (no DISPLAY/WAYLAND_DISPLAY?): {e}"
        ))
    })
}

/// Pump pending GTK main-loop events until `cond()` returns true or
/// `deadline` elapses. Sleeps briefly between iterations so an idle
/// wait doesn't peg a CPU core.
///
/// Must be called on the GTK main thread. Callers should already hold
/// the main-context — that's the contract for any producer method that
/// pumps the loop.
pub(crate) fn pump_until(
    deadline: Instant,
    mut cond: impl FnMut() -> bool,
) -> Result<(), WebSurfaceError> {
    while !cond() {
        if Instant::now() >= deadline {
            return Err(WebSurfaceError::NotReady(
                "WebKitGTK main-loop pump deadline exceeded",
            ));
        }
        // main_iteration_do(false) processes any pending event then
        // returns immediately. Return value is "should the main loop
        // quit" (gtk_main_quit was called) — not "did work" — so we
        // ignore it and always nap a moment before re-checking.
        gtk::main_iteration_do(false);
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

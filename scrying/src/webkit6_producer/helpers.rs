// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! GTK 4 init gate + main-loop pump.

use std::time::{Duration, Instant};

use webkit6::glib;

use crate::WebSurfaceError;

/// Ensure GTK 4 is initialized on the current (must-be-main) thread.
/// `gtk4::init()` is idempotent — repeat calls return `Ok(())` once
/// the first one succeeds. Returns an error if GTK can't initialize
/// (no DISPLAY / WAYLAND_DISPLAY, no compositor, etc.).
pub(crate) fn ensure_gtk_init() -> Result<(), WebSurfaceError> {
    if webkit6::gtk::is_initialized() {
        return Ok(());
    }
    webkit6::gtk::init().map_err(|e| {
        WebSurfaceError::Platform(format!(
            "gtk4::init() failed (no DISPLAY/WAYLAND_DISPLAY?): {e}"
        ))
    })
}

/// Pump pending main-loop events until `cond()` returns true or
/// `deadline` elapses. GTK 4 removed `gtk_main_iteration_do` — the
/// replacement is `glib::MainContext::iteration(false)` which
/// processes pending sources non-blocking.
pub(crate) fn pump_until(
    deadline: Instant,
    mut cond: impl FnMut() -> bool,
) -> Result<(), WebSurfaceError> {
    let ctx = glib::MainContext::default();
    while !cond() {
        if Instant::now() >= deadline {
            return Err(WebSurfaceError::NotReady(
                "WebKitGTK 6 main-loop pump deadline exceeded",
            ));
        }
        ctx.iteration(false);
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

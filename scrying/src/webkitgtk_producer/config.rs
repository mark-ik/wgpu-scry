// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Configuration for [`super::WebKitGtkProducer`].

use std::path::PathBuf;
use std::time::Duration;

use dpi::PhysicalSize;

/// Configuration for [`super::WebKitGtkProducer::new`].
///
/// Mirrors the macOS [`crate::wkwebview_producer::WkWebViewProducerConfig`]
/// shape: physical size, host offset (informational here — the producer
/// renders offscreen and does not embed in a host container), data
/// directory for the `WebsiteDataManager`, and bounded timeouts for the
/// blocking navigate / snapshot paths.
#[derive(Clone, Debug)]
pub struct WebKitGtkProducerConfig {
    /// Initial size of the offscreen WebView render area, in physical pixels.
    pub size: PhysicalSize<u32>,
    /// Offset reported to the host through `set_offset`. Informational
    /// only with the offscreen capture path — preserved for parity with
    /// the Windows / macOS configs so cross-platform host code can stay
    /// uniform.
    pub offset: (f32, f32),
    /// Directory used as the `WebsiteDataManager`'s base data directory.
    /// Created if missing.
    pub data_dir: PathBuf,
    /// Timeout for navigation-completion waits in `navigate_to_*` and
    /// `wait_for_load`.
    pub navigation_timeout: Duration,
    /// Timeout for blocking snapshot acquires.
    pub frame_timeout: Duration,
}

impl WebKitGtkProducerConfig {
    pub fn new(size: PhysicalSize<u32>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            size,
            offset: (0.0, 0.0),
            data_dir: data_dir.into(),
            navigation_timeout: Duration::from_secs(5),
            frame_timeout: Duration::from_secs(2),
        }
    }

    pub fn with_offset(mut self, x: f32, y: f32) -> Self {
        self.offset = (x, y);
        self
    }
}

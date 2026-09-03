// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Configuration for [`super::WebKit6Producer`].

use std::path::PathBuf;
use std::time::Duration;

use dpi::PhysicalSize;

/// Configuration for [`super::WebKit6Producer::new`]. Same shape as
/// the GTK 3 producer's
/// [`crate::webkitgtk_producer::WebKitGtkProducerConfig`] —
/// physical size, offset (informational on the offscreen path),
/// data directory, and bounded timeouts for blocking helpers.
#[derive(Clone, Debug)]
pub struct WebKit6ProducerConfig {
    pub size: PhysicalSize<u32>,
    pub offset: (f32, f32),
    pub data_dir: PathBuf,
    pub navigation_timeout: Duration,
    pub frame_timeout: Duration,
}

impl WebKit6ProducerConfig {
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

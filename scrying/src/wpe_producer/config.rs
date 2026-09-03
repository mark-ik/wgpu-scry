// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

use std::path::PathBuf;

use dpi::PhysicalSize;

/// Configuration for [`WpeProducer`].
#[derive(Clone, Debug)]
pub struct WpeProducerConfig {
    /// Initial view size in physical pixels.
    pub size: PhysicalSize<u32>,
    /// Offset of the embedded view relative to the host surface, in
    /// device-independent pixels.
    pub offset: (f32, f32),
    /// Directory used for WebKit website data.
    pub data_dir: PathBuf,
    /// Timeout for blocking navigation helpers.
    pub navigation_timeout: std::time::Duration,
    /// Timeout for blocking first-frame helpers.
    pub frame_timeout: std::time::Duration,
}

impl WpeProducerConfig {
    pub fn new(size: PhysicalSize<u32>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            size,
            offset: (0.0, 0.0),
            data_dir: data_dir.into(),
            navigation_timeout: std::time::Duration::from_secs(5),
            frame_timeout: std::time::Duration::from_secs(2),
        }
    }

    pub fn with_offset(mut self, x: f32, y: f32) -> Self {
        self.offset = (x, y);
        self
    }
}

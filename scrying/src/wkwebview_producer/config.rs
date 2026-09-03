// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Configuration struct for [`super::WkWebViewProducer::new`] /
//! [`super::WkWebViewProducer::new_with_url_schemes`]. Mirrors the shape
//! of `WebView2CompositionConfig` on Windows so consumers can write
//! cross-platform setup with minimal cfg-gating.
//!
//! **Minimum macOS: 14.0 (Sonoma).** `data_dir` resolves through
//! `WKWebsiteDataStore::dataStoreForIdentifier:` (macOS 14+);
//! `setInspectable` (macOS 13.3+) wires `apply_settings`'s
//! `devtools_enabled`; ScreenCaptureKit (macOS 12.3+) backs the
//! capture pipeline. The producer makes no runtime-availability
//! checks — older OS versions are unsupported.

use std::path::PathBuf;

use dpi::PhysicalSize;

use crate::ColorPipeline;

#[derive(Clone, Debug)]
pub struct WkWebViewProducerConfig {
    /// Initial size of the WKWebView frame and the capture region, in
    /// physical pixels.
    pub size: PhysicalSize<u32>,
    /// Offset of the WKWebView relative to the parent NSView, in
    /// **physical pixels** (matches the trait's `set_offset`
    /// contract and matches `size`'s units). The producer divides
    /// by `backing_scale` to convert to AppKit points internally.
    pub offset: (f32, f32),
    /// Directory used as `WKWebsiteDataStore`'s persistent storage.
    /// Hashed into a deterministic UUID and resolved via
    /// `WKWebsiteDataStore::dataStoreForIdentifier:` (macOS 14+).
    /// Empty path falls back to the shared default store. Ignored
    /// when [`Self::non_persistent`] is `true`.
    pub data_dir: PathBuf,
    /// When `true`, use `WKWebsiteDataStore::nonPersistentDataStore`
    /// — cookies / local storage / IndexedDB live only for the
    /// lifetime of the producer and are wiped on `Drop`. The
    /// "incognito tab" / "private window" mode for browser-shape
    /// consumers. Mutually exclusive with [`Self::data_dir`]; when
    /// both are provided, `non_persistent` wins.
    pub non_persistent: bool,
    /// Timeout for `navigate_to_string`, mirroring the Windows
    /// producer's navigation completion wait.
    pub navigation_timeout: std::time::Duration,
    /// Timeout for the initial frame after `start_capture`. Mirrors the
    /// Windows producer's first-frame block.
    pub frame_timeout: std::time::Duration,
    /// Directory where WebKit-managed downloads are written. Each
    /// download lands at `<download_dir>/<suggested_filename>` (with
    /// numeric suffixes appended on collision).
    pub download_dir: PathBuf,
    /// Color pipeline for the SCK capture path. `Srgb` keeps the
    /// historical default (BGRA8 sRGB tone-mapped); `DisplayP3`
    /// switches SCK's `colorSpaceName` to
    /// `kCGColorSpaceDisplayP3` so wider-gamut page content
    /// (`color(display-p3 …)`, P3-tagged images) survives the
    /// capture round-trip. Stored on the config so consumers can
    /// pick at producer-construction time;
    /// `WkWebViewProducer::set_color_pipeline` flips it live.
    pub color_pipeline: ColorPipeline,
    /// Override the page's `spellcheck` attribute on
    /// `<input>` / `<textarea>` / `[contenteditable]` elements.
    /// `None` leaves the page's own default in place; `Some(true)`
    /// forces spell-checking on, `Some(false)` forces it off.
    ///
    /// This is a *best-effort* knob: WKWebView has no public-API
    /// engine-level spellcheck toggle. The producer injects a
    /// document-start user-script that walks editable elements
    /// and sets `spellcheck="true|false"`, plus a
    /// `MutationObserver` to catch nodes added later. Pages that
    /// dynamically rewrite the attribute themselves can win the
    /// last-write race; pages that respect the attribute (the
    /// vast majority) honor the host's choice.
    pub spellcheck_override: Option<bool>,
}

impl WkWebViewProducerConfig {
    pub fn new(size: PhysicalSize<u32>, data_dir: impl Into<PathBuf>) -> Self {
        let data_dir: PathBuf = data_dir.into();
        let download_dir = data_dir.join("downloads");
        Self {
            size,
            offset: (0.0, 0.0),
            data_dir,
            navigation_timeout: std::time::Duration::from_secs(5),
            frame_timeout: std::time::Duration::from_secs(2),
            download_dir,
            non_persistent: false,
            color_pipeline: ColorPipeline::Srgb,
            spellcheck_override: None,
        }
    }

    /// Switch this config into incognito / non-persistent mode.
    /// Equivalent to setting `non_persistent = true`. Cookie /
    /// local-storage / IndexedDB activity for this producer doesn't
    /// touch any persistent store and is wiped on `Drop`.
    pub fn non_persistent(mut self) -> Self {
        self.non_persistent = true;
        self
    }

    pub fn with_offset(mut self, x: f32, y: f32) -> Self {
        self.offset = (x, y);
        self
    }
}

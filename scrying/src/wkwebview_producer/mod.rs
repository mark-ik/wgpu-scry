// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! macOS WKWebView capture producer.
//!
//! This is the macOS counterpart to
//! [`crate::webview2_composition_producer::WebView2CompositionProducer`].
//! The shape mirrors the Windows producer so consumers can program
//! against a single trait surface (`WebSurfaceProducer`); the
//! *internals* are entirely different because macOS has no public
//! composition-capture API directly analogous to
//! `Windows.Graphics.Capture::CreateFromVisual`.
//!
//! ## Capture options on macOS
//!
//! 1. **`WKWebView.takeSnapshot(...)` → CPU pixels.**
//!    Public, simple, returns an `NSImage`. One-shot per call: each
//!    invocation schedules a fresh render pass. Latency is high
//!    (typically >50ms) and rate is well below display refresh, so this
//!    is a `CpuSnapshot`-tier capability — useful for thumbnails and
//!    offscreen layout inspection, not for an interactive composited
//!    surface.
//!
//! 2. **`ScreenCaptureKit` (macOS 12.3+) → `IOSurfaceRef` →
//!    `MTLTexture`.** The closest analog to `Windows.Graphics.Capture`.
//!    Bind an `SCContentFilter` to either the `NSWindow` hosting the
//!    `WKWebView` or directly to the WKWebView's underlying `CALayer`,
//!    configure an `SCStreamConfiguration` for `BGRA8Unorm`, and stream
//!    frames via `SCStreamOutput`. Each `CMSampleBuffer` carries a
//!    `CVPixelBuffer` whose backing `IOSurfaceRef` maps to a Metal
//!    texture via `MTLDevice::newTextureWithDescriptor:iosurface:plane:`.
//!    This is the intended `ImportedTexture` path. Requires the
//!    "Screen Recording" privacy permission to be granted to the host
//!    binary on first use.
//!
//! 3. **Direct `CALayer` contents observation (private SPI).** WKWebView
//!    is layer-backed; the web-content compositing layer ultimately
//!    holds an `IOSurface`. Reaching it requires SPI / undocumented
//!    interfaces (`-_swapChain`, `-WKLayerHostView`, etc.), is fragile
//!    across macOS versions, and would not be acceptable in App Store
//!    builds. Worth knowing as an emergency hatch but not the canonical
//!    path.
//!
//! ## Module layout
//!
//! The producer lives across several siblings under
//! `wkwebview_producer/`:
//!
//! - [`producer`] — the [`WkWebViewProducer`] struct, its
//!   constructors (`new` / `new_with_url_schemes`), `Drop`, and
//!   shared internal helpers (DPI flush, internal resize, cursor
//!   observation, navigation-completion wait).
//! - [`config`] — [`WkWebViewProducerConfig`].
//! - [`capture`] — the ScreenCaptureKit pipeline: lifecycle methods
//!   (`start_capture` / `start_capture_async` / `capture_status` /
//!   `stop_capture`), the `try_acquire_frame` non-blocking pull,
//!   internal SCK delegate classes, and shared state types.
//! - [`api`] — non-trait, non-capture inherent public methods:
//!   non-blocking loads, snapshot/PDF/find rendering, cookie-store
//!   API, auth/permission handlers, interaction-state round-trip.
//! - [`trait_impl`] — the [`crate::WebSurfaceProducer`] trait
//!   implementation.
//! - [`nav_delegate`] / [`ui_delegate`] / [`title_observer`] /
//!   [`download_handler`] / [`scheme_handler`] / [`script_message`]
//!   — Objective-C delegate classes wired to the WKWebView,
//!   shared-state structs (`NavState`), and the public handler
//!   typedefs.
//! - [`cookies`] / [`input`] / [`helpers`] — pure translation
//!   functions (cookie ↔ NSHTTPCookie, MouseInput → NSEvent, run-loop
//!   pump, JS string escape, FNV-derived profile UUID, etc.).
//!
//! Each submodule is held to a <600-line ceiling so structural
//! concerns stay decoupled and the producer remains amenable to
//! per-aspect review.

#![cfg(target_os = "macos")]

mod api;
mod capture;
mod config;
mod cookie_observer;
mod cookies;
mod download_handler;
mod helpers;
mod input;
mod nav_delegate;
mod producer;
mod scheme_handler;
mod script_message;
mod snapshots;
mod title_observer;
mod trait_impl;
mod ui_delegate;

pub use crate::{UrlSchemeHandlerFn, UrlSchemeResponse};
pub use api::FindOptions;
pub use capture::{CaptureMetrics, CaptureStatus};
pub use config::WkWebViewProducerConfig;
pub use cookie_observer::CookieChangeHandlerFn;
pub use download_handler::DownloadHandlerFn;
pub use nav_delegate::AuthHandlerFn;
pub use producer::{CursorHandlerFn, WkWebViewProducer};
pub use ui_delegate::PermissionHandlerFn;

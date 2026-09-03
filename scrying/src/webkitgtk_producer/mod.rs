// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Linux WebKitGTK producer.
//!
//! This is one of the two co-equal Linux backends — pick it via the
//! `webkitgtk-fallback` cargo feature when WebKitGTK is the engine
//! the host has available (Fedora, most LTS distros) and the WPE
//! producer (see [`crate::wpe_producer`]) when WPE is.
//!
//! ## Architecture
//!
//! The producer hosts a `WebKitWebView` inside a `GtkOffscreenWindow`
//! that it owns itself. The host (winit/wgpu) never sees a GTK widget —
//! WebKitGTK renders into the offscreen window's backing surface and the
//! producer pulls frames out via WebKit's snapshot API. This lets the
//! producer slot into a non-GTK host (winit on Wayland or X11) without
//! the host needing to participate in the GTK widget hierarchy.
//!
//! ## Capture path (today: CPU snapshot)
//!
//! [`WebKitGtkProducer::capture_cpu_snapshot`] / `acquire_frame` call
//! `webkit_web_view_get_snapshot` → `cairo::ImageSurface` (ARGB32) →
//! un-premultiplied RGBA bytes → [`crate::WebSurfaceFrame::CpuRgba`].
//! This is honest about the latency/throughput tier (≪ display refresh,
//! >50ms per frame) — suitable for thumbnails, previews, and runtime
//! verification, not for a live interactive composited surface.
//!
//! A future GPU capture path (WebKitGTK 2.46+'s DMABUF renderer, or
//! wlroots `zwlr_screencopy_manager_v1`) would upgrade `ImportedTexture`
//! support — gated by compositor / WebKitGTK version checks.
//!
//! ## GDK GL-context caveats
//!
//! WebKitGTK 2.40+ runs the page through an accelerated-compositing GL
//! path by default. On some GTK 3 + Wayland sessions GDK can fail to
//! create a GL context (`GDK is not able to create a GL context: The
//! current backend does not support OpenGL`), aborting the host process
//! before the first snapshot. The CPU snapshot path doesn't actually
//! need AC, so hosts that want a robust CPU-only pipeline can set
//! `WEBKIT_DISABLE_DMABUF_RENDERER=1` and `WEBKIT_DISABLE_COMPOSITING_MODE=1`
//! before constructing the producer (these are process-wide WebKit
//! environment variables — `demo-linux` does exactly this in `main`).
//! Hosts that *do* need accelerated compositing (e.g. for a future
//! DMABUF capture path) must instead make sure their target session
//! can create a GL context.
//!
//! ## Module layout
//!
//! - [`config`] — [`WebKitGtkProducerConfig`].
//! - [`producer`] — [`WebKitGtkProducer`] struct and construction.
//! - [`navigation`] — `load_uri` / `load_html` with main-loop-pumped
//!   completion waits.
//! - [`capture`] — `webkit_web_view_get_snapshot` → `CpuRgba`.
//! - [`helpers`] — GTK initialization gate and main-loop pump.
//! - [`trait_impl`] — [`crate::WebSurfaceProducer`] implementation.

#![cfg(all(target_os = "linux", feature = "webkitgtk-fallback"))]

mod capture;
mod config;
mod cookies;
mod cursor;
mod downloads;
mod helpers;
mod ime;
mod input;
mod input_native;
mod navigation;
mod producer;
mod scheme_handler;
mod script_message;
mod trait_impl;

use crate::native_frame::{CapabilityStatus, UnsupportedReason};
use crate::{SystemWebviewBackend, WebSurfaceCapabilities, WebSurfaceMode};

pub use config::WebKitGtkProducerConfig;
pub use producer::WebKitGtkProducer;

/// Capabilities reported when the WebKitGTK producer is selected.
///
/// `preferred_mode = CpuSnapshot` reflects the current capture path
/// (`webkit_web_view_get_snapshot`). When a DMABUF / screencopy path
/// lands this will upgrade to `ImportedTexture`.
pub(crate) fn linux_webkitgtk_capabilities() -> WebSurfaceCapabilities {
    WebSurfaceCapabilities {
        backend: SystemWebviewBackend::WebKitGtk,
        preferred_mode: WebSurfaceMode::CpuSnapshot,
        imported_texture: CapabilityStatus::Unsupported(
            UnsupportedReason::NativeImportNotYetImplemented,
        ),
        native_child_overlay: CapabilityStatus::Unsupported(
            UnsupportedReason::PlatformNotImplemented,
        ),
        cpu_snapshot: CapabilityStatus::Supported,
        supported_frames: Vec::new(),
        reason: "WebKitGTK producer hosts an offscreen WebKitWebView and emits CpuRgba snapshots via webkit_web_view_get_snapshot. Native GPU texture import (DMABUF / wlroots screencopy) is future work; native overlay is structurally out-of-scope for the offscreen-rendering shape.",
    }
}

// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! CPU snapshot capture via `webkit_web_view_get_snapshot`.
//!
//! Returns a [`crate::WebSurfaceFrame::CpuRgba`] suitable for thumbnails
//! and runtime verification. Latency is engine-bound (typically ≥50 ms
//! per frame), so this is honest about its tier: not a live composited
//! capture path.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use dpi::PhysicalSize;
use gtk::cairo::{Format, ImageSurface};
use webkit2gtk::gio;
use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};

use crate::{WebSurfaceError, WebSurfaceFrame};

use super::helpers::pump_until;
use super::producer::WebKitGtkProducer;

impl WebKitGtkProducer {
    /// Take a CPU RGBA snapshot of the current document.
    ///
    /// Drives `webkit_web_view_get_snapshot` to completion by pumping
    /// the GTK main loop. The configured `frame_timeout` bounds the
    /// wait.
    pub fn capture_cpu_snapshot(&self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        let timeout = self.frame_timeout;
        let result: Rc<RefCell<Option<Result<ImageSurface, String>>>> = Rc::new(RefCell::new(None));
        let r = result.clone();
        self.webview.snapshot(
            SnapshotRegion::Visible,
            SnapshotOptions::empty(),
            gio::Cancellable::NONE,
            move |res| {
                let translated = match res {
                    Ok(surface) => ImageSurface::try_from(surface)
                        .map_err(|_| "snapshot returned a non-ImageSurface".to_string()),
                    Err(e) => Err(e.to_string()),
                };
                *r.borrow_mut() = Some(translated);
            },
        );

        let deadline = Instant::now() + timeout;
        pump_until(deadline, || result.borrow().is_some())?;
        let res = result.borrow_mut().take().ok_or(WebSurfaceError::NotReady(
            "WebKitGTK snapshot did not deliver in time",
        ))?;
        let mut surface =
            res.map_err(|e| WebSurfaceError::Platform(format!("snapshot failed: {e}")))?;
        if surface.format() != Format::ARgb32 {
            return Err(WebSurfaceError::Platform(format!(
                "snapshot returned unexpected cairo format: {:?}",
                surface.format()
            )));
        }

        let width = surface.width().max(0) as u32;
        let height = surface.height().max(0) as u32;
        let stride = surface.stride().max(0) as usize;
        let data = surface
            .data()
            .map_err(|e| WebSurfaceError::Platform(format!("borrow snapshot pixel data: {e}")))?;

        let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for y in 0..height as usize {
            let row_start = y * stride;
            for x in 0..width as usize {
                let px = row_start + x * 4;
                // Cairo ARGB32 on little-endian: bytes in memory are
                // [B, G, R, A], components premultiplied.
                let b = data[px] as u32;
                let g = data[px + 1] as u32;
                let r = data[px + 2] as u32;
                let a = data[px + 3] as u32;
                let (r8, g8, b8) = if a == 0 {
                    (0u8, 0u8, 0u8)
                } else if a == 255 {
                    (r as u8, g as u8, b as u8)
                } else {
                    (
                        unpremultiply(r, a),
                        unpremultiply(g, a),
                        unpremultiply(b, a),
                    )
                };
                rgba.extend_from_slice(&[r8, g8, b8, a as u8]);
            }
        }
        drop(data);

        let pixels = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
            WebSurfaceError::Platform("failed to construct RgbaImage from snapshot bytes".into())
        })?;
        Ok(WebSurfaceFrame::CpuRgba {
            size: PhysicalSize::new(width, height),
            pixels,
            generation: self.next_generation(),
        })
    }
}

#[inline]
fn unpremultiply(channel: u32, alpha: u32) -> u8 {
    // Round-to-nearest unpremultiply.
    (((channel * 255) + (alpha / 2)) / alpha).min(255) as u8
}

impl WebKitGtkProducer {
    /// Take a snapshot and encode it as a PNG, returning the bytes.
    ///
    /// Wraps [`Self::capture_cpu_snapshot`] + `image::ImageFormat::Png`
    /// encoding. Useful for thumbnails / previews where the host wants
    /// to ship bytes around (across an IPC boundary, into a cache,
    /// etc.) rather than handing an in-memory `RgbaImage`.
    pub fn capture_snapshot_png(&self) -> Result<Vec<u8>, WebSurfaceError> {
        let frame = self.capture_cpu_snapshot()?;
        let pixels = match frame {
            WebSurfaceFrame::CpuRgba { pixels, .. } => pixels,
            _ => {
                return Err(WebSurfaceError::Platform(
                    "capture_cpu_snapshot returned an unexpected frame variant".into(),
                ));
            }
        };
        let mut buf: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        pixels
            .write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| WebSurfaceError::Platform(format!("PNG encode failed: {e}")))?;
        Ok(buf)
    }
}

//! WebKitGTK 6.0 frame capture.
//!
//! ## Paths available in this module (in priority order)
//!
//! 1. **`capture_native`** — Phase A.8 exploration. Walks the
//!    `GtkWidgetPaintable` → `gtk4::Snapshot` → `GskRenderNode` →
//!    `GskRenderer::render_texture(...)` chain to produce a `GdkTexture`,
//!    then type-checks the result against `GdkDmabufTexture`. The
//!    [`GdkMemoryFormat`] is read via `GdkTextureDownloader`, the bytes
//!    are downloaded, and (because GTK does not expose plane fds — see
//!    the "Empirical outcome" note below) the path returns
//!    [`WebSurfaceFrame::CpuRgba`] with format-aware un-premultiplication.
//!    This replaces the older CPU snapshot in the producer's
//!    `acquire_frame` path.
//!
//! 2. **`capture_cpu_snapshot`** — legacy fallback. Uses
//!    `webkit_web_view_get_snapshot` → `gdk::Texture` → `download(...)`
//!    which always returns Cairo-ARGB32-format pixels (BGRA premultiplied
//!    on little-endian). Kept as the final-resort fallback if the
//!    paintable-render path fails (e.g., the widget has no realized
//!    renderer because the window hasn't mapped yet).
//!
//! ## Empirical outcome — Phase A.8 (Outcome B/C, mixed)
//!
//! GTK 4.22 / gdk4 0.11 / webkit6 0.6 on Fedora 44, Wayland + Mesa:
//!
//! - The paintable-snapshot path is wired and works at runtime: it
//!   produces a `GdkTexture` we can type-check and download.
//! - **The texture's concrete class can be detected via the
//!   `GDK_IS_DMABUF_TEXTURE` cast macro** (exposed in Rust as
//!   `gdk4::DmabufTexture` plus a `glib::Cast::downcast_ref`), and on
//!   stacks where GTK's renderer is wired to DMABUF output we WOULD see
//!   the cast succeed.
//! - **However**, even when the cast succeeds, GTK 4 (through current
//!   stable 4.22) **does not expose any public C API to extract plane
//!   fds / fourcc / modifier / offset / stride from a `GdkDmabufTexture`.**
//!   The C header `<gdk/gdkdmabuftexture.h>` declares only
//!   `gdk_dmabuf_texture_get_type()`; the inverse direction lives only
//!   on `GdkDmabufTextureBuilder` (used by *producers* like GStreamer
//!   plugins handing a DMABUF *to* GTK for display). There is no
//!   `gdk_dmabuf_texture_get_fd` / `_get_fourcc` / `_get_modifier`
//!   anywhere — confirmed against the installed
//!   `/usr/lib64/libgtk-4.so` symbol table.
//!
//! Net result: even if the WebKit GPU process delivers a DMABUF-backed
//! `GdkTexture` to our WebView, **we cannot turn it into a
//! `DmaBufImage` from the gtk-rs side**. The shape we'd need to ship
//! `WebSurfaceFrame::Native(NativeFrame::DmaBufImage(...))` is not
//! available without either:
//!
//! - a GTK upstream PR adding accessors (or a GIR-private FFI peek into
//!   the `GdkDmabufTexture` struct layout — fragile, version-locked,
//!   undefined behaviour by the GTK team's own contract); or
//! - tapping the WebKit GPU-process buffer pipeline *before* GTK wraps
//!   it (the `GstWebKit` / web-process WPE-shaped private hook the
//!   strategy doc references for future work).
//!
//! For Phase A.8 we ship the exploration: the paintable-render path is
//! wired, the type-probe runs, and on Phase A.8's empirical outcome we
//! log whether the texture *is* DMABUF-backed even though we have to
//! fall through to a CPU download to actually deliver pixels. The
//! downloaded path is `GdkMemoryFormat`-aware so we no longer assume
//! Cairo-ARGB32: it handles the renderer's actual output format
//! (typically `B8g8r8a8Premultiplied` or `R8g8b8a8Premultiplied` for
//! the GL renderer; `B8g8r8a8` un-premultiplied on the Vulkan renderer
//! on some configurations).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use dpi::PhysicalSize;
use webkit6::gdk;
use webkit6::gdk::prelude::*;
use webkit6::gio;
use webkit6::glib::object::Cast;
use webkit6::gtk;
use webkit6::gtk::prelude::*;
use webkit6::prelude::*;
use webkit6::{SnapshotOptions, SnapshotRegion};

use crate::{WebSurfaceError, WebSurfaceFrame};

use super::helpers::pump_until;
use super::producer::WebKit6Producer;

/// Outcome of the paintable-render type probe in [`render_widget_to_texture`].
/// Logged once per call so a runtime smoke shows whether GTK is handing us a
/// DMABUF-backed texture (even though we can't extract its plane info).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextureBacking {
    /// `GDK_IS_DMABUF_TEXTURE(texture)` passed — GTK rendered into a DMABUF.
    /// Phase A.8 cannot harvest plane fds from this (see module docs); the
    /// path falls through to a CPU download.
    Dmabuf,
    /// `GdkMemoryTexture` or other CPU-side concrete class. CPU download
    /// path is the natural fit.
    Memory,
    /// `GdkGLTexture` or another non-DMABUF GPU class. Same fallback as
    /// `Memory` — the downloader does the GPU readback.
    OtherGpu,
}

impl WebKit6Producer {
    /// Phase A.8 capture entry point. Tries the paintable-render path
    /// first (which lets us inspect the renderer's texture class), then
    /// falls back to `webkit_web_view_get_snapshot` if that path fails.
    ///
    /// Always returns [`WebSurfaceFrame::CpuRgba`] today — see the
    /// module-level "Empirical outcome" note. The signature is shaped so
    /// future work can add a `WebSurfaceFrame::Native(...)` arm once a
    /// real DMABUF tap becomes available.
    pub fn capture_native(&self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        match self.capture_via_paintable_render() {
            Ok(frame) => Ok(frame),
            Err(primary_err) => {
                // Surface the paintable-path error in the fallback's
                // error chain so a runtime trace of "why did we fall
                // back?" is recoverable. The legacy snapshot path is
                // the documented final-resort.
                self.capture_cpu_snapshot().map_err(|fallback_err| {
                    WebSurfaceError::Platform(format!(
                        "paintable-render path failed ({primary_err}); \
                         webkit snapshot fallback also failed: {fallback_err}"
                    ))
                })
            }
        }
    }

    /// Paintable → snapshot → render node → renderer.render_texture →
    /// type-probe → downloader. Returns the texture's empirical backing
    /// alongside the produced frame so the caller can route based on it.
    fn capture_via_paintable_render(&self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        let (texture, backing) = render_widget_to_texture(&self.webview)?;
        // Once we add a `Native(DmaBufImage)` arm here, this is where
        // it would branch on `backing == TextureBacking::Dmabuf` —
        // today every backing falls through to the format-aware CPU
        // download. Mention the observed backing in the debug log so a
        // smoke trace shows which GTK class we got back.
        eprintln!(
            "scrying webkit6 capture_native: paintable-render texture backing = {:?} \
             (DMABUF→CpuRgba fallthrough is intentional; see capture.rs)",
            backing
        );

        let width = texture.width().max(0) as u32;
        let height = texture.height().max(0) as u32;
        if width == 0 || height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "paintable-render produced empty texture {width}x{height}"
            )));
        }

        let downloader = gdk::TextureDownloader::new(&texture);
        let format = downloader.format();
        let (bytes, stride) = downloader.download_bytes();
        let rgba = decode_memory_format(&bytes, stride, width, height, format).ok_or_else(|| {
            WebSurfaceError::Platform(format!(
                "GdkMemoryFormat {format:?} not handled by webkit6 decode path"
            ))
        })?;

        let pixels = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
            WebSurfaceError::Platform("failed to construct RgbaImage from downloaded bytes".into())
        })?;
        Ok(WebSurfaceFrame::CpuRgba {
            size: PhysicalSize::new(width, height),
            pixels,
            generation: self.next_generation(),
        })
    }

    pub fn capture_cpu_snapshot(&self) -> Result<WebSurfaceFrame, WebSurfaceError> {
        let timeout = std::time::Duration::from_secs(2);
        let result: Rc<RefCell<Option<Result<gdk::Texture, String>>>> = Rc::new(RefCell::new(None));
        let r = result.clone();
        self.webview.snapshot(
            SnapshotRegion::Visible,
            SnapshotOptions::empty(),
            gio::Cancellable::NONE,
            move |res| {
                *r.borrow_mut() = Some(res.map_err(|e| e.to_string()));
            },
        );

        let deadline = Instant::now() + timeout;
        pump_until(deadline, || result.borrow().is_some())?;
        let texture = result
            .borrow_mut()
            .take()
            .ok_or(WebSurfaceError::NotReady(
                "WebKitGTK 6 snapshot did not deliver in time",
            ))?
            .map_err(|e| WebSurfaceError::Platform(format!("snapshot failed: {e}")))?;

        let width = texture.width().max(0) as u32;
        let height = texture.height().max(0) as u32;
        let stride = (width as usize) * 4;
        let mut buf = vec![0u8; stride * (height as usize)];
        // `Texture::download` writes Cairo-ARGB32 format — BGRA
        // premultiplied on little-endian, identical to the GTK 3
        // producer's `ImageSurface::data()`.
        texture.download(&mut buf, stride);

        let rgba = decode_bgra_premultiplied(&buf, stride, width, height);

        let pixels = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
            WebSurfaceError::Platform("failed to construct RgbaImage from snapshot bytes".into())
        })?;
        Ok(WebSurfaceFrame::CpuRgba {
            size: PhysicalSize::new(width, height),
            pixels,
            generation: self.next_generation(),
        })
    }

    /// Take a snapshot and encode it as a PNG.
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

/// Build a `GtkWidgetPaintable` for the WebView, snapshot it into a
/// GTK `Snapshot`, convert to a `GskRenderNode`, and render that node
/// through the widget's `GskRenderer` to produce a `GdkTexture`. The
/// resulting texture is then probed against `GdkDmabufTexture` so the
/// caller knows the renderer's empirical output class.
///
/// Errors when:
/// - the widget has no realized `GtkNative` (window not yet mapped),
/// - the native has no `GskRenderer` realized,
/// - the paintable has zero intrinsic size (widget hasn't been allocated yet),
/// - the snapshot produces no render node (empty widget tree).
fn render_widget_to_texture(
    webview: &webkit6::WebView,
) -> Result<(gdk::Texture, TextureBacking), WebSurfaceError> {
    // Width/height — prefer the widget's allocation, fall back to the
    // paintable's intrinsic size.
    // GTK 4.12+ replaced `allocated_width`/`_height` with the
    // unprefixed accessors that account for transforms.
    let alloc_w = WidgetExt::width(webview);
    let alloc_h = WidgetExt::height(webview);

    let paintable = gtk::WidgetPaintable::new(Some(webview));
    let intrinsic_w = paintable.intrinsic_width();
    let intrinsic_h = paintable.intrinsic_height();
    let width = if alloc_w > 0 { alloc_w } else { intrinsic_w };
    let height = if alloc_h > 0 { alloc_h } else { intrinsic_h };
    if width <= 0 || height <= 0 {
        return Err(WebSurfaceError::NotReady(
            "widget has no allocation or intrinsic size yet (paintable-render path)",
        ));
    }

    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width as f64, height as f64);
    let node = snapshot
        .to_node()
        .ok_or(WebSurfaceError::NotReady(
            "paintable snapshot produced no render node (widget tree empty?)",
        ))?;

    // The renderer lives on the widget's GtkNative (its enclosing
    // GdkSurface). For our hidden top-level Window, that's the window
    // itself.
    let native = webview.native().ok_or(WebSurfaceError::NotReady(
        "WebView has no GtkNative yet (window not mapped?)",
    ))?;
    let renderer = native.renderer().ok_or(WebSurfaceError::NotReady(
        "GtkNative has no realized GskRenderer yet",
    ))?;

    let texture = renderer.render_texture(&node, None);

    let backing = if texture.downcast_ref::<gdk::DmabufTexture>().is_some() {
        TextureBacking::Dmabuf
    } else if texture.downcast_ref::<gdk::MemoryTexture>().is_some() {
        TextureBacking::Memory
    } else {
        TextureBacking::OtherGpu
    };

    Ok((texture, backing))
}

/// Convert a downloaded buffer of `format` into un-premultiplied RGBA8.
/// Returns `None` for memory formats we don't have a conversion for
/// (uncommon HDR / 16-bit / >RGBA cases — these would warrant their own
/// path in a follow-up).
fn decode_memory_format(
    buf: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    format: gdk::MemoryFormat,
) -> Option<Vec<u8>> {
    use gdk::MemoryFormat as F;
    match format {
        F::B8g8r8a8Premultiplied => Some(decode_bgra_premultiplied(buf, stride, width, height)),
        F::R8g8b8a8Premultiplied => Some(decode_rgba_premultiplied(buf, stride, width, height)),
        F::B8g8r8a8 => Some(decode_bgra_unpremultiplied(buf, stride, width, height)),
        F::R8g8b8a8 => Some(decode_rgba_unpremultiplied(buf, stride, width, height)),
        // The legacy snapshot path is BGRA-premultiplied, and the
        // GL/Vulkan renderers we've observed report `B8g8r8a8Premultiplied`
        // — handle the common cases above and leave HDR / wide-gamut
        // formats for a follow-up. Returning None here triggers a
        // structured error in the caller (it doesn't crash).
        _ => None,
    }
}

#[inline]
fn decode_bgra_premultiplied(buf: &[u8], stride: usize, width: u32, height: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let px = row_start + x * 4;
            let b = buf[px] as u32;
            let g = buf[px + 1] as u32;
            let r = buf[px + 2] as u32;
            let a = buf[px + 3] as u32;
            let (r8, g8, b8) = unpremul_rgb(r, g, b, a);
            rgba.extend_from_slice(&[r8, g8, b8, a as u8]);
        }
    }
    rgba
}

#[inline]
fn decode_rgba_premultiplied(buf: &[u8], stride: usize, width: u32, height: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let px = row_start + x * 4;
            let r = buf[px] as u32;
            let g = buf[px + 1] as u32;
            let b = buf[px + 2] as u32;
            let a = buf[px + 3] as u32;
            let (r8, g8, b8) = unpremul_rgb(r, g, b, a);
            rgba.extend_from_slice(&[r8, g8, b8, a as u8]);
        }
    }
    rgba
}

#[inline]
fn decode_bgra_unpremultiplied(buf: &[u8], stride: usize, width: u32, height: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let px = row_start + x * 4;
            let b = buf[px];
            let g = buf[px + 1];
            let r = buf[px + 2];
            let a = buf[px + 3];
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    rgba
}

#[inline]
fn decode_rgba_unpremultiplied(buf: &[u8], stride: usize, width: u32, height: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height as usize {
        let row_start = y * stride;
        for x in 0..width as usize {
            let px = row_start + x * 4;
            rgba.extend_from_slice(&buf[px..px + 4]);
        }
    }
    rgba
}

#[inline]
fn unpremul_rgb(r: u32, g: u32, b: u32, a: u32) -> (u8, u8, u8) {
    if a == 0 {
        (0, 0, 0)
    } else if a == 255 {
        (r as u8, g as u8, b as u8)
    } else {
        (
            unpremultiply(r, a),
            unpremultiply(g, a),
            unpremultiply(b, a),
        )
    }
}

#[inline]
fn unpremultiply(channel: u32, alpha: u32) -> u8 {
    (((channel * 255) + (alpha / 2)) / alpha).min(255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `B8g8r8a8Premultiplied` round-trip — a single opaque red pixel
    /// stored as `[B=0, G=0, R=255, A=255]` should decode to RGBA
    /// `[255, 0, 0, 255]`. This is the path the GL/Vulkan GTK renderers
    /// observed on this machine deliver via the downloader.
    #[test]
    fn bgra_premul_opaque_red_decodes() {
        let buf = [0u8, 0, 255, 255]; // BGRA = (0, 0, 255, 255)
        let out = decode_bgra_premultiplied(&buf, 4, 1, 1);
        assert_eq!(out, vec![255, 0, 0, 255]);
    }

    /// Premultiplied BGRA with 50% alpha: source color was opaque red
    /// (255,0,0), premultiplied stores `[B=0,G=0,R=128,A=128]`.
    /// Un-premultiplying recovers approximately `[255,0,0,128]` (the
    /// +alpha/2 round-half rule lands on 255 exactly here).
    #[test]
    fn bgra_premul_halfalpha_red_unpremultiplies() {
        let buf = [0u8, 0, 128, 128];
        let out = decode_bgra_premultiplied(&buf, 4, 1, 1);
        assert_eq!(out, vec![255, 0, 0, 128]);
    }

    /// `R8g8b8a8` un-premultiplied is a straight copy.
    #[test]
    fn rgba_unpremul_passthrough() {
        let buf = [42u8, 99, 200, 220];
        let out = decode_rgba_unpremultiplied(&buf, 4, 1, 1);
        assert_eq!(out, vec![42, 99, 200, 220]);
    }

    /// `B8g8r8a8` un-premultiplied flips B/R but keeps alpha intact.
    #[test]
    fn bgra_unpremul_swaps_channels() {
        let buf = [10u8, 20, 30, 40]; // B=10, G=20, R=30, A=40
        let out = decode_bgra_unpremultiplied(&buf, 4, 1, 1);
        assert_eq!(out, vec![30, 20, 10, 40]);
    }

    /// `decode_memory_format` dispatches to the right concrete decoder
    /// based on the `GdkMemoryFormat` discriminant. Exercises the
    /// match-arm coverage without needing a display.
    #[test]
    fn decode_memory_format_dispatches_known_formats() {
        let bgra_pre = [0u8, 0, 255, 255];
        let rgba_pre = [255u8, 0, 0, 255];
        let bgra = [10u8, 20, 30, 40];
        let rgba = [11u8, 22, 33, 44];

        let out1 = decode_memory_format(&bgra_pre, 4, 1, 1, gdk::MemoryFormat::B8g8r8a8Premultiplied);
        assert_eq!(out1, Some(vec![255, 0, 0, 255]));
        let out2 = decode_memory_format(&rgba_pre, 4, 1, 1, gdk::MemoryFormat::R8g8b8a8Premultiplied);
        assert_eq!(out2, Some(vec![255, 0, 0, 255]));
        let out3 = decode_memory_format(&bgra, 4, 1, 1, gdk::MemoryFormat::B8g8r8a8);
        assert_eq!(out3, Some(vec![30, 20, 10, 40]));
        let out4 = decode_memory_format(&rgba, 4, 1, 1, gdk::MemoryFormat::R8g8b8a8);
        assert_eq!(out4, Some(vec![11, 22, 33, 44]));
    }

    /// Unhandled memory formats return None so the caller surfaces a
    /// structured "format not handled" error instead of panicking.
    #[test]
    fn decode_memory_format_returns_none_for_unhandled() {
        let buf = vec![0u8; 16];
        let out = decode_memory_format(&buf, 8, 1, 1, gdk::MemoryFormat::R16g16b16a16Float);
        assert!(out.is_none());
    }
}

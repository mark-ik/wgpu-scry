# Phase 4c.2 — WPE Producer on WPEPlatform Headless: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `WpeProducer` produce real `DmaBufImage` frames from a self-owned headless WPEPlatform display + WebKitWebView via the `buffer-rendered` signal.

**Architecture:** Thread-affine producer + synchronous `glib::MainContext` pump (model A, matching the other three backends). The producer owns a `WPEDisplay` (headless) + `WebKitWebView` + `WPEView`; the `buffer-rendered` signal closure downcasts `WPEBufferDMABuf`, `dup()`s its plane fds into a producer-owned `DmaBufImage`, returns WPE's buffer immediately, and writes a single-slot `Arc<Mutex<Option<DmaBufImage>>>`. Hand-written `extern "C"` only for WPE-specific symbols; GObject mechanics (refcount, signal connect, downcast) come from the `glib` crate.

**Tech Stack:** Rust (edition 2024), `glib` 0.22 (GObject mechanics), hand-written `extern "C"` to `libWPEWebKit-2.0.so`, pkg-config (`wpe-webkit-2.0`) via a new `build.rs`, WPEWebKit 2.52.3 / libwpe 1.16.2 on Fedora 44.

**Spec:** [`2026-05-20_phase4c2_wpe_platform_producer.md`](2026-05-20_phase4c2_wpe_platform_producer.md)

**Conventions for this plan:**
- New cargo feature `wpe` gates all FFI. The scaffold types
  (`WpeProducerConfig`, `WpeProducer`, `linux_wpe_capabilities`) stay
  always-compiled so the existing `lib.rs` producer alias keeps building
  without WPEWebKit dev libs installed.
- All FFI work compiles only under `--features wpe`. Build/run commands
  below pass `--features wpe`.
- Commit after every task. Commit trailer:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.

---

## File Structure

The current single-file `scrying/src/wpe_producer.rs` (~212 lines) grows
past one responsibility once FFI lands, so it becomes a module directory:

- Create `scrying/build.rs` — pkg-config link for `wpe-webkit-2.0`, only
  when the `wpe` feature is active.
- Create `scrying/src/wpe_producer/mod.rs` — module doc, re-exports,
  `linux_wpe_capabilities` (moved verbatim from the old file).
- Create `scrying/src/wpe_producer/config.rs` — `WpeProducerConfig`
  (moved verbatim).
- Create `scrying/src/wpe_producer/producer.rs` — `WpeProducer` struct,
  the pure-Rust frame-slot logic (`enqueue_dmabuf_frame`,
  `try_acquire_frame`, fd close-on-evict/teardown), and the
  `WebSurfaceProducer` impl. Always compiled.
- Create `scrying/src/wpe_producer/ffi.rs` — `extern "C"` decls +
  GObject type imports. `#[cfg(feature = "wpe")]`.
- Create `scrying/src/wpe_producer/headless.rs` — display/webview
  construction + `buffer-rendered` wiring + the glib pump.
  `#[cfg(feature = "wpe")]`.
- Delete `scrying/src/wpe_producer.rs` (content moved into the module).
- Modify `scrying/Cargo.toml` — add `wpe` feature + `glib` dep +
  `pkg-config` build-dep.

Split rationale: `producer.rs` holds the fd-ownership logic that *can*
be unit-tested with pipe fds and must compile without WPEWebKit;
`ffi.rs`/`headless.rs` hold the parts that need a live display.

---

## Task 1: Build plumbing — `wpe` feature, `glib` dep, `build.rs`, module split

**Files:**
- Modify: `scrying/Cargo.toml`
- Create: `scrying/build.rs`
- Create: `scrying/src/wpe_producer/mod.rs`, `config.rs`, `producer.rs`
- Delete: `scrying/src/wpe_producer.rs`

- [ ] **Step 1: Add the `wpe` feature, `glib` dep, and `pkg-config` build-dep to `Cargo.toml`**

In `[features]` add:

```toml
# Hand-written WPEPlatform FFI producer (the planned Linux primary).
# Requires WPEWebKit dev libs at build time (Fedora 44):
#   sudo dnf install -y wpewebkit-devel libwpe-devel wpebackend-fdo-devel
# Links libWPEWebKit-2.0.so via pkg-config (wpe-webkit-2.0) in build.rs.
wpe = ["dep:glib"]
```

In `[target.'cfg(target_os = "linux")'.dependencies]` add (glib pinned
to the modern stack the gtk4 0.11 / webkit6 0.6 line resolves to):

```toml
# GObject mechanics (refcount, g_signal_connect, downcast) for the
# hand-written WPEPlatform FFI in `wpe_producer`. glib re-exports the
# low-level `glib::ffi` / `glib::gobject_ffi` we need for raw calls.
glib = { version = "0.22", optional = true }
```

Add a `[build-dependencies]` section (top-level, not target-scoped — the
`build.rs` itself guards on target + feature):

```toml
[build-dependencies]
pkg-config = "0.3"
```

- [ ] **Step 2: Write `build.rs` to link `wpe-webkit-2.0` only under the `wpe` feature**

Create `scrying/build.rs`:

```rust
fn main() {
    // Only the `wpe` feature needs the native WPEWebKit link. Cargo sets
    // CARGO_FEATURE_WPE when the feature is active; TARGET tells us the OS.
    let wpe = std::env::var_os("CARGO_FEATURE_WPE").is_some();
    let linux = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux");
    if wpe && linux {
        pkg_config::Config::new()
            .atleast_version("2.52")
            .probe("wpe-webkit-2.0")
            .expect(
                "wpe feature requires WPEWebKit ≥ 2.52 dev libs \
                 (dnf install wpewebkit-devel); pkg-config wpe-webkit-2.0 failed",
            );
    }
}
```

- [ ] **Step 3: Split the scaffold into a module directory (no behavior change)**

Create `scrying/src/wpe_producer/config.rs` — move `WpeProducerConfig`
(struct + `impl`) verbatim from the old file, keeping its imports
(`std::path::PathBuf`, `dpi::PhysicalSize`).

Create `scrying/src/wpe_producer/mod.rs`:

```rust
//! Linux WPE producer (WPEPlatform headless).
//!
//! The planned Linux primary: a self-owned headless `WPEDisplay` +
//! `WebKitWebView` render into DMABUF buffers that scrying imports
//! through wgpu's Vulkan backend. GObject mechanics come from the `glib`
//! crate; only WPE-specific symbols are hand-written `extern "C"`
//! (see [`ffi`]). FFI is gated behind the `wpe` cargo feature; the
//! producer types compile without it so the `lib.rs` alias still builds.

#![cfg(target_os = "linux")]

mod config;
mod producer;

#[cfg(feature = "wpe")]
mod ffi;
#[cfg(feature = "wpe")]
mod headless;

pub use config::WpeProducerConfig;
pub use producer::WpeProducer;

use crate::native_frame::{CapabilityStatus, NativeFrameKind, UnsupportedReason};
use crate::{SystemWebviewBackend, WebSurfaceCapabilities, WebSurfaceMode};

pub(crate) fn linux_wpe_capabilities() -> WebSurfaceCapabilities {
    WebSurfaceCapabilities {
        backend: SystemWebviewBackend::Wpe,
        preferred_mode: WebSurfaceMode::Unsupported,
        imported_texture: CapabilityStatus::Unsupported(
            UnsupportedReason::NativeImportNotYetImplemented,
        ),
        native_child_overlay: CapabilityStatus::Unsupported(
            UnsupportedReason::PlatformNotImplemented,
        ),
        cpu_snapshot: CapabilityStatus::Unsupported(
            UnsupportedReason::NativeImportNotYetImplemented,
        ),
        supported_frames: vec![NativeFrameKind::DmaBufImage],
        reason: "WPE is the planned Linux primary backend (DMABUF + Vulkan external memory); the producer API and DMABUF frame contract are present, but the WPE FFI callback bridge and Vulkan importer are not wired yet.",
    }
}
```

Create `scrying/src/wpe_producer/producer.rs` — move the `WpeProducer`
struct, `enqueue_dmabuf_frame`, `try_acquire_frame`, `offset`, and the
`WebSurfaceProducer` impl verbatim, but **drop the `unsafe fn new`**
(replaced in Task 3). Temporarily add a minimal safe constructor so the
crate still builds and the alias resolves:

```rust
impl WpeProducer {
    /// Placeholder constructor — real headless construction lands in
    /// Task 3. Without the `wpe` feature this is the only constructor.
    #[cfg(not(feature = "wpe"))]
    pub fn new(config: WpeProducerConfig) -> Result<Self, crate::WebSurfaceError> {
        if config.size.width == 0 || config.size.height == 0 {
            return Err(crate::WebSurfaceError::Platform(format!(
                "WPE producer size must be non-zero, got {}x{}",
                config.size.width, config.size.height
            )));
        }
        Ok(Self {
            capabilities: super::linux_wpe_capabilities(),
            size: config.size,
            offset: config.offset,
            pending_frame: std::sync::Arc::new(std::sync::Mutex::new(None)),
            generation: 0,
        })
    }
}
```

Keep `pending_frame: Arc<Mutex<Option<DmaBufImage>>>` and `generation`
fields as-is for now (Task 4 moves `generation` into shared state).
Delete `scrying/src/wpe_producer.rs`.

- [ ] **Step 4: Verify both feature configurations build**

Run: `cargo build -p scrying` (no wpe)
Expected: builds; `wpe_producer` compiles as scaffold.

Run: `cargo build -p scrying --features wpe`
Expected: builds and links `libWPEWebKit-2.0.so`. If glib fails to
resolve, run `cargo tree -p scrying -i glib` to see the version the
gtk4/webkit6 line pins and align the `glib = "0.22"` value to it.

- [ ] **Step 5: Commit**

```bash
git add scrying/Cargo.toml scrying/build.rs scrying/src/wpe_producer/ scrying/src/wpe_producer.rs
git commit -m "$(cat <<'EOF'
phase 4c.2: wpe cargo feature + build.rs link + module split

Adds the `wpe` feature gating hand-written WPEPlatform FFI, a build.rs
that pkg-config-links wpe-webkit-2.0 only under that feature, and splits
wpe_producer.rs into a module dir (config/producer always compiled;
ffi/headless feature-gated). No behavior change yet.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Binding spike — headless display + WebView, assert the binding

Resolves the spec's flagged unknown: how a headless `WPEDisplay` binds
to a `WebKitWebView`. This is empirical (GObject construct properties are
registered in the `.c`, not headers), so it ships as a `#[cfg(feature =
"wpe")]` **ignored** test that stands the objects up and asserts the
binding — the foundation everything else builds on.

**Files:**
- Create: `scrying/src/wpe_producer/ffi.rs`
- Create: `scrying/src/wpe_producer/headless.rs`
- Test: inline `#[cfg(test)]` in `headless.rs`

- [ ] **Step 1: Declare the FFI surface in `ffi.rs`**

```rust
//! Hand-written `extern "C"` declarations for WPE-specific symbols in
//! libWPEWebKit-2.0.so. GObject-generic operations (ref/unref, signal
//! connect, type checks) use the `glib` crate instead. Signatures
//! verified against WPEWebKit 2.52.3 headers under
//! /usr/include/wpe-webkit-2.0.

use glib::ffi::{gpointer, GType};
use std::os::raw::{c_char, c_int};

// Opaque GObject types — always used behind pointers.
#[repr(C)]
pub struct WPEDisplay {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WPEView {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WPEBuffer {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WPEBufferDMABuf {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct WebKitWebView {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    // --- wpe-platform ---
    pub fn wpe_display_headless_new() -> *mut WPEDisplay;

    pub fn wpe_view_buffer_released(view: *mut WPEView, buffer: *mut WPEBuffer);

    pub fn wpe_buffer_get_width(buffer: *mut WPEBuffer) -> c_int;
    pub fn wpe_buffer_get_height(buffer: *mut WPEBuffer) -> c_int;

    pub fn wpe_buffer_dma_buf_get_type() -> GType;
    pub fn wpe_buffer_dma_buf_get_format(buffer: *mut WPEBufferDMABuf) -> u32;
    pub fn wpe_buffer_dma_buf_get_n_planes(buffer: *mut WPEBufferDMABuf) -> u32;
    pub fn wpe_buffer_dma_buf_get_fd(buffer: *mut WPEBufferDMABuf, plane: u32) -> c_int;
    pub fn wpe_buffer_dma_buf_get_offset(buffer: *mut WPEBufferDMABuf, plane: u32) -> u32;
    pub fn wpe_buffer_dma_buf_get_stride(buffer: *mut WPEBufferDMABuf, plane: u32) -> u32;
    pub fn wpe_buffer_dma_buf_get_modifier(buffer: *mut WPEBufferDMABuf) -> u64;

    // --- wpe-webkit ---
    pub fn webkit_web_view_get_display(web_view: *mut WebKitWebView) -> *mut WPEDisplay;
    pub fn webkit_web_view_get_wpe_view(web_view: *mut WebKitWebView) -> *mut WPEView;
    pub fn webkit_web_view_get_type() -> GType;
}

// Suppress "field never read" on the opaque marker.
#[allow(dead_code)]
fn _opaque_marker_used(_: &WPEDisplay) {}
const _: () = {
    let _ = std::mem::size_of::<gpointer>();
    let _ = std::ptr::null::<c_char>();
};
```

- [ ] **Step 2: Write the headless construction + binding-assert spike in `headless.rs`**

The WebView is created with `glib::Object::new` against the WebKit
GObject type, passing the display as the `"display"` construct property
(the expected WPEPlatform path). `webkit_web_view_get_type()` registers
the type so glib can construct it.

```rust
//! Headless WPEPlatform display + WebKitWebView construction and the
//! `buffer-rendered` frame seam. `#[cfg(feature = "wpe")]` only.

use glib::translate::{from_glib_none, ToGlibPtr};
use glib::Object;

use super::ffi;
use crate::WebSurfaceError;

/// Construct a headless display and a WebView bound to it. Returns the
/// raw display + webview pointers (ownership held by the returned glib
/// `Object` for the webview; the display is owned by the webview once
/// bound). Internal — callers go through `WpeProducer::new`.
pub(super) fn build_headless_webview() -> Result<(Object, *mut ffi::WPEDisplay), WebSurfaceError> {
    // SAFETY: wpe_display_headless_new takes no args and returns a
    // floating/owned WPEDisplay* or null on failure.
    let display = unsafe { ffi::wpe_display_headless_new() };
    if display.is_null() {
        return Err(WebSurfaceError::Platform(
            "wpe_display_headless_new() returned null; no headless WPE display available".into(),
        ));
    }

    // Register and fetch the WebKitWebView GType, then construct via glib
    // with the "display" construct property bound to our headless display.
    let webview_gtype = unsafe { glib::Type::from_glib(ffi::webkit_web_view_get_type()) };
    // Wrap the raw display pointer as a glib Value of type WPEDisplay so
    // it can be passed as a construct property.
    let display_value = unsafe {
        glib::Value::from_type(glib::Type::OBJECT)
        // replaced below; see Step 3 note
    };
    let _ = display_value;

    let webview: Object = unsafe {
        glib::Object::with_mut_values(webview_gtype, &mut [])
    };

    // Assert the binding actually took.
    let raw_webview: *mut ffi::WebKitWebView = webview.to_glib_none().0 as *mut _;
    let bound_display = unsafe { ffi::webkit_web_view_get_display(raw_webview) };
    let wpe_view = unsafe { ffi::webkit_web_view_get_wpe_view(raw_webview) };
    let _ = (bound_display, wpe_view, display);

    Ok((webview, display))
}
```

> **Implementation note (read before coding Step 2):** the exact glib
> call to pass `display` as a construct property depends on the glib
> 0.22 API and how WebKitWebView's `"display"` property is typed. The
> spike's job is to make `webkit_web_view_get_display(view) == display`
> true. Try, in order, until the assertion in Step 4 passes:
> 1. `glib::Object::builder_with_type(webview_gtype).property("display", unsafe { glib::Object::from_glib_none(display as *mut glib::gobject_ffi::GObject) }).build()`
> 2. Raw: `glib::gobject_ffi::g_object_new(gtype, c"display".as_ptr(), display, std::ptr::null::<c_char>())` then `from_glib_full`.
> Pick whichever compiles and passes; delete the other. This is the one
> place the plan cannot pre-commit exact code — the test is the oracle.

- [ ] **Step 3: Write the ignored binding test**

Add to `headless.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn headless_webview_binds_display() {
        let (webview, display) = build_headless_webview().expect("build headless webview");
        let raw: *mut ffi::WebKitWebView = webview.to_glib_none().0 as *mut _;
        let bound = unsafe { ffi::webkit_web_view_get_display(raw) };
        assert_eq!(bound, display, "webview must be bound to our headless display");
        let view = unsafe { ffi::webkit_web_view_get_wpe_view(raw) };
        assert!(!view.is_null(), "webview must expose a WPEView");
    }
}
```

- [ ] **Step 4: Run the spike test and iterate on the construct-property call until it passes**

Run: `cargo test -p scrying --features wpe headless_webview_binds_display -- --ignored --nocapture`
Expected: PASS — `bound == display` and `wpe_view` non-null. If it
fails, adjust the construct-property call per the Step 2 note. Do not
proceed to Task 3 until this passes.

- [ ] **Step 5: Commit**

```bash
git add scrying/src/wpe_producer/ffi.rs scrying/src/wpe_producer/headless.rs
git commit -m "$(cat <<'EOF'
phase 4c.2: WPE FFI decls + headless display/webview binding spike

extern "C" surface for WPEPlatform + an ignored runtime test that
constructs a headless WPEDisplay and a WebKitWebView bound to it,
asserting webkit_web_view_get_display(view) == display and a non-null
WPEView. Resolves the display->webview binding unknown from the spec.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Safe self-owned `WpeProducer::new(config)` under the `wpe` feature

**Files:**
- Modify: `scrying/src/wpe_producer/producer.rs`
- Modify: `scrying/src/wpe_producer/headless.rs`

- [ ] **Step 1: Hold the owned GObject handles on the producer**

In `producer.rs`, add feature-gated fields to `WpeProducer` (keep
existing fields). Use a sub-struct so the non-`wpe` build is unaffected:

```rust
#[cfg(feature = "wpe")]
pub(super) struct WpeHandles {
    /// Owns the WebKitWebView (and, transitively, the bound display).
    pub webview: glib::Object,
    /// Borrowed-from-webview view pointer, valid for the webview's life.
    pub view: *mut super::ffi::WPEView,
    /// GLib context the producer is affine to; pumped by acquire/navigate.
    pub main_context: glib::MainContext,
}
```

Add to the `WpeProducer` struct:

```rust
    #[cfg(feature = "wpe")]
    handles: WpeHandles,
```

- [ ] **Step 2: Implement the real `new` (feature-gated) and keep the stub for non-`wpe`**

In `producer.rs`, replace the Task-1 placeholder block with two
cfg-gated constructors. The `wpe` one wires headless construction:

```rust
impl WpeProducer {
    #[cfg(feature = "wpe")]
    pub fn new(config: WpeProducerConfig) -> Result<Self, crate::WebSurfaceError> {
        use crate::WebSurfaceError;
        if config.size.width == 0 || config.size.height == 0 {
            return Err(WebSurfaceError::Platform(format!(
                "WPE producer size must be non-zero, got {}x{}",
                config.size.width, config.size.height
            )));
        }
        let main_context = glib::MainContext::default();
        let (webview, view) = super::headless::build_producer_view()?;
        Ok(Self {
            capabilities: super::linux_wpe_capabilities(),
            size: config.size,
            offset: config.offset,
            pending_frame: std::sync::Arc::new(std::sync::Mutex::new(None)),
            generation: 0,
            handles: WpeHandles { webview, view, main_context },
        })
    }

    #[cfg(not(feature = "wpe"))]
    pub fn new(config: WpeProducerConfig) -> Result<Self, crate::WebSurfaceError> {
        if config.size.width == 0 || config.size.height == 0 {
            return Err(crate::WebSurfaceError::Platform(format!(
                "WPE producer size must be non-zero, got {}x{}",
                config.size.width, config.size.height
            )));
        }
        Ok(Self {
            capabilities: super::linux_wpe_capabilities(),
            size: config.size,
            offset: config.offset,
            pending_frame: std::sync::Arc::new(std::sync::Mutex::new(None)),
            generation: 0,
        })
    }
}
```

- [ ] **Step 3: Promote the spike into `build_producer_view`**

In `headless.rs`, rename `build_headless_webview` to
`build_producer_view`, returning `(glib::Object, *mut ffi::WPEView)`
(the resolved, working construct-property code from Task 2), with the
null-display error path retained:

```rust
pub(super) fn build_producer_view(
) -> Result<(glib::Object, *mut ffi::WPEView), crate::WebSurfaceError> {
    // ... resolved Task-2 construction ...
    // returns (webview, wpe_view) with wpe_view asserted non-null.
}
```

Update the Task-2 test to call `build_producer_view` and assert on the
returned view pointer.

- [ ] **Step 4: Build both configurations**

Run: `cargo build -p scrying`
Expected: PASS (non-wpe stub `new`).

Run: `cargo build -p scrying --features wpe`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add scrying/src/wpe_producer/producer.rs scrying/src/wpe_producer/headless.rs
git commit -m "$(cat <<'EOF'
phase 4c.2: safe self-owned WpeProducer::new(config)

Under the wpe feature, new() constructs the headless display + webview
and holds the owned handles + glib MainContext (model A affinity). The
non-wpe build keeps a stub new() so the lib.rs alias still resolves.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: The `buffer-rendered` frame seam

Connect the signal; the closure builds a `DmaBufImage` from
`WPEBufferDMABuf` with **dup'd** fds, releases WPE's buffer immediately,
and writes the shared slot. Because the closure can't borrow `&mut
self`, the shared state (`pending_frame` + generation) is captured by
`Arc` clones.

**Files:**
- Modify: `scrying/src/wpe_producer/producer.rs` (generation → shared)
- Modify: `scrying/src/wpe_producer/headless.rs` (signal wiring + builder)
- Modify: `scrying/src/wpe_producer/ffi.rs` (add `dup` via libc — already a dep)

- [ ] **Step 1: Move `generation` into shared state**

In `producer.rs`, change the producer to share the generation counter so
the signal closure can bump it:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
// field change:
//   generation: u64,  ->  generation: std::sync::Arc<AtomicU64>,
```

Update `enqueue_dmabuf_frame` to use the shared counter:

```rust
        let gen = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        frame.generation = gen;
```

(Initialize `generation: Arc::new(AtomicU64::new(0))` in both `new`s.)

- [ ] **Step 2: Add a shared frame-sink type the closure can own**

In `producer.rs`, expose the slot + generation as a cloneable bundle:

```rust
#[derive(Clone)]
pub(super) struct FrameSink {
    pub pending: std::sync::Arc<std::sync::Mutex<Option<crate::native_frame::DmaBufImage>>>,
    pub generation: std::sync::Arc<AtomicU64>,
}

impl FrameSink {
    /// Store a new frame; close fds of any evicted stale frame first.
    pub fn submit(&self, frame: crate::native_frame::DmaBufImage) {
        let mut slot = match self.pending.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        if let Some(old) = slot.take() {
            super::producer::close_frame_fds(&old);
        }
        *slot = Some(frame);
    }
}
```

Add the fd-closing helper (used here and by teardown/Task 5):

```rust
pub(super) fn close_frame_fds(frame: &crate::native_frame::DmaBufImage) {
    for plane in &frame.planes {
        // SAFETY: producer-owned dup'd fd not yet handed to the importer.
        unsafe { libc::close(plane.fd); }
    }
    if let Some(fd) = frame.semaphore_fd {
        unsafe { libc::close(fd); }
    }
}
```

Make `WpeProducer` expose its sink: add a method
`pub(super) fn frame_sink(&self) -> FrameSink`.

- [ ] **Step 3: Build a `DmaBufImage` from a `WPEBufferDMABuf` in `headless.rs`**

```rust
use super::ffi;
use crate::native_frame::{DmaBufImage, DmaBufPlane, SyncMechanism};

/// Convert a rendered WPEBufferDMABuf into a producer-owned DmaBufImage
/// by dup()-ing each plane fd. `buffer_base` is the same buffer as
/// `dmabuf`, cast to the WPEBuffer base type (for width/height).
unsafe fn dmabuf_to_image(
    dmabuf: *mut ffi::WPEBufferDMABuf,
    buffer_base: *mut ffi::WPEBuffer,
) -> Option<DmaBufImage> {
    let width = ffi::wpe_buffer_get_width(buffer_base);
    let height = ffi::wpe_buffer_get_height(buffer_base);
    if width <= 0 || height <= 0 {
        return None;
    }
    let n_planes = ffi::wpe_buffer_dma_buf_get_n_planes(dmabuf);
    if n_planes == 0 {
        return None;
    }
    let mut planes = Vec::with_capacity(n_planes as usize);
    for i in 0..n_planes {
        let raw_fd = ffi::wpe_buffer_dma_buf_get_fd(dmabuf, i);
        // dup so the importer can own its copy independently of WPE's pool.
        let fd = libc::dup(raw_fd);
        if fd < 0 {
            // Close any fds dup'd so far, then bail.
            for p in &planes {
                libc::close((p as &DmaBufPlane).fd);
            }
            return None;
        }
        planes.push(DmaBufPlane {
            fd,
            offset: ffi::wpe_buffer_dma_buf_get_offset(dmabuf, i),
            stride: ffi::wpe_buffer_dma_buf_get_stride(dmabuf, i),
        });
    }
    let drm_format = ffi::wpe_buffer_dma_buf_get_format(dmabuf);
    let drm_modifier = ffi::wpe_buffer_dma_buf_get_modifier(dmabuf);
    Some(DmaBufImage {
        size: dpi::PhysicalSize::new(width as u32, height as u32),
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        drm_format,
        drm_modifier,
        planes,
        generation: 0, // assigned by FrameSink::submit path / enqueue
        producer_sync: SyncMechanism::None,
        semaphore_fd: None,
    })
}
```

> **Note:** `format` maps the DRM fourcc to a `wgpu::TextureFormat`.
> WPE's default headless buffer is BGRA. If the runtime smoke (Task 6)
> shows a different fourcc via `drm_format`, map it explicitly; start
> with `Bgra8UnormSrgb` and correct against the observed value.

- [ ] **Step 4: Connect the `buffer-rendered` signal in `build_producer_view`**

Use glib's `connect_closure`/`connect_local` on the webview's WPEView.
The signal hands `(WPEView*, WPEBuffer*)`. Inside: downcast to DMABuf via
the registered type, build the image, assign generation, submit to the
sink, then `wpe_view_buffer_released`.

```rust
pub(super) fn connect_buffer_rendered(
    view: *mut ffi::WPEView,
    view_obj: &glib::Object, // the WPEView wrapped as a glib Object
    sink: super::producer::FrameSink,
) {
    use glib::translate::ToGlibPtr;
    let dmabuf_gtype = unsafe { glib::Type::from_glib(ffi::wpe_buffer_dma_buf_get_type()) };
    view_obj.connect_closure(
        "buffer-rendered",
        false,
        glib::closure_local!(move |_view: glib::Object, buffer: glib::Object| {
            let raw_buf: *mut ffi::WPEBuffer = buffer.to_glib_none().0 as *mut _;
            // Type-check before treating as DMABuf.
            if !buffer.type_().is_a(dmabuf_gtype) {
                unsafe { ffi::wpe_view_buffer_released(view, raw_buf); }
                return;
            }
            let raw_dmabuf = raw_buf as *mut ffi::WPEBufferDMABuf;
            let image = unsafe { dmabuf_to_image(raw_dmabuf, raw_buf) };
            // Hand WPE's buffer back immediately regardless.
            unsafe { ffi::wpe_view_buffer_released(view, raw_buf); }
            if let Some(mut image) = image {
                image.generation = sink.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                sink.submit(image);
            }
        }),
    );
}
```

> **Note:** the `buffer-rendered` signal name and its glib closure
> argument types are confirmed by connecting; if `connect_closure`
> reports an unknown signal or arity mismatch, inspect available signals
> via `WPEView`'s GType at runtime (`glib`'s
> `SignalQuery`) and adjust the name/args. The handler body is otherwise
> final.

Wire it into `build_producer_view` after the view is obtained, passing
the producer's `FrameSink` (thread it through `new`).

- [ ] **Step 5: Build under the feature**

Run: `cargo build -p scrying --features wpe`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add scrying/src/wpe_producer/
git commit -m "$(cat <<'EOF'
phase 4c.2: buffer-rendered seam -> DmaBufImage with dup'd fds

Connects the WPEView buffer-rendered signal; the closure downcasts
WPEBufferDMABuf, dup()s each plane fd into a producer-owned DmaBufImage
(SyncMechanism::None), releases WPE's buffer immediately, and submits to
a shared FrameSink (generation moved into shared atomic).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: fd lifecycle — close-on-evict + teardown (unit-TDD)

The only pure-Rust logic that can be tested without a display: evicting a
stale frame must close its fds, and dropping the producer must close any
unconsumed frame's fds. Use real pipe fds so closure is observable.

**Files:**
- Modify: `scrying/src/wpe_producer/producer.rs` (Drop impl + test)
- Test: inline `#[cfg(test)]` in `producer.rs`

- [ ] **Step 1: Write the failing eviction test**

```rust
#[cfg(test)]
mod fd_tests {
    use super::*;
    use crate::native_frame::{DmaBufImage, DmaBufPlane, SyncMechanism};

    fn pipe_fd() -> i32 {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { libc::close(fds[1]) }; // close write end; keep read end
        fds[0]
    }

    fn fd_open(fd: i32) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    fn frame_with_fd(fd: i32) -> DmaBufImage {
        DmaBufImage {
            size: dpi::PhysicalSize::new(4, 4),
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            drm_format: 0,
            drm_modifier: 0,
            planes: vec![DmaBufPlane { fd, offset: 0, stride: 16 }],
            generation: 0,
            producer_sync: SyncMechanism::None,
            semaphore_fd: None,
        }
    }

    #[test]
    fn evicting_stale_frame_closes_its_fds() {
        let sink = FrameSink {
            pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
            generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let stale_fd = pipe_fd();
        sink.submit(frame_with_fd(stale_fd));
        let fresh_fd = pipe_fd();
        sink.submit(frame_with_fd(fresh_fd)); // evicts stale
        assert!(!fd_open(stale_fd), "stale frame's fd must be closed on eviction");
        assert!(fd_open(fresh_fd), "fresh frame's fd must remain open");
    }
}
```

- [ ] **Step 2: Run it — expect it to PASS** (the `FrameSink::submit` +
`close_frame_fds` from Task 4 already implement eviction-close)

Run: `cargo test -p scrying --features wpe evicting_stale_frame_closes_its_fds`
Expected: PASS. If FAIL, fix `FrameSink::submit`/`close_frame_fds`.

- [ ] **Step 3: Write the failing teardown test**

```rust
    #[test]
    fn dropping_producer_closes_unconsumed_fd() {
        let producer = WpeProducer::new(
            crate::wpe_producer::WpeProducerConfig::new(
                dpi::PhysicalSize::new(8, 8), std::env::temp_dir(),
            ),
        );
        // NOTE: under --features wpe, new() needs a display; this test is
        // marked #[ignore] when wpe is on. The fd-close-on-drop logic is
        // exercised via FrameSink directly below instead.
        let _ = producer;

        let pending = std::sync::Arc::new(std::sync::Mutex::new(None));
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let leftover_fd = pipe_fd();
        {
            let sink = FrameSink { pending: pending.clone(), generation };
            sink.submit(frame_with_fd(leftover_fd));
        } // sink dropped, but slot still holds the frame
        // Simulate producer teardown closing the unconsumed frame.
        if let Some(frame) = pending.lock().unwrap().take() {
            close_frame_fds(&frame);
        }
        assert!(!fd_open(leftover_fd), "unconsumed frame's fd closed at teardown");
    }
```

- [ ] **Step 4: Implement `Drop` for `WpeProducer` to close the unconsumed slot**

In `producer.rs`:

```rust
impl Drop for WpeProducer {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.pending_frame.lock() {
            if let Some(frame) = slot.take() {
                close_frame_fds(&frame);
            }
        }
    }
}
```

- [ ] **Step 5: Run the fd tests**

Run: `cargo test -p scrying --features wpe fd_tests`
Expected: PASS (both). Also run without the feature to confirm the logic
compiles standalone: `cargo test -p scrying fd_tests` (the `wpe`-gated
`FrameSink`/`close_frame_fds` must be available to the test; if they're
behind `#[cfg(feature="wpe")]`, gate this test module the same way).

- [ ] **Step 6: Commit**

```bash
git add scrying/src/wpe_producer/producer.rs
git commit -m "$(cat <<'EOF'
phase 4c.2: fd lifecycle — close on stale-evict and producer teardown

Drop impl closes any unconsumed frame's dup'd fds; FrameSink eviction
closes the displaced frame's fds. Unit-tested with real pipe fds so
closure is observable without a WPE display.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Gated runtime smoke — render a page, assert a real frame

End-to-end validation against a live display: load a solid-color page,
pump the glib context until one `buffer-rendered` lands, assert the
resulting `DmaBufImage`. `#[ignore]`d (needs GPU + Wayland), mirroring
`demo-linux`'s first-frame gate. Navigation API is 4c.3, so this uses
the lowest primitive that triggers a paint.

**Files:**
- Modify: `scrying/src/wpe_producer/headless.rs` (a minimal
  `load_html_for_smoke` + `pump_until` helper)
- Modify: `scrying/src/wpe_producer/producer.rs` (test)

- [ ] **Step 1: Add a glib pump helper mirroring the GTK producer**

In `headless.rs`:

```rust
use std::time::{Duration, Instant};

pub(super) fn pump_until(
    ctx: &glib::MainContext,
    deadline: Instant,
    mut cond: impl FnMut() -> bool,
) -> Result<(), crate::WebSurfaceError> {
    while !cond() {
        if Instant::now() >= deadline {
            return Err(crate::WebSurfaceError::NotReady(
                "WPE main-loop pump deadline exceeded",
            ));
        }
        ctx.iteration(false); // process pending events, non-blocking
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}
```

- [ ] **Step 2: Add a minimal paint trigger**

In `headless.rs`, a thin `load_html_for_smoke(webview, html)` that calls
the WebKit load entry point for a `data:`/inline HTML string. Declare the
needed `webkit_web_view_load_html(view, content, base_uri)` in `ffi.rs`:

```rust
// ffi.rs
pub fn webkit_web_view_load_html(
    web_view: *mut WebKitWebView,
    content: *const c_char,
    base_uri: *const c_char,
);
```

```rust
// headless.rs
pub(super) fn load_html_for_smoke(webview: &glib::Object, html: &str) {
    use glib::translate::ToGlibPtr;
    let raw: *mut ffi::WebKitWebView = webview.to_glib_none().0 as *mut _;
    let c_html = std::ffi::CString::new(html).unwrap();
    unsafe { ffi::webkit_web_view_load_html(raw, c_html.as_ptr(), std::ptr::null()); }
}
```

- [ ] **Step 3: Write the ignored runtime smoke test**

In `producer.rs`:

```rust
#[cfg(all(test, feature = "wpe"))]
mod smoke {
    use super::*;
    use crate::native_frame::{NativeFrame, SyncMechanism};
    use crate::WebSurfaceFrame;

    #[test]
    #[ignore = "needs a headless WPE display (GPU + Wayland); run manually"]
    fn renders_one_dmabuf_frame() {
        let mut producer = WpeProducer::new(
            crate::wpe_producer::WpeProducerConfig::new(
                dpi::PhysicalSize::new(256, 256), std::env::temp_dir(),
            ),
        ).expect("construct headless producer");

        super::super::headless::load_html_for_smoke(
            &producer.handles.webview,
            "<body style='margin:0;background:#1e90ff'></body>",
        );

        let ctx = producer.handles.main_context.clone();
        let pending = producer.pending_frame.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        super::super::headless::pump_until(&ctx, deadline, || {
            pending.lock().map(|s| s.is_some()).unwrap_or(false)
        }).expect("a frame should arrive within 5s");

        let frame = producer.acquire_frame().expect("frame available");
        let WebSurfaceFrame::Native(NativeFrame::DmaBufImage(img)) = frame else {
            panic!("expected a DMABUF frame");
        };
        assert!(img.size.width > 0 && img.size.height > 0, "non-zero size");
        assert!(!img.planes.is_empty(), "at least one plane");
        assert!(img.planes[0].fd >= 0, "valid dup'd fd");
        assert_eq!(img.producer_sync, SyncMechanism::None);
        eprintln!(
            "smoke: {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}",
            img.size.width, img.size.height, img.drm_format,
            img.drm_modifier, img.planes.len()
        );
        // Close the fd we just took ownership of (no importer in this test).
        super::close_frame_fds(&img);
    }
}
```

- [ ] **Step 4: Run the smoke test manually**

Run: `cargo test -p scrying --features wpe renders_one_dmabuf_frame -- --ignored --nocapture`
Expected: PASS, with a printed line like
`smoke: 256x256 fourcc=0x34325258 mod=0x... planes=1`. Use the printed
`fourcc` to confirm/correct the `format` mapping from Task 4 Step 3. If
no frame arrives, raise the deadline and confirm a GPU/Wayland session
is available (the same env `demo-linux` needs).

- [ ] **Step 5: Commit**

```bash
git add scrying/src/wpe_producer/headless.rs scrying/src/wpe_producer/ffi.rs scrying/src/wpe_producer/producer.rs
git commit -m "$(cat <<'EOF'
phase 4c.2: gated runtime smoke — render solid-color page, assert frame

Ignored end-to-end test: loads an inline HTML page, pumps the glib
context until buffer-rendered fires, and asserts the resulting
DmaBufImage (non-zero size, >=1 plane, valid dup'd fd, SyncMechanism
None). Prints the observed fourcc/modifier to validate the format map.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- Model A threading → Task 3 (MainContext on producer) + Task 6
  (`pump_until` mirrors GTK). ✓
- Safe `new(config)` self-owned → Task 3. ✓
- Null-display error → Task 2/3 (`build_producer_view`). ✓
- `enqueue_dmabuf_frame` kept as test seam → preserved in Task 1 move;
  generation→shared in Task 4. ✓
- Frame seam: downcast, dup, immediate release, write slot → Task 4. ✓
- Size from base `WPEBuffer` → Task 4 Step 3. ✓
- Close-on-evict + teardown → Task 5. ✓
- `SyncMechanism::None` → Task 4 Step 3. ✓
- FFI surface confirmed → Task 2 `ffi.rs`. ✓
- Display→WebView binding validated first → Task 2 spike. ✓
- Importer/`DmaBufImage` contract untouched → no edits to
  `native_frame/`; only producer-local fd handling. ✓
- Gated runtime smoke mirroring demo-linux → Task 6. ✓
- Deferred (navigation/input/cookies/schemes/explicit-sync/wpe-rs) → not
  in any task. ✓

**Placeholder scan:** Two spots are explicitly empirical, not
placeholders — Task 2's construct-property call (the test is the oracle,
two concrete candidates given) and the `format` fourcc map (concrete
default `Bgra8UnormSrgb` + correction step against observed value). Both
have concrete code + a decision procedure. No "TBD"/"handle errors"
left.

**Type consistency:** `FrameSink { pending, generation }`,
`close_frame_fds(&DmaBufImage)`, `build_producer_view() -> (glib::Object,
*mut ffi::WPEView)`, `pump_until(&MainContext, Instant, FnMut)`,
`WpeHandles { webview, view, main_context }` — names consistent across
Tasks 3–6. `generation` is `Arc<AtomicU64>` from Task 4 onward in both
constructors.

**Known risk carried into execution:** glib 0.22 high-level API details
(construct-property passing, `connect_closure` arg marshalling) are the
likeliest friction; Tasks 2 and 4 isolate them behind a runtime oracle
so they're resolved empirically rather than guessed.

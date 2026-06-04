# End-to-end WPE → Vulkan round-trip integration test

Closes the loop the 4c.2 retrospective + 4c.3 final review both flagged as
the next natural step: take the `DmaBufImage` a real headless WPE render
produces and hand it to Phase 4a's `import_dmabuf_image`, proving the
producer → importer pipeline wires up end-to-end.

## Scope

Scope decision was the empirical "single-plane attempt, accept whatever
happens" path (variant A in brainstorming). This task builds the test
*framework* — the harness invoking the importer on a live WPE frame.
Pixel correctness is out of scope; multi-plane DCC import is a separate
Phase 4a expansion (variant B); `WPEDisplay` subclassing to force a
simpler modifier is yet another path (variant C).

In:
- New integration test binary `scrying/tests/wpe_to_vulkan_roundtrip.rs`,
  gated on `target_os = "linux"` + `feature = "wpe"`.
- Construct producer, navigate, acquire DMABUF, invoke
  `import_dmabuf_image`, assert.

Out:
- Modifying `native_frame/dmabuf.rs` to handle multi-plane DRM-modifier
  imports.
- Pixel-correctness sampling / color-equivalence assertion.
- Custom `WPEDisplay` subclassing to advertise only LINEAR modifier.
- Any change to `headless.rs`, `navigation.rs`, `producer.rs`, `ffi.rs`,
  `mod.rs`, `lib.rs`, or the existing unit-test smoke.

## Architecture

A `tests/*.rs` integration test in cargo is its own binary, so its
WebKit state is independent of the unit-test binary — this avoids the
one-WPE-per-process constraint the unit-test module's
`navigate_resize_and_render` honors. That constraint is the explicit
reason the headless module doc steers end-to-end coverage into
separate `tests/` targets; this is the first one to exercise that.

```
[wgpu]  <-- import_dmabuf_image(device, dmabuf_image) --  [Phase 4a]
                            ^                                ^
                            |                                |
                            +-------- DmaBufImage -----------+
                                          ^
                                          |
[WpeProducer (headless WPEPlatform)] -- 4c.2/4c.3 surface
                                          ^
                                          |
                            navigate_to_string("<…dodgerblue…>")
```

## Flow

The test:

1. Build wgpu Instance → request_adapter (vulkan backend) → request_device,
   matching the existing `tests/dmabuf_roundtrip.rs` pattern. Whatever
   helper that test uses to obtain a DMABUF-capable device
   (`build_dmabuf_capable_device` per Phase 4a.7, or its direct ash usage)
   gets reused — the exact symbol is confirmed at implementation time by
   reading the existing test.
2. Construct `WpeProducer` (`WpeProducerConfig::new(PhysicalSize::new(256, 256),
   std::env::temp_dir())`). The headless toplevel coerces to 1024×768 —
   that's fine; the test reads actual dimensions off the frame.
3. `navigate_to_string("<body style='margin:0;background:#1e90ff'></body>",
   Duration::from_secs(5))`. On return, drain `poll_navigation_event` and
   assert a `Completed { success: true }` arrived.
4. Pump until `pending_frame` is `Some` (same post-navigate pump shape the
   unit smoke uses — `wait_for_load` returns when load-changed FINISHED
   fires, before `buffer-rendered` lands). Then `acquire_frame` →
   `WebSurfaceFrame::Native(NativeFrame::DmaBufImage(image))`.
5. Print `fourcc / modifier / planes / size` (diagnostic).
6. Call `native_frame::dmabuf::import_dmabuf_image(&device, image)` (exact
   signature verified at write time — may take `&Device` and `DmaBufImage`,
   or wrap them through ash directly; the existing roundtrip test is the
   reference).
7. Inspect the result. See assertions below.

## Assertions

**WPE side (always):**
- `image.size.width > 0 && image.size.height > 0`.
- `!image.planes.is_empty()`.
- `image.planes[0].fd >= 0`.
- `image.producer_sync == SyncMechanism::None`.

**Import side:**
- Assert `import_dmabuf_image(...)` returns `Ok(...)`. If it returns `Err`,
  fail the test with the literal Err message — that failure IS the
  actionable data ("Phase 4a importer needs multi-plane DRM-modifier
  expansion to handle this WPE buffer; observed modifier=0x... planes=N").
  The test is `#[ignore]`d so CI doesn't trip on this; manual runs see the
  clear signal.
- On `Ok`, assert the returned wgpu texture has `size().width ==
  image.size.width` and `size().height == image.size.height`, and that its
  `format()` matches the producer's `image.format` (likely
  `Bgra8UnormSrgb` per the 4c.2 retro's XR24 mapping).

**Pixel-correctness sampling is OUT.** Per the (A) decision: we're not
asserting the imported texture renders correctly. That requires either
multi-plane import (Phase 4a expansion) or forcing a linear modifier; both
are separate phases.

## Fd ownership

`DmaBufImage`'s fds transfer to the importer per the existing contract
(`native_frame/mod.rs`'s doc: "the Vulkan importer must duplicate or
consume the fd, then close it"). The producer dup'd them at the
`buffer-rendered` seam; the importer takes ownership on success and Vulkan
closes them on drop of the imported texture.

**One nuance:** if `import_dmabuf_image` returns `Err`, the fds may or may
not have been consumed depending on where it failed. The existing
importer's behavior on Err needs to be honored — if it's "fds are
ours-to-close on Err", the test calls `close_frame_fds` on the original
DmaBufImage before failing. If it's "fds are consumed regardless", we
don't. The implementation reads the importer's contract at write time and
follows it. (The `dmabuf_roundtrip.rs` test is the reference for the
correct handling.)

## File structure

- New: `scrying/tests/wpe_to_vulkan_roundtrip.rs` — one `#[test]
  #[ignore]` integration test gated on Linux + wpe.
- No other files change.

## Dependencies

None new. The wpe-gated `WpeProducer` API is in place as of `c397210`.
`wgpu` and `pollster` are existing dev-deps used by
`tests/dmabuf_roundtrip.rs`. The test reuses the same wgpu device path the
gbm roundtrip uses.

## Test infrastructure considerations

- **Gating.** `#![cfg(all(target_os = "linux", feature = "wpe"))]` at the
  top of the test file so it compiles out cleanly on non-Linux and
  non-wpe builds. The single test inside is marked `#[ignore]` so manual
  runs (`cargo test --features wpe --test wpe_to_vulkan_roundtrip --
  --ignored`) opt in to the live runtime.
- **Independent WebKit init.** Because this is a separate binary, the
  unit-test smoke and this round-trip can each construct their own
  producer without colliding. They CANNOT both run in the same
  invocation of any single test binary, but they can both be invoked in
  back-to-back `cargo test` commands.
- **One ignored test in this binary.** Same one-per-process discipline:
  only one runtime-WPE test in this binary. If pixel correctness later
  needs to ship, it goes in this same test or a different binary, never
  as a sibling test in this one.

## Empirical unknowns

These get acknowledged in the test code with comments, not pre-decided:

1. **Does `import_dmabuf_image` return `Ok` on the AMD-DCC 2-plane
   buffer?** Likely no (Vulkan refuses the modifier without multi-plane
   info, or returns garbage). The test fails loudly on Err with the
   message captured — that IS the data point that triggers the
   Phase-4a-expansion decision.
2. **Exact signature/path of `import_dmabuf_image`.** Read at write time
   from `scrying/src/native_frame/dmabuf.rs` and the existing
   `dmabuf_roundtrip.rs` test.
3. **fd ownership on Err.** Read at write time from the importer's
   doc + the gbm roundtrip's handling.

## Deferred / explicitly not done

- Multi-plane DRM-modifier import in `dmabuf.rs` (Phase 4a expansion).
- `WPEDisplay` subclassing for format negotiation.
- Pixel sampling / color-equivalence assertion.
- A second `#[ignore]`d test in this binary.
- Any modification to the existing producer / FFI / nav surface.

## Followups this test enables / informs

- If the test passes today: the producer → importer pipeline works
  end-to-end; pixel-correctness sampling becomes the next honest
  expansion.
- If the test fails on AMD-DCC: the Err message + observed modifier
  inform the Phase 4a expansion design.
- Either way, the harness exists and remains useful when the importer
  expands.

# Improvement backlog

Prioritized, actionable follow-ups for `scrying`, distinct from the descriptive
[capability parity matrix](../docs/parity-matrix.md) (which documents *what works
where*). This lists *what to do next* and why. Most items are Linux/macOS work
(do them on a Fedora/Mac host); the cross-repo interop convergence is in flight
on Windows.

## In flight (cross-repo, Windows-verifiable)

- **Converge on `grafting` as the shared native-texture interop crate.** `welding`
  and `scrying` each carry their own DX12 / Metal / Vulkan-DMABUF import + sync
  (welding's `vulkan_dmabuf` is literally "Ported from `wgpu-graft/grafting`"), so
  the same subtle GPU-sync logic is maintained in triplicate. `grafting` is already
  the standalone, wgpu-version-flexible interop core. Step 1 done: `grafting`'s GL
  path is now behind a default-on `gl` feature so non-GL consumers can depend on it
  leanly (wgpu-graft `953da76`). Next: `welding` and `scrying` delegate their
  shared-texture import to `grafting`, keeping only their producer-specific frame
  acquisition. NB: reconcile `welding`'s D3D12 cache-flush against `grafting`'s
  synchronizer model so the convergence does not regress correctness.

## Fedora / macOS work

- **WebKitGTK 6.0 input fidelity.** webkit6 input is JS-event-synthesis only, so
  events arrive `isTrusted === false`; pages that gate on it (autoplay,
  `requestFullscreen`, some click-fraud defenses) reject them. GTK 4 removed
  `gtk_main_do_event`, so the GTK 3 native-dispatch primary has no analog. Fix:
  a `gtk4::GestureClick` / `GdkSurface` event-queue path. See parity-matrix
  `[^wk6-input-js-only]`. Highest real-site-compat leverage.
- **Process-crash recovery.** Neither webkit6 nor WPE wires a
  `web-process-terminated` recovery handler, and WPE couples one `WebKitWebView`
  to one process-global headless display (a second producer SIGABRTs/hangs). Add a
  recovery handler and document/enforce the one-producer-per-process (or
  subprocess-per-producer) model. See `[^wpe-process]`, `[^wk6-process]`.
- **IME observability parity on Windows + macOS.** GTK and WPE surface
  focus/change/blur through the `scryIme` handler; WebView2 (`WM_IME_*`) and
  WKWebView do not surface them back to the host. Bring Win/Mac up to the GTK/WPE
  observability shape. See `[^win-ime]`, `[^mac-ime]`.
- **Close the test-coverage `?` rows.** Touch (every backend), IME (Win/Mac), and
  find/pdf/profile (webkit6) are all "no test flag exercises it." Build out the
  cross-backend trait-test infra so guesses become verified ✔/✘ rows.

## Upstream-blocked (track, not actionable here)

- **WPE Vulkan DCC pixel-correctness.** Blocked on wgpu landing a `texture_from_raw`
  initial-state API; scrying's spec-correct foreign-queue acquire barrier is
  already written and stays dormant until then. See `[^wpe-dcc]`.
- **WebKitGTK 6.0 zero-copy import.** Blocked on GTK 4 exposing `GdkDmabufTexture`
  plane accessors (only the builder side is public through 4.22), so webkit6 falls
  back to a full CPU pixel download per frame. See `[^wk6-dmabuf]`.

## Host-side (mere repo, not this crate)

- **Generalize meerkat's scry host beyond Windows.** `meerkat`'s `scrying_host`
  wires only the WebView2 (Windows) producer (the X1 scope), though the crate
  supports WKWebView and WebKitGTK/WPE. Generalizing `windows_pool` into the other
  producers is the host-side X4, and is the shape `WeldHost`/`GraftHost` should
  share. Tracked in mere.

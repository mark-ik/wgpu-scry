# demo-scrying-winit

Cross-platform winit/wgpu smoke host for `scrying`.

This demo intentionally stays small: the host creates a window and wgpu device, then asks `scrying` to report the selected system-webview backend, platform producer/config aliases, preferred surface mode, supported frame kinds, and fallback status for the current target. It is the dependency-selection proof that Windows selects WebView2, macOS selects WKWebView, and Linux selects the compile-only WPE alias without pulling platform FFI into this cross-platform probe.

For platform-specific runtime coverage, use [`../demo-win`](../demo-win/) on Windows and [`../demo-mac`](../demo-mac/) on macOS.

Run:

```bash
cargo run -p demo-scrying-winit
```

For a quick non-interactive smoke:

```bash
cargo run -p demo-scrying-winit -- --probe-only
```

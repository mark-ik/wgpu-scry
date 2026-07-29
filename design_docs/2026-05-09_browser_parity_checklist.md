# Browser-class parity checklist

Companion to [`2026-05-07_platform_ceilings.md`](2026-05-07_platform_ceilings.md).
That doc covers per-platform capability ceilings, GPU sync upgrade
paths, and the macOS producer's slice-by-slice landing notes with
implementation specifics. **This** doc is the flat, scannable
checklist of everything an embeddable system-WebView library needs
to be a "real" browser-class building block — something a tabbed
browser shell (e.g. [`merely-made/mere`](https://github.com/merely-made/mere))
can be built on top of without going around it for any meaningful
capability.

The split exists because the platform doc was getting hard to
skim at a glance — every line of "what's left to ship" was
embedded in a paragraph of macOS impl notes. A reader who just
wants "are we done yet?" should be able to read this file in
under a minute.

Windows-specific target depth lives in
[`2026-05-11_windows_webview2_target.md`](2026-05-11_windows_webview2_target.md):
it audits the current WebView2 producer, states how good the integration can
be, and tracks the Windows implementation lane.

Status values, applied per row:

- ✅ shipped on macOS (0.4.x)
- ⏳ outstanding — public-API path exists, just hasn't been wired
- 🚫 blocked by platform SPI — would need `_WK*` private API or
  similar; punted unless a downstream consumer makes the case
- 📐 design-only — the capability's surface is a documentation
  question rather than a code question (e.g. "how does a host
  attach DevTools")
- n/a — capability doesn't apply at this layer (browser-shell
  consumers handle it themselves)

Cross-platform: every row also notes whether the slice would need
matching work on Windows (WebView2 + WGC) and Linux (WPE primary,
WebKitGTK fallback). "?" means we haven't audited the equivalent
yet.

---

## Navigation & lifecycle

| Capability | macOS | Windows | Linux | Notes |
| --- | --- | --- | --- | --- |
| History controls (reload / stop / back / forward) | ✅ | ✅ | ? | Trait methods on `WebSurfaceProducer` |
| New-window / popup intercept | ✅ | ✅ | ? | `NavigationEvent::NewWindowRequested { url }`; Windows covered by `demo-win --popup-test` |
| Process-failure recovery | ✅ | ✅ | ? | `NavigationEvent::ContentProcessTerminated`; Windows covered by `demo-win --process-test` |
| Tab-state serialize / restore | ✅ | 🚫 | ? | macOS round-trips WebKit's opaque `interactionState` bytes. Windows has same-named methods for cross-platform call sites, but WebView2 exposes no equivalent opaque blob: `serialize_interaction_state()` returns `None`, `restore_interaction_state(...)` returns `Unsupported`, and hosts should restore URL/history/form state explicitly |
| Page Visibility / occlusion sync | ✅ | ✅ | ⏳ | `set_visible(bool)` cascades through `NSView::setHidden:` on macOS and `ICoreWebView2Controller::SetIsVisible` on Windows. Windows page-observed state is covered by `demo-win --visibility-test`. Distinct from the SPI-only `_setSuspended:` / hard pause paths |
| Throttling control (hard pause) | ✅ | 🚫 | ⏳ | `WebSurfaceSettings::inactive_scheduling_policy` (`Suspend` / `Throttle` / `None`) → `WKPreferences.inactiveSchedulingPolicy` on macOS 14+ / iOS 17+. Windows WebView2 exposes `SetIsVisible` for Page Visibility but no public inactive-scheduling / hard-pause equivalent; `apply_settings` returns `Unsupported` for this field and `demo-win --browser-test` covers that ceiling. SPI alternative (`_suspendPage:` for older macOS) and the wider SPI evaluation live in [`2026-05-09_spi_evaluation.md`](2026-05-09_spi_evaluation.md) |

## Input forwarding

| Capability | macOS | Windows | Linux | Notes |
| --- | --- | --- | --- | --- |
| Mouse + scroll wheel (with modifiers) | ✅ | ✅ | ? | Scroll wheel carries location + `CGEventFlags` |
| Pointer / touch / pen synthesis | ✅ | ✅ | ? | macOS has no public direct-touch synthesis API — events arrive at JS as `pointerType: "mouse"`; Windows uses WebView2 `SendPointerInput` |
| Keyboard + IME (composition) | ✅ | 🟡 | ? | Pure Windows visual-hosted WebView2 remains blocked for Win32/OS-native keyboard routing: `demo-win --keyboard-test` proves DOM focus plus physical-style `SendInput` still times out. `demo-win --cdp-input-test` now proves a no-overlay DOM input lane: `Input.dispatchKeyEvent`, `Input.insertText`, `Input.imeSetComposition`, `Runtime.evaluate`, and `ExecuteScript` all produced DOM input. `demo-win --ime-bridge-test` proves the hardened host-owned IME bridge: focused editable/caret state -> winit IME candidate area -> CDP composition/commit, with source-side password suppression, textarea scroll/multiline caret coverage, current offset/DPI mapping, and blur/navigation cancellation. Full TSF text-store parity remains future work. Host accelerator events are covered separately: `demo-win --accelerator-test` observes WebView2 `AcceleratorKeyPressed` in pure CompositionController mode. Window-to-Visual proves ASCII DOM input and WGC -> wgpu import, including three animated pages with avg parent capture/import around 103ms, but the stricter smokes report visible `Chrome_WidgetWin_0` child HWNDs, so it remains rejected as a no-overlay target |
| Cursor-change events (host mirrors WebKit cursor) | ✅ | ✅ | ? | macOS polls `NSCursor.currentSystemCursor`; Windows listens to WebView2 `CursorChanged` |
| Drag-and-drop *in* (file / URL → page) | ✅ | ✅ | ⏳ | macOS and Windows emit `NavigationEvent::DropDetected` via a document drop bridge; Windows also has OLE `IDataObject` drag-enter / over / leave / drop helpers. Trait-level `send_drag_input` reports that Windows requires the concrete OLE data-object path. Windows bridge observability is covered by `demo-win --drop-test` |
| Drag-and-drop *out* (page content → host) | 🚫 | ? | ⏳ | macOS path is `_WK*` SPI |
| Context-menu interception | ✅ | ⏳ | ? | JS user-script + `WKScriptMessageHandler` always fires `NavigationEvent::ContextMenuRequested` (observability); engine-default menu suppression is gated on `window.__scryingSuppressContextMenu` and respects `WebSurfaceSettings::default_context_menus_enabled` via `evaluateJavaScript:` |

## Capture & rendering

| Capability | macOS | Windows | Linux | Notes |
| --- | --- | --- | --- | --- |
| Overlay vs imported-texture mode | ✅ | ✅ | ? | Capability-driven via `WebSurfaceMode` |
| Source-rect crop to webview region | ✅ | ✅ | ? | macOS via per-frame Metal blit; Apple ignores `sourceRect` for single-window filters |
| Chrome (title-bar) offset honored | ✅ | n/a | n/a | Window-frame top-left coords; macOS-specific because SCK captures full window |
| Resize correctness (dim-match guard) | ✅ | ✅ | ? | macOS rejects stale pre-resize SCK samples; Windows drops WGC frames whose `ContentSize` no longer matches the producer size after resize/restart churn |
| DPI awareness across monitor moves | ✅ | ✅ | ? | macOS backing-scale-change observer re-applies `config.size`; Windows routes winit `ScaleFactorChanged` through renderer resize and covers the capture restart path with `--scale-test` |
| Capture cadence probe (`CaptureMetrics`) | ✅ | ✅ | ⏳ | macOS exposes `samples_received` / `samples_consumed`; Windows also reports `stale_frames_dropped` for resize/restart diagnostics |
| Cross-API GPU sync (explicit fences) | ✅ | ✅ | ✅ | macOS producer encodes `MTLCommandBuffer::encodeSignalEvent_value` and producer-side waits before handoff; Windows supports `D3D12_FENCE_FLAG_SHARED` when the host supplies a shared fence handle, with the older barrier path as fallback. Architectural follow-up on macOS: consumer-side `encodeWaitForEvent:value:` via the wgpu-hal Metal escape removes the CPU stall entirely (~1ms per acquire saved) |
| Color management — Display P3 SDR | ✅ | 🚫 | ⏳ | macOS exposes `ColorPipeline::DisplayP3`; Windows WebView2/WGC currently reports fixed `ColorPipeline::Srgb` + `Bgra8Unorm` because the public composition capture path exposes no P3 color-space control |
| Color management — HDR / 16-float | ✅ | 🚫 | ⏳ | macOS exposes `ColorPipeline::Hdr16f`; Windows WebView2/WGC currently has no public HDR / 16-float capture target for this path |
| Pre-composition extraction | 🚫 | 🚫 | ⏳ | macOS would need direct `CALayer.contents` access pre-WindowServer-composite; Windows would need Microsoft to expose pre-DComp WebView2 textures; WPE is the strategic path that can expose pre-composition buffers natively |
| Sub-iframe / sub-frame capture | 🚫 | 🚫 | ⏳ | `WKWebView` is a single composition root; per-frame access is `_WK*`. Long-term WPE-first story (WPE exposes per-view buffers natively) |

## Settings / environment

| Capability | macOS | Windows | Linux | Notes |
| --- | --- | --- | --- | --- |
| Settings application (zoom / UA / JS / inspectable) | ✅ | ✅ | ? | `apply_settings(&WebSurfaceSettings)` |
| Custom URL schemes | ✅ | ✅ | ? | `WkWebViewProducer::new_with_url_schemes` on macOS; `WebView2CompositionProducer::register_virtual_host_handler` on Windows, covered by `demo-win --routing-test` |
| Per-profile data store | ✅ | ✅ | ? | `WKWebsiteDataStore::dataStoreForIdentifier:` (macOS 14+) |
| Incognito / non-persistent profile | ✅ | ✅ | ? | `WkWebViewProducerConfig::non_persistent` on macOS; `WebView2CompositionConfig::non_persistent` creates an InPrivate CompositionController on Windows and is covered by `demo-win --incognito-test` |
| Multi-instance verification (cross-talk isolation) | ✅ | 🟡 | ? | Windows profile smoke proves sequential producer recreation with the same `user_data_dir`; `demo-win --multi-view-test` proves simultaneous two-producer composition on separate HWNDs. Same-HWND composition remains unsupported in the current demo setup |
| Cookie store (read / write / delete) | ✅ | ✅ | ? | Wraps `WKHTTPCookieStore` on macOS and WebView2 `ICoreWebView2CookieManager` on Windows |
| Cookie / storage *change events* | ✅ | 🟡 | ⏳ | macOS uses `cookiesDidChangeInCookieStore:` for page writes, `Set-Cookie` headers, and host writes. Windows fires pulses for host `set_cookie` / `delete_cookie`, page-side `document.cookie` writes, and native `Set-Cookie` response headers via `WebResourceResponseReceived`; broader storage-change observation remains open |

## Browser-shape UX

| Capability | macOS | Windows | Linux | Notes |
| --- | --- | --- | --- | --- |
| Find-in-page (with options) | ✅ | ✅ | ? | macOS: `find_in_page` + `poll_find_match`; Windows: native `ICoreWebView2Find` with match count, covered by `demo-win --find-test` |
| Page-to-PDF rendering | ✅ | ✅ | ? | macOS: `request_pdf` + `poll_pdf`; Windows: native `PrintToPdfStream`, covered by `demo-win --pdf-test` |
| Print / `Cmd+P` (interactive) | ✅ | ✅ | ⏳ | macOS uses `NSPrintOperation`; Windows `print()` invokes WebView2's browser print UI via `ShowPrintUI`. Interactive cancellation is host/user driven and not smoke-tested |
| Auth challenges (events + host-driven disposition) | ✅ | ✅ | ? | Option A (events) + Option B (`set_auth_handler`) both shipped; Windows covers WebView2 `BasicAuthenticationRequested` with `demo-win --auth-test` |
| Auth during downloads (mid-stream / post-promotion) | ✅ | 🟡 | ? | `WKDownloadDelegate::download:didReceiveAuthenticationChallenge:` routes through the same shared auth handler as page-load auth. Windows WebView2 exposes Basic auth at the WebView level; active download URL matching reports `AuthSource::Download` when possible |
| Permission handlers (camera / mic / orientation) | ✅ | ✅ | ? | `set_permission_handler` returns `Allow` / `Deny` / `Prompt`; Windows maps camera / microphone / sensor prompts and is covered by `demo-win --permission-test` |
| WebRTC capture lifecycle observability | ✅ | ✅ | ⏳ | JS user-script monkey-patches `navigator.mediaDevices.getUserMedia`, tracks `track.ended`; emits `NavigationEvent::MediaCaptureStateChanged { audio_active_tracks, video_active_tracks }`. Windows bridge/event path is covered by `demo-win --media-test` |
| Title-changed notifications | ✅ | ✅ | ? | KVO on `WKWebView::title` |
| Downloads pipeline (id-correlated, host destination, cancel, resume) | ✅ | ✅ | ? | Windows has `DownloadId`, `set_download_handler`, `cancel_download`, live `pause_download` / `resume_download(id)` / `can_resume_download(id)`, progress, finish/cancel events, and `demo-win --download-test`. Offline `resume_data` blobs remain macOS/WebKit-only; Windows cancellations report `resume_data: None` |
| Context-menu requested event | ✅ | ✅ | ? | macOS and Windows emit `NavigationEvent::ContextMenuRequested`; Windows registers native WebView2 `ContextMenuRequested` and a deterministic document bridge covered by `demo-win --context-test` |
| Content blocking (`WKContentRuleList` / AdBlock-shape) | ✅ | 🚫 | ⏳ | macOS has `compile_and_apply_content_rule_list(id, json)` / `clear_all_content_rule_lists`; WebView2 has request events and filters, but no public `WKContentRuleList`-style compiled rule-list engine. Windows currently exposes virtual-host app routing only, not a portable adblock-shape blocker |
| Spellcheck / autocorrect controls | ✅ | 🚫 | ? | macOS best-effort `spellcheck_override` injects standard `spellcheck` attributes. The bound WebView2 API exposes no producer-level spellcheck/autocorrect setting; page-authored `spellcheck` attributes remain the portable path |
| Autofill / Keychain integration | ✅ | ✅ | ⏳ | macOS is system-driven through Keychain/AppKit. Windows exposes `set_password_autosave_enabled` and `set_general_autofill_enabled` over WebView2 `ICoreWebView2Settings4`; `demo-win --browser-test` toggles both. Profile isolation follows the selected WebView2 user-data directory / InPrivate controller |
| DevTools / Web Inspector remote attach | 📐 | ✅ | ? | macOS has `setInspectable(true)` wired (macOS 13.3+) but the Safari → Develop attach flow needs documentation; Windows has `open_devtools_window` |

---

## Notes on coverage

- **macOS** is the most complete column because it's the primary
  platform pushed in 0.4.x. The remaining ⏳ rows are public-API
  paths that just haven't been prioritized yet; the 🚫 rows are
  the genuine ceiling.
- **Windows** is a real WebView2 composition producer, but its
  remaining work is browser-shape API completion rather than frame
  production. `✅` rows were checked against
  [`webview2_composition_producer.rs`](../scrying/src/webview2_composition_producer.rs);
  `⏳` rows are public-WebView2-shaped follow-ups; `?` rows are still
  unaudited or need a design call. Runtime assertions should land in
  [`demo-win`](../demo-win/) rather than the cross-platform selector
  smoke; it now has `--scripted`, `--browser-test`, and
  `--cookie-test` / `--profile-test` one-shot modes for the shipped
  WebView2 slices. The Windows profile smoke recreates the producer
  because one CompositionController target can be attached to a given
  HWND at a time. The focused target and lane live in
  [`2026-05-11_windows_webview2_target.md`](2026-05-11_windows_webview2_target.md).
- **Linux** rows are largely "?" because the WPE producer is
  unstarted (out-of-band slice; see roadmap in
  [`2026-05-07_platform_ceilings.md`](2026-05-07_platform_ceilings.md)).
  WPE structurally wins on a couple of capture rows (per-view
  Vulkan buffers, native VkSemaphore) but loses on the embedding-
  shell rows that require system integration.

## Adding a row

When the team picks up a new ⏳ slice and lands it:

1. Flip the status in the relevant table here.
2. Move the macOS impl detail (the *paragraph* describing the
   API surface, edge cases, and any threading notes) to the
   matching section of
   [`2026-05-07_platform_ceilings.md`](2026-05-07_platform_ceilings.md) —
   either inside the "Browser-class consumer roadmap" Recently
   Shipped list (if it's a browser-shape capability) or under
   the per-platform ceilings section (if it touches the capture
   or rendering pipeline).
3. Cross-link both directions so a reader landing on either
   doc finds the other.

Don't duplicate impl detail across the two files. The platform
doc is the single source of truth for "how it works on this
platform"; this doc is the index.

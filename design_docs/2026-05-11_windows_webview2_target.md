# Windows WebView2 integration target

**Status:** target and audit doc, split from
[`2026-05-07_platform_ceilings.md`](2026-05-07_platform_ceilings.md) on
2026-05-11.

This document sets the Windows target for scrying's system-webview
integration. It is deliberately Windows-specific: the cross-platform index
stays in
[`2026-05-09_browser_parity_checklist.md`](2026-05-09_browser_parity_checklist.md),
and the platform ceiling overview stays in
[`2026-05-07_platform_ceilings.md`](2026-05-07_platform_ceilings.md).

## Target

Windows should be a production-quality WebView2 embedding backend for a
browser-shaped host: a tabbed shell should be able to own navigation,
input, profiles, downloads, permissions, auth prompts, context menus, and
capture diagnostics through scrying without reaching around the producer for
normal browser chrome work.

The rendering target is more constrained: Windows has two public WebView2
paths that matter. `ICoreWebView2CompositionController` gives direct visual
hosting and a robust WGC texture path, but not browser-grade keyboard/IME.
Window-to-Visual hosting keeps the normal controller/input model while routing
WebView2 output through the parent HWND. The bounded
`demo-win --window-to-visual-test` proof shows ASCII DOM keyboard input and
WGC -> wgpu import passing in that mode, but the stricter post-navigation check
reports a visible `Chrome_WidgetWin_0` child HWND. The three-page
`--window-to-visual-multi-test` also animates and imports within budget, but
reports three visible WebView child HWNDs. Window-to-Visual is therefore
**closed** as a production target (see Non-Goals); the smokes remain as
diagnostics of WebView2's visible-child-HWND behavior.

As of the 2026-05-13 audit, the accepted no-overlay rendering path remains pure
CompositionController. For the **single-WebView-per-HWND** case (scrying's
core envelope), both real hardware keyboard and programmatic Win32 `SendInput`
work natively (verified 2026-05-12 / 2026-05-13); `--keyboard-test` PASSes 3/3.
Multi-pane keyboard via child HWNDs is unverified (see "Multi-WebView Scope").
Host-driven IME and DevTools-shaped automation are routed through WebView2's
public CDP bridge (`Input.dispatchKeyEvent`, `Input.insertText`, and
`Input.imeSetComposition`). Host chrome
accelerators are surfaced through WebView2 `AcceleratorKeyPressed`; the
bounded `demo-win --accelerator-test` smoke observes F3 in the pure
CompositionController path. The first host-owned IME bridge is now proven by
`demo-win --ime-bridge-test`: WebView2 reports focused editable caret geometry,
the winit host enables native IME and sets the candidate area from that rect,
and Scrying applies composition/commit through CDP. The bridge now suppresses
password-like editables at the document bridge before selection/caret metadata
is emitted, validates textarea scroll/multiline caret placement, maps the rect
through the current composition offset and host scale factor, and cancels active
composition on blur, navigation start, and content-process failure. The bridge
now also reports the raw DOM `input_type`, `inputmode`, and `autocomplete`
attributes alongside caret geometry, and `TextInputState::purpose()` derives a
coarse [`InputPurpose`] (Text / Search / Email / Url / Tel / Numeric / Decimal /
Password / OneTimeCode / Disabled) so thin hosts can pattern-match without
re-implementing the DOM precedence rules. Full TSF text-store parity remains a
larger Windows-specific follow-up. Raw Win32 message posting remains a
diagnostic probe only.

## How Good Can It Be?

Very good for app/browser integration:

- Navigation and chrome ownership can be first-class: history, title,
  popups, crashes, auth, permissions, downloads, DevTools, settings, and
  cookie/profile state all have public WebView2-shaped paths.
- Input is strong in pure visual hosting **for the single-WebView-per-HWND
  case**: mouse, wheel, touch, pen, focus, cursor, OLE drag-in, real
  hardware keyboard, and programmatic Win32 `SendInput` all work
  (verified 2026-05-12 / 2026-05-13; `--keyboard-test` PASSes 3/3 after
  removing a smoke bug where a `producer.send_mouse_input` click was
  shifting DOM focus off the target). The composition controller does not
  expose a programmatic keyboard-send API equivalent to its
  `SendMouseInput` / `SendPointerInput`, but raw Win32 `SendInput` after
  host-side `SetForegroundWindow` reaches the focused DOM element
  directly. CDP (`--cdp-input-test`) remains useful for DevTools-shaped
  automation and host-driven IME ownership but is not the primary lane
  for synthetic keyboard. **Multiple WebViews in one OS window also work**
  via a shared `CompositionRoot` — mouse and real keyboard both route to
  the correct pane (see "Multi-WebView Scope" below).
- Texture handoff can be reliable: the producer already supports explicit
  D3D12 fence sync when the host passes a shared fence handle, with the old
  barrier/cache fallback retained.
- Profile/cookie behavior can be browser-like for persistent stores:
  `user_data_dir` is the WebView2 profile identity and persistent cookies
  survive producer recreation.

But it cannot be a raw WebView renderer:

- WebView2 does not expose pre-DComp textures. WGC observes the composed
  visual, so latency is at least one compositor frame and often 1-2 frames.
- Sub-frame / iframe capture is out of reach without upstream API additions.
- Capture while the visual is detached or hidden from the composition tree is
  not a public contract.
- A single HWND cannot host two independent `DesktopWindowTarget` roots
  (`DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`). This is a real DComp limit,
  not a scrying-API artifact — but it is **not** a multi-WebView blocker:
  multiple producers share one `DesktopWindowTarget` via `CompositionRoot` /
  `new_attached` (see "Multi-WebView Scope"). It only means you cannot give
  each producer its own target on the same HWND.

The right Windows ambition is: **excellent browser-host semantics and a
diagnosable, synchronized post-composition texture stream**.

## Multi-WebView Scope

Scrying's value is the **no-overlay composition model**: WebView visuals are
composited directly onto the host's HWND via DComp (the live display is
zero-copy), and WGC capture is the additive path for hosts that want the
pixels as a GPU texture — instead of a wrapped HWND the host has to z-order
around (the wry shape).

**Multiple WebViews in one OS window works.** Verified 2026-05-14 via
`--multi-pane-input-test`: two WebViews hosted in one HWND, side by side, with
mouse and real hardware keyboard both routing to the correct pane.

The architecture is **one shared `CompositionRoot` per HWND, N producers
attached to it**:

- Windows allows exactly one `DesktopWindowTarget` per HWND — a second
  `CreateDesktopWindowTarget` fails with
  `DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED` (directly proven 2026-05-13,
  not just inherited from `--profile-test`). So multi-pane does **not** give
  each producer its own target.
- Instead, `CompositionRoot` owns the one-per-HWND `Compositor` +
  `DesktopWindowTarget` + root visual. `WebView2CompositionProducer::new()`
  builds a private `CompositionRoot` and delegates to `new_attached()`;
  `new_attached(&Arc<CompositionRoot>, config)` attaches a producer as a
  per-pane container visual (child of the shared root, positioned by
  `config.offset`). Each producer keeps its own WebView2 environment,
  controller, and capture pipeline; only the target/compositor/root is shared.
- Because there is one HWND and one `DesktopWindowTarget`, WebView2's keyboard
  chain has a single HWND to track — the same topology single-pane succeeds
  with. Click-to-focus (`move_focus` on the clicked pane's producer) hands DOM
  focus to that pane; real keyboard then routes to whichever WebView holds DOM
  focus, with no cross-talk between panes.

Verified working (2026-05-14):

- ✅ N WebViews rendering in one HWND, no wrapper child HWNDs, no native
  overlay.
- ✅ Mouse routing — host hit-tests the cursor against per-pane rects and
  forwards `send_mouse_input` to the matching producer with pane-local coords.
- ✅ Real hardware keyboard → the focused pane's DOM, correct pane only
  (`abc` to pane 0, `def` to pane 1, zero leakage).

Not yet exercised: mouse wheel in multi-pane (wired, untested), IME in
multi-pane, programmatic `SendInput` across panes (the automation lane — and
per the single-pane finding, that's tests-only, not user-facing), and resize
reflow (panes hold their startup rects if the window resizes).

The earlier framing — that multi-pane needed wrapper child HWNDs and its
keyboard story was unverified — is superseded. The wrapper-HWND approach
(`--composition-focus-hwnd-test`) is retained only as a documented negative
result: STATIC child HWNDs are inert to clicks, and even with programmatic
focus the nested-HWND topology breaks WebView2's keyboard delivery. The shared
`CompositionRoot` path is the multi-pane answer.

**serval** is still the long-term engine for web content rendered directly
into mere's scene graph — but scrying now covers single- *and* multi-WebView
hosting for the bridge months, not just single-WebView.

## Audited Current State

Audited against
[`../scrying/src/webview2_composition_producer.rs`](../scrying/src/webview2_composition_producer.rs),
[`../scrying/src/lib.rs`](../scrying/src/lib.rs), and
[`../demo-win/`](../demo-win/).

### Shipped

- Composition path: `ICoreWebView2CompositionController` attached to a
  WinComp visual, `GraphicsCaptureItem::CreateFromVisual`,
  `Direct3D11CaptureFramePool`, one shared D3D11 destination texture per
  emitted frame, and `NativeFrame::Dx12SharedTexture` handoff.
- Window-to-Visual diagnostic path: normal `ICoreWebView2Controller` creation
  with
  `COREWEBVIEW2_FORCED_HOSTING_MODE=COREWEBVIEW2_HOSTING_MODE_WINDOW_TO_VISUAL`,
  focused DOM input receiving `SendInput` text, and parent-HWND WGC capture
  imported into wgpu. The stricter single- and three-page smokes reject this as
  a no-overlay target because the live pages expose visible `Chrome_WidgetWin_0`
  child HWNDs.
- Explicit GPU sync:
  `WebView2CompositionConfig::with_dx12_fence_synchronizer` opens a
  host-created D3D12 shared fence on the producer side; emitted frames carry
  `SyncMechanism::ExplicitFence`, a monotonic fence value, and a fresh
  resource. Imported-texture construction rejects a missing synchronizer.
- Lifecycle and navigation: inline HTML, URL navigation, reload, stop,
  back/forward, `CanGoBack`, `CanGoForward`, `NavigationStarting`,
  `SourceChanged`, `NavigationCompleted`, document-title events, and
  `ProcessFailed` mapping to `NavigationEvent::ContentProcessTerminated`.
- Input: mouse and wheel via `SendMouseInput`; touch/pen/pointer via
  `SendPointerInput`; focus via `MoveFocus`; cursor-change events; and OLE
  `DragEnter` / `DragOver` / `DragLeave` / `Drop`. **Real hardware keyboard
  AND programmatic Win32 `SendInput` both work in pure CompositionController**:
  hardware keystrokes from a user-clicked-in field produce normal `keydown` /
  `input` events on the focused element with zero scrying involvement
  (verified 2026-05-12 by `dom-keydown` / `dom-input` listeners reporting
  `keyboard-smoke:a/b/c` while no `[ime]` lines fired); `SendInput` after
  host-side `SetForegroundWindow` lands on the focused DOM element directly
  and `--keyboard-test` PASSes 3/3 (verified 2026-05-13). The
  long-standing `--keyboard-test` timeout was a smoke bug — a
  `producer.send_mouse_input` click before SendInput shifted DOM focus off
  the target — not a WebView2 ceiling. The CDP bridge
  (`send_keyboard_input` →  `Input.dispatchKeyEvent`, `Input.insertText`,
  `Input.imeSetComposition`) is therefore for DevTools-shaped automation
  and host-driven IME ownership, not for synthetic keyboard fallback. WebView2
  `AcceleratorKeyPressed` maps to `NavigationEvent::AcceleratorKeyPressed` for
  host shortcut/chrome policy. A document-start text-input bridge maps focused
  editable/caret state to `NavigationEvent::TextInputFocused`,
  `TextInputChanged`, and `TextInputBlurred` so the host can own native IME
  enablement and candidate placement. The bridge suppresses password-like
  editables before emitting rect/selection state, reports textarea caret rects
  through scroll/multiline layout, and cancels composition on blur/navigation
  boundaries. Password suppression is a "host never observes password state"
  contract (no caret rect, selection, or value-length signal), not a
  "password rejects all typing" contract: IME-mediated keystrokes still reach
  the focused DOM password element via CDP `insert_text` because a user who
  needs an IME to type their password must be able to. `TextInputState` carries the raw DOM `input_type`, `input_mode`,
  and `autocomplete` strings; `TextInputState::purpose()` derives an
  `InputPurpose` enum for hosts that need a single coarse switch instead of
  re-deriving the DOM precedence rules. Raw Win32 `WM_*` forwarding is retained
  only as a diagnostic probe because it does not reach DOM input.
- JS interop: host-to-page strings via `PostWebMessageAsString`, page-to-host
  strings via `WebMessageReceived`, and document-start script injection.
- New-window / popup ownership: WebView2 `NewWindowRequested` maps to
  `NavigationEvent::NewWindowRequested { url }`, and the producer suppresses
  the default popup so the host owns tab creation.
- App-owned content routing: `register_virtual_host_handler(host, handler)`
  maps `https://{host}/...` requests through WebView2 `WebResourceRequested`
  and serves `UrlSchemeResponse` bodies/headers without network access.
- Settings: zoom, user agent, JavaScript enablement, DevTools enablement,
  default context menus, accelerator keys, and WebView visibility with
  page-observed Page Visibility proof.
- Cookies and profiles: `ICoreWebView2CookieManager` request / set / delete,
  best-effort cookie-change pulses for host mutations and page-side
  `document.cookie` writes, plus persistent profile identity through
  `user_data_dir`, and `WebView2CompositionConfig::non_persistent()` for
  InPrivate / non-persistent controllers.
- Downloads: WebView2 `DownloadStarting` maps to the shared download event
  family with `DownloadId`, host destination decisions, progress,
  completion/cancellation, `cancel_download`, and live `pause_download` /
  `resume_download` / `can_resume_download` operation control. WebView2 does
  not expose a portable offline resume-data blob through this path.
- Auth: WebView2 `BasicAuthenticationRequested` maps to
  `NavigationEvent::AuthChallenged` and `set_auth_handler` can provide HTTP
  Basic credentials. Challenges whose URL matches an active download operation
  are classified as `AuthSource::Download`; other WebView-level challenges
  remain `AuthSource::Page`.
- Permissions: WebView2 `PermissionRequested` maps camera, microphone, and
  sensor-like prompts through `set_permission_handler`.
- Snapshots: `ICoreWebView2::CapturePreview` PNG snapshots.
- Runtime proof in `demo-win`: `--scripted`, `--browser-test`,
  `--cookie-test`, `--profile-test`, `--incognito-test`, `--popup-test`,
  `--routing-test`, `--process-test`, `--download-test`, `--auth-test`, and
  `--permission-test`, `--visibility-test`, `--multi-view-test`, and
  `--capture-test`, `--accelerator-test`, `--ime-bridge-test`, plus
  `--window-to-visual-test` and `--window-to-visual-multi-test` as failing
  no-overlay diagnostics for the normal-controller path.

### Partially Shipped or Needs Runtime Proof

- Keyboard and IME: real hardware keyboard works in pure CompositionController
  for normally-focused DOM inputs (verified 2026-05-12 by interactive run with
  `dom-keydown` / `dom-input` listeners on the probe page). Programmatic
  Win32 `SendInput` ALSO works after host-side `SetForegroundWindow`:
  `--keyboard-test` PASSes 3/3 (verified 2026-05-13). The earlier
  long-standing keyboard-test timeout was a smoke bug — the smoke called
  `producer.send_mouse_input` at probe-relative coordinates `(210, 150)`
  before `SendInput`, which shifted DOM focus off the keyboard-smoke
  input. Removing the mouse-click sequence fixed it. Pure CompositionController
  exposes one visible child HWND (`Chrome_WidgetWin_0`), host-thread-owned;
  there is no hidden focus-target descendant. `demo-win --composition-focus-hwnd-test` narrows
  the hidden-child-HWND hypothesis: three CompositionController panes can be
  parented to hidden or 1x1 child HWNDs, captured/imported independently, and
  focused at the Win32 level (`GetFocus` returns the child HWND), but DOM
  keyboard input still does not arrive through physical `SendInput` or raw
  posted keyboard messages. Window-to-Visual changes the input result: the
  bounded `demo-win --window-to-visual-test` proof receives ASCII DOM input and
  imports the captured parent HWND into wgpu. It does not satisfy the no-overlay
  target as tested: after navigation the single-page smoke reports one visible
  WebView child HWND, and the three-page smoke reports three. CJK IME, dead
  keys, selection/candidate placement, occlusion/minimize behavior, and
  hidden/non-presented source-window capture remain unproven.
- DOM input compensation: `demo-win --cdp-input-test` keeps the pure
  CompositionController/no-overlay path and tries increasingly indirect input
  bridges. CDP is useful here as a page automation/probing channel; the smoke
  now reports `Input.dispatchKeyEvent`, `Input.insertText`,
  `Input.imeSetComposition`, `Runtime.evaluate`, and WebView2 `ExecuteScript`
  independently. The 2026-05-13 runtime result reports DOM input observed for
  all five routes. Treat that as a constrained synthetic input capability, not
  as a replacement for native OS IME ownership. It does not solve OS
  IME/candidate UI, virtual keyboard ownership, dead-key translation,
  app-level shortcut policy, or native popup widgets such as `<select>` menus.
- Host-owned IME bridge: `demo-win --ime-bridge-test` proves the first
  CompositionController-compatible native IME shape. The WebView2 producer
  observes the focused editable and caret rect in CSS pixels, emits
  `NavigationEvent::TextInputFocused`, the winit host maps that rect through
  the current composition offset and host scale factor into
  `Window::set_ime_cursor_area`, enables native IME on the host window, and
  forwards composition/commit through `Input.imeSetComposition` /
  `Input.insertText`. The smoke also covers password-like input suppression,
  textarea multiline/scroll caret geometry, composition cancellation on
  blur/navigation boundaries, and a purpose-derivation matrix that focuses
  `type=search/email/url/tel/number`, `inputmode="numeric"`, and
  `inputmode="none"` inputs and asserts `TextInputState::purpose()` returns
  the expected `InputPurpose`. The 2026-05-12 runtime result reports the
  matrix passes end-to-end: search→Search, email→Email, url→Url, tel→Tel,
  number→Decimal, numeric-inputmode→Numeric, none-inputmode→Disabled.
  Demo-win treats `InputPurpose::Disabled` (`inputmode="none"`) as "do not
  enable native IME." Higher-resolution hints (number vs decimal vs email vs
  url) only matter for hosts that drive a TSF text store or own a soft
  keyboard: winit 0.30's `Window::set_ime_purpose` is documented as
  unsupported on Windows, so demo-win logs the derived purpose but does not
  push it down to winit. It is not full TSF
  `ITextStoreACP2`: Windows input scopes, reconversion, surrounding-text
  queries, rich `contenteditable`, vendor IME quirks, and every zoom/layout
  edge remain future hardening.
- WPF CompositionControl comparison: Microsoft ships a WPF
  `WebView2CompositionControl` that uses visual hosting plus
  `GraphicsCaptureSession`, but its `IKeyboardInputSink` implementation is not
  a portable general keyboard path. The public WPF docs say
  `KeyboardInputSite` exists only for `TabInto` traversal,
  `TranslateAccelerator` and `TranslateChar` are not implemented by WebView2,
  and `OnKeyDown` should not be called by normal WPF keyboard routing because
  focus is on the controller HWND. WPF re-raises key events when WebView2
  reports `AcceleratorKeyPressed`; that is useful for host accelerator/chrome
  policy, not for browser text/IME delivery. Scrying now implements that bridge
  and `demo-win --accelerator-test` proves F3 is observed in the pure
  CompositionController path. The portable hybrid for scrying is therefore:
  keep CompositionController/WGC for pixels, keep WebView2 mouse / pointer /
  drag APIs for spatial input, use CDP `Input.*` for synthetic DOM
  keyboard/text/composition, and use `AcceleratorKeyPressed` for host
  shortcuts.
- API/binding sweep: WebView2 0.39.1 exposes composition-controller extensions
  for cursor, accessibility provider, OLE drag/drop, non-client hit testing, and
  drag-starting. No public composition-controller keyboard/text/IME sender is
  present. The nearby `AllowHostInputProcessing` controller option lets input
  pass through a browser window to the host; it is not an input injection route
  for visual hosting.
- Drag-and-drop: WebView2 OLE forwarding helpers are wired. The
  `WebSurfaceProducer::send_drag_input` trait method now returns a
  Windows-specific unsupported message because real WebView2 drops require the
  host's OLE `IDataObject`; use the concrete `drag_enter` / `drag_over` /
  `drag_leave` / `drop_data` methods until a portable data-carrier abstraction
  exists.
- Profile: persistent cookie recreation and InPrivate isolation are proven.
  Simultaneous multi-view behavior is proven for separate HWNDs by
  `demo-win --multi-view-test`; one HWND still cannot own two composition roots
  in the current setup.

### Missing Windows Slices

- Tab-state serialize / restore: WebView2 exposes no opaque interaction-state
  blob equivalent to WKWebView. Windows now has same-named methods for
  cross-platform call sites; serialization returns `None`, restore returns
  `Unsupported`, and hosts should restore URL/history/form state explicitly.
- Portable download resume data: WebView2 exposes live operation
  `pause_download(id)` / `resume_download(id)` / `can_resume_download(id)`, but
  not a macOS-style offline resume-data blob. Windows cancellations therefore
  report `resume_data: None`; hosts that want resume must pause/resume before
  cancelling the operation.
- Browser conveniences: find-in-page, print-to-PDF / PDF request, interactive
  print, autofill/password toggles, and the content-blocking/spellcheck
  ceilings are wired or documented. Find/PDF are covered by `--find-test` /
  `--pdf-test`; autofill toggles and the hard-throttle ceiling are covered by
  `--browser-test`.
- Observability events: external drop detected, context-menu requested, and
  WebRTC capture lifecycle events are wired, and native
  `Set-Cookie` observation is covered through WebView2
  `WebResourceResponseReceived`.
- Capture polish: producer capture metrics, resize stale-frame/dim-match
  guard, fixed Windows color-target reporting, demo host scale-change resize
  routing, and the documented WebView2 hard-throttling ceiling are wired.

## Implementation Lane

### W0 - Keep the Runtime Lane Bounded

Every GUI smoke must be bounded by an external timeout and process-tree kill.
Do not run raw `cargo run -p demo-win -- ...` during validation. The Win32
message pumps used inside test helpers must bound inner `PeekMessageW` drains
so a busy queue cannot defeat the timeout. ✅ The WebView2 producer's internal
message pump now caps each drain slice, and `demo-win --capture-test` has a
hard external process timeout in the validation lane.

### W1 - Input and Visibility Proof

Goal: close the remaining input confidence gap without changing the public
surface unnecessarily.

- ✅ Add a bounded `demo-win --keyboard-test` probe for the pure
  CompositionController path. Current result (2026-05-13): PASSes 3/3 — Win32
  `SendInput` after host-side `SetForegroundWindow` reaches the focused DOM
  element. The earlier long-standing timeout was a smoke bug
  (`producer.send_mouse_input` shifting DOM focus before SendInput); removing
  it fixed the path.
- ✅ Add bounded `demo-win --window-to-visual-test` and
  `--window-to-visual-multi-test` diagnostics. Current result: ASCII keyboard,
  three simultaneous animated pages, and parent-HWND WGC -> wgpu import work;
  the path is rejected for now because the live WebViews report visible
  `Chrome_WidgetWin_0` child HWNDs.
- ✅ Add bounded `demo-win --composition-focus-hwnd-test` diagnostic. Current
  result: three hidden and 1x1 visible child-HWND CompositionController panes
  capture/import independently, and `SetFocus` lands on the child HWND, but DOM
  keyboard input still times out for physical `SendInput` and raw posted
  keyboard messages.
- ✅ Add bounded `demo-win --cdp-input-test` diagnostic. Current result: direct
  CDP input is a viable no-overlay DOM input route: `dispatchKeyEvent`,
  `insertText`, `imeSetComposition`, `Runtime.evaluate`, and `ExecuteScript`
  all produced DOM input in the 2026-05-13 smoke. This still leaves native OS
  IME/candidate UI ownership with the host rather than WebView2.
- ✅ Add an `AcceleratorKeyPressed` bridge so CDP/CompositionController keyboard
  input can feed browser content while WebView2-originated accelerators still
  surface to the host chrome. The WPF `WebView2CompositionControl` pattern is
  the reference here: it forwards accelerator events back into WPF key events
  rather than using WPF as the input source for WebView text. Current
  2026-05-13 smoke result: `demo-win --accelerator-test` observes F3 with
  `IsBrowserAcceleratorKeyEnabled=true`.
- Dead keys, CJK IME composition, selection/candidate placement, and
  hidden/non-presented source-window capture are lower priority unless a
  no-visible-child Window-to-Visual configuration is found. The
  `--ime-bridge-test` now covers the bridge mechanics plus the purpose
  derivation matrix, but a real-IME pass under Microsoft Pinyin / Japanese
  IME / Korean IME is still pending. That pass is the gate before opening a
  TSF `ITextStoreACP2` lane: TSF only becomes worth doing if the real-IME
  pass exposes a concrete winit-layer ceiling (bad candidate placement,
  missing composition updates, no reliable cancellation, or an input-scope
  surface winit cannot express).
- ✅ Add a visibility smoke that listens for page-side `visibilitychange` and
  confirms `document.hidden`.
- ✅ Keep drag-in Windows-inherent for now: WebView2 needs an OLE `IDataObject`,
  and the portable trait method reports that concrete requirement.

Done condition: a Windows host can confidently route mouse, wheel, touch/pen,
focus, cursor, visibility, OLE drag-in, CDP-backed keyboard/text, and baseline
host-owned IME candidate placement through scrying, while WebView2 accelerators
surface to host chrome and the API clearly distinguishes that baseline bridge
from full TSF/browser-grade IME ownership.

#### W1.1 - Real-IME Validation Procedure

`--ime-bridge-test` proves the bridge plumbing (caret rect, CDP composition,
purpose matrix). The remaining gap is whether real vendor IMEs work through
the same loop:

```text
host HWND (winit, set_ime_allowed=true, set_ime_cursor_area=caret)
  ── user types ──► Windows IME ── WM_IME_COMPOSITION ──► winit
  ── WindowEvent::Ime(Preedit | Commit) ──► forward_ime_to_composition
  ── producer.set_ime_composition / insert_text ──► CDP ──► DOM
```

Setup:

1. Install at least one CJK IME: Microsoft Pinyin (zh-CN), Microsoft IME for
   Japanese (ja-JP), or Microsoft IME for Korean (ko-KR).
2. `cargo run -p demo-win` (no flags) navigates the probe page and stays
   interactive. The probe HTML focuses `#keyboard-smoke` on load.
3. With the probe window foregrounded and the input focused, switch to the
   target IME via Win+Space (or the language bar).

Each Ime event is logged through the `[ime]` channel in `forward_ime_to_composition`:

- `[ime] enabled` — winit reported the host window's IME context became active.
- `[ime] preedit "<text>" cursor=(<start>..<end> utf16)` — composition in progress.
- `[ime] commit "<text>"` — composition committed, `insert_text` dispatched.
- `[ime] dropped (no text-input focus): <event>` — load-bearing diagnostic: a
  composition arrived but `TextInputState` is not active, so we silently
  dropped it. Indicates either focus race or a probe-page focus bug.

Pass criteria:

1. **Pinyin pass**: type `ni3hao3` (the apostrophe / tone digits are optional
   depending on Pinyin's mode). `[ime] preedit` events fire as you type; on
   selecting a candidate the `[ime] commit` event reports `你好` (or similar),
   and the same characters appear in `#keyboard-smoke`.
2. **Japanese pass**: switch to hiragana, type `nihongo`, convert with space.
   `[ime] preedit` shows the romaji-to-hiragana progression; `[ime] commit`
   reports the chosen kanji and the page input value matches.
3. **Korean pass**: type `dkssudgktpdy` (Korean IME 2-Beolsik). Each syllable
   composes through `[ime] preedit` and commits character-by-character as the
   syllable closes; expected commit produces `안녕하세요`.
4. **Candidate placement**: in all three, the IME candidate window appears
   visually near the caret inside the WebView's input (not at the top-left of
   the host window or the screen). That validates the `set_ime_cursor_area`
   mapping through the producer offset + DPI scale.
5. **Cancellation**: while composing, click outside the input. The bridge
   should emit `TextInputBlurred`, demo-win calls `set_ime_allowed(false)`,
   and the IME composition disappears without committing.

A run is **PASS** if all five hold for at least one CJK IME.

A run is **FAIL with a winit ceiling** (= justifies opening a TSF lane) if any
of the following are observed:

- Candidate window appears at the wrong location (top-left of host or screen)
  and `set_ime_cursor_area` doesn't move it.
- `[ime] preedit` events stop firing partway through composition.
- Commit text differs from what the IME visibly committed (encoding /
  surrogate-pair bug).
- `[ime] dropped` fires for compositions that should reach DOM (focus race
  inherent to the host-window IME model).
- Input scope is required to make the IME behave correctly (e.g. Pinyin
  refuses to enter Latin numerals into a `type="tel"` field without
  app-side input-scope hinting) and winit cannot express it.

A run is **FAIL without a winit ceiling** (= bug to fix in scrying/demo-win,
not a TSF justification) if `[ime] commit` fires with correct text but the
`#keyboard-smoke` value doesn't update — that's the CDP `Input.insertText`
path failing.

Status: 2026-05-12, PASS for Pinyin + emoji panel — interactive run committed
`你好°F\(@^0^@)/` into `#keyboard-smoke` via Microsoft Pinyin / the Windows
emoji panel. The commit reached the live WebView visual and round-tripped
through the WGC-imported texture (capture-latency window of a few frames
between live and imported, as expected). Japanese and Korean IMEs are
nice-to-have additional vendor coverage but the single CJK pass already
clears the load-bearing gates: composition events fire on the host HWND,
candidate placement under `set_ime_cursor_area` was correct enough to pick
candidates, commits encode UTF-16 surrogates correctly through
`producer.insert_text`, and `[ime] dropped (no text-input focus)` did not
fire during the run. **TSF lane is not currently justified.**

### W2 - Browser Shell Ownership

Goal: give a tabbed shell ownership of browser chrome.

- Wire new-window/popup interception.
- Wire process-failure events and recovery smoke. ✅ `--process-test`
- Wire custom scheme / virtual host content routing. ✅ `--routing-test`
- ✅ Decide and document tab-state restore semantics. Windows exposes the same
  method names as macOS, but `serialize_interaction_state()` returns `None` and
  `restore_interaction_state(...)` returns `Unsupported`; browser shells should
  persist URL/history/form state explicitly when they need Windows tab restore.
- Add `demo-win` modes that prove popup routing, crash recovery if practical,
  and app-owned content loading.

Done condition: a host can own tabs, app content, history recovery, and crash
UI without reaching around `WebView2CompositionProducer`.

### W3 - Trust, Downloads, and Permissions

Goal: make Windows viable for real browsing and authenticated/document flows.

- Wire page auth events into the shared auth types. ✅ `--auth-test`
- Document download-channel auth source limits in WebView2. ✅
- Wire permission requests for camera/microphone/device-like prompts. ✅ `--permission-test`
- Wire downloads with id correlation, destination decisions, progress,
  cancellation, and follow-up resume policy. ✅ WebView2 live pause/resume is
  wired; offline resume-data blobs are a documented Windows ceiling. Covered by
  `--download-test` for the deterministic transfer path.
- Add runtime smokes with local deterministic test pages/servers where needed. ✅

Done condition: a browser shell can prompt for credentials and permissions,
route downloads, cancel downloads, and report transfer progress entirely
through scrying.

### W4 - Browser Conveniences

Goal: match the chrome-visible macOS conveniences where WebView2 exposes public
APIs.

- ✅ Find-in-page with result polling. Covered by `--find-test`.
- ✅ PDF generation / print-to-PDF plus interactive print. `request_pdf` uses
  `PrintToPdfStream`; `print()` invokes WebView2's print UI. Covered by
  `--pdf-test` for the non-interactive PDF path.
- ✅ Context-menu requested event and optional default-menu suppression. Native
  WebView2 context-menu events are registered; the deterministic smoke uses
  the document-start bridge and is covered by `--context-test`.
- ✅ External drop detected event. The document-start bridge mirrors the macOS
  `DataTransfer` heuristic and is covered by `--drop-test`; real page delivery
  still goes through the concrete OLE `IDataObject` drag/drop helpers.
- ✅ WebRTC capture lifecycle observability. Covered by `--media-test` for the
  bridge/event path.
- ✅ Document content blocking: WebView2 exposes request events/filters and the
  Windows producer uses them for virtual-host app routing, but there is no
  public `WKContentRuleList`-style compiled rule-list engine in this path.
- ✅ Document spellcheck/autocorrect: the bound WebView2 API exposes no
  producer-level spellcheck/autocorrect setting; page-authored `spellcheck`
  attributes remain the portable path.
- ✅ Wire autofill/password controls: Windows exposes
  `set_password_autosave_enabled` and `set_general_autofill_enabled` through
  WebView2 `ICoreWebView2Settings4`, and `--browser-test` toggles both.

Done condition: Windows matches the browser-class checklist rows that app
chrome surfaces directly.

### W5 - Capture Robustness and Display Quality

Goal: keep the post-composition texture stream trustworthy under real windowing
conditions.

- ✅ Add capture metrics to the Windows producer: `capture_metrics()` reports
  WGC frames received, emitted frames consumed by the host, and stale
  dimension-mismatch frames dropped during resize/restart churn.
- ✅ Add stale-frame / dim-match guards around resize and capture restart.
- ✅ Add DPI monitor-move handling and a runtime smoke that moves or simulates
  scale changes where feasible. `demo-win` routes `ScaleFactorChanged` through
  the same renderer resize path as physical resize events, and `--scale-test`
  simulates scale-induced physical capture-size changes by resizing the
  WebView2 producer and acquiring/importing fresh WGC frames at each size.
- ✅ Decide the Windows color target: the WebView2/WGC path currently reports
  `ColorPipeline::Srgb` and `Bgra8Unorm`. Public WebView2/WGC APIs in the
  bound version do not expose a Display P3 or HDR pixel-format/color-space
  control for this composition capture path, so P3/HDR stay unsupported on
  Windows until Microsoft exposes that surface.
- ✅ Document the hard-throttling limit: WebView2 exposes `SetIsVisible` for
  Page Visibility but no public inactive-scheduling / hard-pause equivalent to
  macOS `WKPreferences.inactiveSchedulingPolicy`. Windows `apply_settings`
  returns `Unsupported` when `inactive_scheduling_policy` is set, and
  `demo-win --browser-test` covers that ceiling.
- Keep explicit D3D12 fence sync as the preferred path and retain the fallback
  invalidation escape hatch.

Done condition: a host can diagnose capture cadence, resize and monitor moves
do not produce stale/badly-scaled frames, and color expectations are explicit.

## Non-Goals Unless Microsoft Ships New APIs

- Pre-composition WebView2 texture access.
- Sub-frame / iframe texture extraction.
- Capture while the visual is not part of the composition tree.
- Lower-than-compositor-frame latency.
- Native cookie-change observation for response-header `Set-Cookie` when the
  bound WebView2 version exposes no such event.
- Window-to-Visual hosting as the no-overlay rendering target. The original
  motivation was that `SendInput` reaches DOM in Window-to-Visual but not in
  pure CompositionController, so it was framed as a possible keyboard
  rehabilitation path. The 2026-05-12 / 2026-05-13 findings (real hardware
  keyboard AND programmatic `SendInput` work in pure CompositionController for
  the single-WebView-per-HWND case) removed that motivation for scrying's
  scoped envelope. The `--window-to-visual-test` smoke remains a useful
  diagnostic of WebView2's visible-child-HWND behavior, but Window-to-Visual
  is **closed** as a production target: the visible `Chrome_WidgetWin_0`
  children are incompatible with the no-overlay contract, and no use case
  in scrying's multi-WebView scope (see "Multi-WebView Scope") uniquely
  requires this hosting mode. Multi-pane needs that fall outside scrying's
  envelope are expected to be served by serval, not by re-opening
  Window-to-Visual.

## Audit Notes

- `webview2-com = 0.39.1` is the current binding version. Several target rows
  may become simpler after auditing newer WebView2 interfaces, but the target
  should not assume APIs that the crate does not expose.
- The shared `NavigationEvent`, `Download*`, `Auth*`, `Permission*`, and
  `Cookie` public types are already broad enough for most Windows parity work;
  the likely code work is producer wiring, not wholesale API invention.
- `demo-win` is the right home for Windows runtime proof. The catchall
  `demo-scrying-winit` should remain a backend-selection and dependency-gating
  smoke.

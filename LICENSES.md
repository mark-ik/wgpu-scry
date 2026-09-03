# Licenses in this repository

**This repository: MPL-2.0.** Every file Mark wrote carries Exhibit A and the
SPDX tag `MPL-2.0`, per the
[license posture brief](../mere/design_docs/2026-08-22_license_posture_brief.md)
of 2026-08-22 (mere `design_docs/2026-08-22_license_posture_brief.md`). The
full text is in [`LICENSE`](LICENSE), and [`NOTICE`](NOTICE) carries the one
upstream attribution this workspace owes.

This file is the provenance ledger. It is the authority for what the relicense
tool (mere `scripts/relicense_headers.py`) skips: the backtick-quoted paths in
the **Retained licenses** table are never touched. Provenance comes before
license — a file gets Exhibit A only if Mark wrote it.

## What this repository is made of

**None of it is retained: 0 of 158 tracked files, 0 of 108 tracked source
files.** wgpu-scry vendors nothing. Every crate — `scrying` and the six
`demo-*` workspace members — is Mark's, and every manifest already says
`MPL-2.0` or inherits it from the workspace. The provenance grep for
`Copyright`, `Licensed under`, `Permission is hereby granted`, `Apache License`
and `SPDX-License-Identifier` finds nothing in any source file; its only hits
are `LICENSE` and `NOTICE`, which are quoting license text and recording the
Slint attribution below.

## Retained licenses

**None.** There is no third-party code in this repository. The relicense tool
therefore skips nothing, and every tracked source file is owned.

| Path | License | Upstream | Notice files |
|---|---|---|---|

## Derivatives carrying MPL-2.0 with an upstream notice retained

These are **not** skipped. Each is Mark's substantial work over an upstream
starting point, relicensed MPL-2.0 with the upstream notice kept verbatim.
Applied with `--retain-notice`.

| Path | Upstream | Notices kept |
|---|---|---|
| `scrying/src/native_frame` | the [Slint Servo embedding example](https://github.com/slint-ui/slint/tree/master/examples/servo)'s per-platform `rendering_context/` shape, MIT | `Copyright (c) SixtyFPS GmbH <info@slint.dev>` in the root [`NOTICE`](NOTICE), which is the notice file for the module; no per-file upstream copyright line exists to preserve |

The derivation is structural — the module takes platform-native texture handles
(D3D12 NT handle, IOSurface, DMA-BUF) rather than the example's Servo-emitted GL
framebuffer surfaces — and MIT permits the relicensing so long as the notice
travels with it, which `NOTICE` does.

**This section is deliberately not the skip list.** The tool reads only the
`## Retained licenses` table above. Adding a path here documents a disposition;
it does not exempt the path from receiving a header.

## Exceptions under the fork/vendor criterion

**None.** The brief's §4 test — a crate stays MIT OR Apache-2.0 only when a
third party would need to *modify or vendor* it rather than merely link it —
admits nothing in this repository. `scrying` is MPL-2.0 already, so no
published grant changes; per the sweep plan's invariant 8 no crate is
republished for the license.

## How to add a file from elsewhere

1. Do not delete or rewrite the upstream copyright or license notice, ever.
2. Add its path to **Retained licenses** above with its license, upstream URL,
   and where its notice text lives. The tool then skips it automatically.
3. If it is a substantial derivative rather than a verbatim import, the brief's
   rule is MPL-2.0 on the derivative *with the upstream notice retained* —
   record it in that section so the distinction is not lost.
4. Never add `license-file` to an owned manifest.
5. Re-run `python ../mere/scripts/relicense_headers.py --repo . --audit` and
   confirm the owned source count moved by exactly what you expected.

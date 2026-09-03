// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

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

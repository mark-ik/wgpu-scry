# Wgpu-scry documentation

This is the canonical index for active documentation in this repository.

## Working principles

- Keep compile coverage, imported-resource shape, pixel correctness, and
  headed hardware receipts as separate claims.
- Scrying owns system-webview adaptation and frame production. The host owns
  windowing, embedding, navigation policy, storage policy, and fallback choice.
- Keep the feature-selected wgpu row identical to the host row and re-export
  the selected types at the public boundary.
- Publish and tag the exact source being described. Once a crate version is on
  crates.io, later public API work receives a new version.
- Treat `README.md`, `scrying/README.md`, and `docs/parity-matrix.md` as current
  public guidance and reconcile them when a platform receipt changes.

## Active documents

- [Documentation policy](DOC_POLICY.md): shared documentation rules and the Wgpu-scry local addendum.
- [Platform ceilings and parity roadmap](2026-05-07_platform_ceilings.md): platform API ceilings and the long-form backend implementation record.
- [Browser-class parity checklist](2026-05-09_browser_parity_checklist.md): cross-platform browser capability and verification matrix.
- [WKWebView SPI evaluation](2026-05-09_spi_evaluation.md): private-API research and public-API stop lines for macOS.
- [Windows WebView2 integration target](2026-05-11_windows_webview2_target.md): Windows composition, capture, and input target shape.
- [Windows producer decomposition plan](2026-05-12_windows_decomposition_plan.md): separation of reusable WebView2 production from the demo host.
- [Linux WebKitGTK phase 2a](2026-05-14_linux_webkitgtk_phase_2a.md): WebKitGTK 4.1 baseline and the three-backend Linux strategy.
- [Phase 4 strategy](2026-05-15_phase4_strategy.md): Vulkan DMABUF import and WPE sequencing.
- [WPE bindings decision](2026-05-20_phase4b_wpe_bindings_decision.md): ownership and generation strategy for WPE Rust bindings.
- [WPE platform producer](2026-05-20_phase4c_wpe_platform_producer.md): headless WPEPlatform producer and DMABUF callback seam.
- [WPE producer implementation plan](2026-05-20_phase4c2_implementation_plan.md): Phase 4c.2 implementation phases and gates.
- [WPE producer retrospective](2026-06-03_phase4c2_retrospective.md): findings from the completed Phase 4c.2 implementation.
- [Navigation and resize plan](2026-06-03_phase4c3_implementation_plan.md): Phase 4c.3 implementation gates.
- [Navigation and resize record](2026-06-03_phase4c3_navigation_resize.md): landed WPE navigation and resize behavior.
- [Multi-plane DCC import](2026-06-04_phase4a_x_multiplane_dcc_import.md): DRM-modifier and shared-fd import findings.
- [Multi-plane DCC plan](2026-06-04_phase4a_x_multiplane_dcc_plan.md): implementation strategy for multi-plane compressed imports.
- [Interactive input plan](2026-06-04_phase4c4_implementation_plan.md): Phase 4c.4 input MVP gates.
- [Interactive input record](2026-06-04_phase4c4_input_mvp.md): landed WPE mouse, keyboard, and scroll path.
- [WPE to Vulkan round-trip plan](2026-06-04_wpe_to_vulkan_roundtrip_plan.md): end-to-end import-test strategy.
- [WPE to Vulkan round-trip record](2026-06-04_wpe_to_vulkan_roundtrip.md): integration-test implementation and hardware findings.
- [WPE script-message bridge](2026-06-05_phase4c5a_script_message.md): script-message implementation and verification.
- [WPE cookies](2026-06-05_phase4c5b_cookies.md): cookie API implementation and verification.
- [Improvement backlog](2026-06-28_improvement_backlog.md): prioritized actionable gaps and upstream-blocked work.

## Maintainer-owned description

[PROJECT_DESCRIPTION.md](PROJECT_DESCRIPTION.md) is reserved for the
maintainer and has not yet been created. The root [README](../README.md)
remains the public project description until the maintainer supplies it.

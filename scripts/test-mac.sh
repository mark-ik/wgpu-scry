#!/usr/bin/env bash
# Run every demo-mac integration-test mode in sequence, propagate
# their per-mode exit codes (each test mode now exits 1 on FAIL),
# and surface an aggregate PASS/FAIL summary plus a nonzero exit
# code if any mode fails. Designed for CI and for "did the refactor
# break anything?" smoke runs.
#
# Each mode is wrapped in a wall-clock alarm via perl(1) — internal
# step deadlines (typically 5s per step) should keep the process
# under the alarm even on a slow runner, but the alarm guarantees
# no test mode can hang the whole suite.
#
# Each \`--*-test\` mode runs headless by default (no visible window,
# `NSApplicationActivationPolicyProhibited` so the developer's
# frontmost app keeps focus and no Dock icon flashes), so this
# script is safe to run while you're working on something else.
# To watch a failing test in real time, run the failing mode
# manually with \`--visible\`:
#     cargo run -p demo-mac -- --interaction-state-test --visible
#
# Requires:
#   - cargo on PATH
#   - a logged-in macOS user session (the AppKit run loop still
#     needs a WindowServer connection even for hidden windows)
#
# ScreenCaptureKit is intentionally a separate hardware suite because macOS
# must grant Screen Recording permission to the runner ahead of time:
#     CAPTURE=1 bash scripts/test-mac.sh

set -uo pipefail

cd "$(dirname "$0")/.."

# Per-mode wall-clock cap. The internal step deadlines fail any
# stuck step in 5s, so 60s is plenty for a healthy run.
TIMEOUT="${TIMEOUT:-60}"

MODES=(
    --scripted
    --browser-test
    --interaction-state-test
    --pointer-input-test
    --incognito-test
    --download-test
    --profile-test
    --two-tabs
)

if [[ "${CAPTURE:-0}" = "1" ]]; then
    MODES+=(--capture-test --capture-test+resize)
fi

# Build once so each `cargo run` invocation skips compile overhead.
echo "==> building demo-mac"
cargo build --locked -q -p demo-mac

passed=0
failed=0
failed_modes=()

for mode in "${MODES[@]}"; do
    echo
    echo "==> $mode"
    args=("$mode")
    if [[ "$mode" = "--capture-test+resize" ]]; then
        args=(--capture-test --resize-test)
    fi
    if perl -e 'alarm shift; exec @ARGV' "$TIMEOUT" \
        cargo run --locked -q -p demo-mac -- "${args[@]}"; then
        echo "  -> PASS"
        passed=$((passed + 1))
    else
        rc=$?
        if [[ $rc -eq 142 ]]; then
            echo "  -> FAIL (timed out after ${TIMEOUT}s)"
        else
            echo "  -> FAIL (exit $rc)"
        fi
        failed=$((failed + 1))
        failed_modes+=("$mode")
    fi
done

echo
echo "==> summary"
echo "  passed: $passed / ${#MODES[@]}"
echo "  failed: $failed"
if [[ $failed -gt 0 ]]; then
    for m in "${failed_modes[@]}"; do
        echo "    - $m"
    done
    exit 1
fi
echo "  all PASS"

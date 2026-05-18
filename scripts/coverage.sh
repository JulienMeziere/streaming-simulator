#!/usr/bin/env bash
# Run the unit-test suite under llvm-cov and emit an HTML coverage
# report. Targeted at local iteration during the test refactor + 100%-
# coverage push. Pass any extra `cargo llvm-cov` flags as positional
# arguments (e.g. `--summary-only`, `--fail-under-lines 95`).
#
# Prerequisites:
#   cargo install cargo-llvm-cov
#
# Usage:
#   ./scripts/coverage.sh                       # HTML report
#   ./scripts/coverage.sh --summary-only        # numeric summary only
#   ./scripts/coverage.sh --fail-under-lines 95 # gate suitable for CI
#
# Notes:
#   * `--features fdk-aac` mirrors what `cargo test` uses in CI; without
#     it the AAC-LC / HE-AAC / AAC-BT processors aren't compiled and
#     their coverage shows as 0%.
#   * `--lib` skips the standalone-binary entry in src/main.rs (which
#     just calls `nih_export_standalone()` — nothing testable).

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Default to HTML output; let callers override by passing `--summary-only`
# or other flags.
if [[ "$#" -eq 0 ]]; then
    set -- --html
fi

cargo llvm-cov --features fdk-aac --lib "$@"

if [[ " $* " == *" --html "* ]]; then
    echo
    echo "report: target/llvm-cov/html/index.html"
fi

#!/usr/bin/env bash
# Line coverage for the unit tests plus the end-to-end smoke test.
#
#   scripts/coverage.sh                    # summary only
#   LCOV_PATH=lcov.info scripts/coverage.sh # also write an lcov file (CI uploads it)
#   HTML_DIR=target/cov-html ...           # also write an HTML report
#   MIN_LINES=90 ...                       # fail when line coverage drops below 90%
#
# python3 is required: the end-to-end pass runs the instrumented binary against
# the fake compositor, which is what covers main.rs and the daemon's real socket.
set -euo pipefail
cd "$(dirname "$0")/.."

# Some dev profiles pick the cranelift backend, which cannot instrument code.
export CARGO_PROFILE_DEV_CODEGEN_BACKEND="${CARGO_PROFILE_DEV_CODEGEN_BACKEND:-llvm}"
export CARGO_PROFILE_TEST_CODEGEN_BACKEND="${CARGO_PROFILE_TEST_CODEGEN_BACKEND:-llvm}"

eval "$(cargo llvm-cov show-env --sh)"
cargo llvm-cov clean --workspace

cargo test --all-features >/dev/null
cargo build --all-features >/dev/null
NSTICKY_BIN="${CARGO_LLVM_COV_TARGET_DIR:-target}/debug/nsticky" \
  scripts/e2e-smoke.sh >/dev/null

cargo llvm-cov report --summary-only --fail-under-lines "${MIN_LINES:-0}"

if [[ -n "${LCOV_PATH:-}" ]]; then
  cargo llvm-cov report --lcov --output-path "$LCOV_PATH"
  echo "lcov written to $LCOV_PATH"
fi

if [[ -n "${HTML_DIR:-}" ]]; then
  cargo llvm-cov report --html --output-dir "$HTML_DIR"
  echo "html written to $HTML_DIR"
fi

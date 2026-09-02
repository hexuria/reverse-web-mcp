#!/usr/bin/env bash
# The gate every step must pass before it is committed.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace 2>&1 | tee /tmp/rwmcp-test.log | grep -E '^test result|FAILED|panicked' | sort | uniq -c
grep -q 'FAILED\|panicked' /tmp/rwmcp-test.log && { echo "GATE: tests failed"; exit 1; }
echo "GATE: green"

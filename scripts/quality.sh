#!/usr/bin/env bash
# Local quality gate (story card A3): fmt, clippy, test, doc.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

echo "== fmt =="
cargo fmt --all --check

echo "== clippy =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== test =="
cargo test --workspace

echo "== doc =="
cargo doc --workspace --no-deps

echo "== all quality gates passed =="

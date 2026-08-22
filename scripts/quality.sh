#!/usr/bin/env bash
# Local quality gate: fmt, clippy, test, doc.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

echo "== fmt =="
cargo fmt --all --check

echo "== clippy =="
cargo clippy --workspace --all-targets --features deadlock-detection -- -D warnings

echo "== build fixtures =="
# Build fixture cdylibs up front so the tests below never race nested
# `cargo build` calls against the same target dir. `-p` selection keeps the
# sdk dependency free of its abi-exports (a `--workspace` build would enable
# them and hit duplicate symbols when linking the cdylibs on Linux).
cargo build -p cordis-fixture-hello -p cordis-fixture-bad-version -p cordis-fixture-not-a-plugin -p cordis-fixture-spawn -p cordis-fixture-meta -p cordis-fixture-context

echo "== test =="
cargo test --workspace --features deadlock-detection

echo "== doc =="
cargo doc --workspace --no-deps --features deadlock-detection

echo "== all quality gates passed =="

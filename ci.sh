#!/usr/bin/env bash
# CI quality gate for steelbx (no podman required).
# Usage: ./ci.sh
set -euo pipefail
cd "$(dirname "$0")"

cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
cargo build --release
cargo test --release --lib --bins

echo "✓ All checks passed."

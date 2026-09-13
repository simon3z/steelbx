#!/usr/bin/env bash
# End-to-end integration tests (requires a working podman on this host).
# Run on a machine where podman is native — not nested in a container.
# Usage: ./e2e.sh
set -euo pipefail
cd "$(dirname "$0")"

if ! command -v podman &>/dev/null; then
    echo "✗ podman not found — install it and re-run (see README)."
    exit 1
fi

podman --version

# Single-threaded: the integration tests share one podman store,
# and parallel first use is flaky.
cargo test --release --test integration -- --test-threads=1

echo "✓ Integration tests passed."

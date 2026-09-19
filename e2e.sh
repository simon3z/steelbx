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

# Manual: `run` is interactive (needs a TTY and a local image), so it is
# not automated here. Try it by hand against a local steelbx image:
#   steelbx run -i <local-image> ~/some/path
# The box auto-removes on exit; the exit code is the shell's (130 on Ctrl-C).

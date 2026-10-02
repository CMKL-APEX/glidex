#!/usr/bin/env bash
# A throwaway control plane for the UI e2e suite: its own HOME (database,
# images, disks, firmware vars), listening on E2E_API_PORT. It still talks
# to the host's glidex-netd and Open vSwitch.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../../.." && pwd)
e2e_home=${GLIDEX_E2E_HOME:-/tmp/glidex-ui-e2e}
port=${E2E_API_PORT:-8851}

# Build before HOME changes: cargo and rustup live under the real HOME.
cargo build -q -p glidex-control-plane --bin glidex-control-plane --manifest-path "$repo/Cargo.toml"
binary="${CARGO_TARGET_DIR:-$repo/target}/debug/glidex-control-plane"

rm -rf "$e2e_home"
mkdir -p "$e2e_home/.glidex" "$e2e_home/images"
# The UI's default Cloud-Hypervisor firmware is ~/.glidex/CLOUDHV.fd.
if [ -e "$HOME/.glidex/CLOUDHV.fd" ]; then
  ln -s "$HOME/.glidex/CLOUDHV.fd" "$e2e_home/.glidex/CLOUDHV.fd"
fi

export HOME=$e2e_home
export GLIDEX_LISTEN=127.0.0.1:$port
exec "$binary"

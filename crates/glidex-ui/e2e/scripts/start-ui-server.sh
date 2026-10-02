#!/usr/bin/env bash
# glidex-ui serving the built UI for the e2e suite, in front of the
# scratch control plane over TCP (there's no ui.sock peer to be here).
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../../.." && pwd)
port=${E2E_UI_SERVER_PORT:-5175}
api_port=${E2E_API_PORT:-8851}

(cd "$repo/crates/glidex-ui/ui" && bun run build >/dev/null)
cargo build -q -p glidex-ui --manifest-path "$repo/Cargo.toml"
binary="${CARGO_TARGET_DIR:-$repo/target}/debug/glidex-ui"

export GLIDEX_UI_DIR="$repo/crates/glidex-ui/ui/dist"
export GLIDEX_UI_LISTEN=127.0.0.1:$port
export GLIDEX_API_URL=http://127.0.0.1:$api_port
unset GLIDEX_API_SOCKET GLIDEX_UI_HOSTS GLIDEX_UI_TLS_CERT GLIDEX_UI_TLS_KEY
exec "$binary"

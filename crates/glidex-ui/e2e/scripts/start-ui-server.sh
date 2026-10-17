#!/usr/bin/env bash
# glidex-ui serving the built UI for the e2e suite, in front of the
# scratch control plane's ui.sock (the test user is its ui_user).
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../../.." && pwd)
port=${E2E_UI_SERVER_PORT:-5175}

(cd "$repo/crates/glidex-ui/ui" && bun run build >/dev/null)
cargo build -q -p glidex-ui --manifest-path "$repo/Cargo.toml"
binary="${CARGO_TARGET_DIR:-$repo/target}/debug/glidex-ui"

export GLIDEX_UI_DIR="$repo/crates/glidex-ui/ui/dist"
export GLIDEX_UI_LISTEN=127.0.0.1:$port
# Plain HTTP on loopback (the only place it is allowed).
export GLIDEX_UI_TLS=off
export GLIDEX_API_SOCKET=${GLIDEX_E2E_HOME:-/tmp/glidex-ui-e2e}/ui.sock
unset GLIDEX_API_URL GLIDEX_UI_HOSTS GLIDEX_UI_TLS_CERT GLIDEX_UI_TLS_KEY
exec "$binary"

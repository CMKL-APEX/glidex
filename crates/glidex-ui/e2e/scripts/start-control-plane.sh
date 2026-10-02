#!/usr/bin/env bash
# A throwaway control plane for the UI e2e suite: its own HOME (database,
# images, disks, firmware vars), config, runtime directory and sockets,
# listening on E2E_API_PORT. It still talks to the host's glidex-netd and
# Open vSwitch.
#
# Authentication is on, as in production. The user running the tests is
# the scratch instance's break-glass admin on its api.sock (admin_group is
# their primary group); tests/auth.setup.ts turns that into a browser
# session.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../../.." && pwd)
e2e_home=${GLIDEX_E2E_HOME:-/tmp/glidex-ui-e2e}
port=${E2E_API_PORT:-8851}
ui_port=${E2E_UI_PORT:-5174}
ui_server_port=${E2E_UI_SERVER_PORT:-5175}

# Build before HOME changes: cargo and rustup live under the real HOME.
cargo build -q -p glidex-control-plane --bin glidex-control-plane --manifest-path "$repo/Cargo.toml"
binary="${CARGO_TARGET_DIR:-$repo/target}/debug/glidex-control-plane"

rm -rf "$e2e_home"
mkdir -p "$e2e_home/.glidex" "$e2e_home/images" "$e2e_home/run" "$e2e_home/policies"
# The UI's default Cloud-Hypervisor firmware is ~/.glidex/CLOUDHV.fd.
if [ -e "$HOME/.glidex/CLOUDHV.fd" ]; then
  ln -s "$HOME/.glidex/CLOUDHV.fd" "$e2e_home/.glidex/CLOUDHV.fd"
fi

group=$(id -gn)
cat >"$e2e_home/control-plane.json" <<EOF
{
  "listen": ["127.0.0.1:$port"],
  "api_socket": "$e2e_home/api.sock",
  "ui_socket": "$e2e_home/ui.sock",
  "admin_group": "$group",
  "users_group": "$group",
  "auth": {
    "allowed_origins": [
      "http://localhost:$ui_port", "http://127.0.0.1:$ui_port",
      "http://localhost:$ui_server_port", "http://127.0.0.1:$ui_server_port"
    ]
  },
  "authz": { "policy_files_dir": "$e2e_home/policies" }
}
EOF

export HOME=$e2e_home
export GLIDEX_CONFIG=$e2e_home/control-plane.json
export GLIDEX_RUN_DIR=$e2e_home/run
unset RUNTIME_DIRECTORY CREDENTIALS_DIRECTORY
export GLIDEX_LISTEN=127.0.0.1:$port
exec "$binary"

#!/usr/bin/env bash
# A throwaway control plane for the UI e2e suite: its own HOME (database,
# images, disks, firmware vars), config, runtime directory and sockets,
# listening on E2E_API_PORT. It still talks to the host's glidex-netd and
# Open vSwitch.
#
# Authentication is on, as in production. The user running the tests is
# the scratch instance's break-glass admin on its api.sock (admin_group is
# their primary group); tests/auth.setup.ts turns that into a browser
# session. They are its `ui_user` too: the dev server and glidex-ui reach
# it through its ui.sock, where the UI's gRPC-Web RPCs are taken
# (spec/grpc.md §6).
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../../.." && pwd)
e2e_home=${GLIDEX_E2E_HOME:-/tmp/glidex-ui-e2e}
port=${E2E_API_PORT:-8851}
ui_port=${E2E_UI_PORT:-5174}
ui_server_port=${E2E_UI_SERVER_PORT:-5175}

# One suite run at a time on this machine: runs share the host's
# glidex-netd and Open vSwitch, and by default the ports and $e2e_home,
# which is wiped below. The lock is taken before anything is touched and
# passes to the control plane (exec), so it is held until the suite stops
# it; a second run fails here instead of breaking the first.
lock=${GLIDEX_E2E_LOCK:-/tmp/glidex-ui-e2e.lock}
exec 9>>"$lock" || { echo "error: cannot open the e2e lock $lock" >&2; exit 1; }
if ! flock -n 9; then
  echo "error: another UI e2e run is going on this machine ($(cat "$lock" 2>/dev/null || echo "see $lock")); wait for it to finish" >&2
  exit 1
fi
printf 'pid %s, %s, from %s, since %s\n' "$$" "$(id -un)" "$repo" "$(date '+%F %T')" >"$lock"

# Build before HOME changes: cargo and rustup live under the real HOME.
# The shim too: VMs run under it (detached runner), found next to the binary.
cargo build -q -p glidex-control-plane --bin glidex-control-plane -p glidex-vm-shim --bin glidex-vm-shim --manifest-path "$repo/Cargo.toml"
binary="${CARGO_TARGET_DIR:-$repo/target}/debug/glidex-control-plane"

rm -rf "$e2e_home"
mkdir -p "$e2e_home/.glidex" "$e2e_home/images" "$e2e_home/run" "$e2e_home/policies"
# Pulling the cloudhv-edk2 firmware image copies ~/.glidex/CLOUDHV.fd (the
# installer's pinned build) instead of downloading it.
if [ -e "$HOME/.glidex/CLOUDHV.fd" ]; then
  ln -s "$HOME/.glidex/CLOUDHV.fd" "$e2e_home/.glidex/CLOUDHV.fd"
fi

# GLIDEX_E2E_GROUP: when run under another primary group (sg glidex, for
# glidex-netd), the group that makes you the scratch admin.
group=${GLIDEX_E2E_GROUP:-$(id -gn)}
cat >"$e2e_home/control-plane.json" <<EOF
{
  "listen": ["127.0.0.1:$port"],
  "tls": "off",
  "api_socket": "$e2e_home/api.sock",
  "ui_socket": "$e2e_home/ui.sock",
  "admin_group": "$group",
  "users_group": "$group",
  "ui_user": "$(id -un)",
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
# Registered datastores live under this, not /srv/glidex (spec/storage.md S11).
export GLIDEX_DATASTORE_ROOT=$e2e_home/datastores
mkdir -p "$GLIDEX_DATASTORE_ROOT"
unset RUNTIME_DIRECTORY CREDENTIALS_DIRECTORY
export GLIDEX_LISTEN=127.0.0.1:$port
exec "$binary"

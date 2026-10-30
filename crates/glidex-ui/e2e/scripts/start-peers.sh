#!/usr/bin/env bash
# Two more throwaway control planes for the two-organization spec
# (tests/two-organizations.spec.ts, spec/19-tenancy.md D63–D73): `north`, a
# cluster, and `acme`, another organization whose directory's tenant
# north admits. Each serves its API over HTTPS with a self-signed
# certificate (trust pins it) and has its own glidex-ui over HTTPS:
#
#   north: API https://127.0.0.1:$E2E_NORTH_API_PORT (8861), UI https://localhost:$E2E_NORTH_UI_PORT (5186)
#   acme:  API https://127.0.0.1:$E2E_ACME_API_PORT (8862),  UI https://127.0.0.1:$E2E_ACME_UI_PORT (5187)
#
# The two web UIs are on different host names so their session cookies
# (which aren't per port) stay apart, and each is published as that
# address (`cluster.ui_url`): glidex-ui would report this host's name,
# which the loopback-only UI doesn't answer to. Neither talks to glidex-netd: they
# run no VMs, and leave the host's networking to the main scratch
# control plane. The test user is the break-glass admin on each api.sock.
# Everything stops when this script is stopped.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../../../.." && pwd)
base=${GLIDEX_E2E_PEERS_HOME:-/tmp/glidex-ui-e2e-peers}
north_api=${E2E_NORTH_API_PORT:-8861}
north_ui=${E2E_NORTH_UI_PORT:-5186}
acme_api=${E2E_ACME_API_PORT:-8862}
acme_ui=${E2E_ACME_UI_PORT:-5187}

cargo build -q -p glidex-control-plane --bin glidex-control-plane -p glidex-ui --manifest-path "$repo/Cargo.toml"
target="${CARGO_TARGET_DIR:-$repo/target}/debug"

rm -rf "$base"
mkdir -p "$base"
# Its own build of the UI: the main suite's glidex-ui builds into ui/dist
# at the same time.
(cd "$repo/crates/glidex-ui/ui" && bunx vite build --outDir "$base/dist" --emptyOutDir >/dev/null)

pids=()
stop() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap stop EXIT INT TERM

group=${GLIDEX_E2E_GROUP:-$(id -gn)}

# peer <name> <api port> <ui host> <ui port>
peer() {
  local name=$1 api=$2 ui_host=$3 ui=$4
  local home="$base/$name"
  mkdir -p "$home/.glidex" "$home/run" "$home/policies" "$home/no-netd"
  cat >"$home/control-plane.json" <<EOF
{
  "listen": ["127.0.0.1:$api"],
  "tls": "auto",
  "api_socket": "$home/api.sock",
  "ui_socket": "$home/ui.sock",
  "admin_group": "$group",
  "users_group": "$group",
  "ui_user": "$(id -un)",
  "cluster": { "api_url": "https://127.0.0.1:$api", "ui_url": "https://$ui_host:$ui" },
  "auth": {
    "allowed_origins": ["https://$ui_host:$ui"],
    "issuer_base": "https://$ui_host:$ui"
  },
  "authz": { "policy_files_dir": "$home/policies" }
}
EOF
  (
    export HOME=$home
    export GLIDEX_CONFIG=$home/control-plane.json
    export GLIDEX_RUN_DIR=$home/run
    export GLIDEX_DATASTORE_ROOT=$home/datastores
    # No glidex-netd here: networking reads as unavailable.
    export GLIDEX_NETD_RUN_DIR=$home/no-netd
    unset RUNTIME_DIRECTORY CREDENTIALS_DIRECTORY GLIDEX_LISTEN
    exec "$target/glidex-control-plane" >"$home/control-plane.log" 2>&1
  ) &
  pids+=($!)
  (
    export HOME=$home
    export GLIDEX_UI_DIR=$base/dist
    export GLIDEX_UI_LISTEN=127.0.0.1:$ui
    export GLIDEX_UI_HOSTS=$ui_host:$ui
    export GLIDEX_UI_TLS=auto
    export GLIDEX_API_SOCKET=$home/ui.sock
    unset GLIDEX_API_URL GLIDEX_UI_TLS_CERT GLIDEX_UI_TLS_KEY
    exec "$target/glidex-ui" >"$home/ui.log" 2>&1
  ) &
  pids+=($!)
}

peer north "$north_api" localhost "$north_ui"
peer acme "$acme_api" 127.0.0.1 "$acme_ui"

# Playwright waits for north's web UI; stop with the first that exits.
wait -n
echo "a peer stopped; logs under $base/*/" >&2
exit 1

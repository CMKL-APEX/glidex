#!/bin/bash
# Dev/test only: run the web UI end-to-end suite (crates/glidex-ui/e2e)
# against a throwaway control plane, for Cloud Hypervisor and/or QEMU.
# Installs the suite's dependencies and Playwright's Chromium on first use.
#
#   scripts/dev/ui-e2e.sh                       # both hypervisors
#   scripts/dev/ui-e2e.sh -H qemu               # one: ch | cloudhypervisor | qemu | all
#   scripts/dev/ui-e2e.sh -i ~/images/ubuntu.img
#   scripts/dev/ui-e2e.sh -- --headed -g "create form"   # extra Playwright args
#
# The VM boot test needs a UEFI-bootable cloud image (-i, or
# GLIDEX_TEST_IMAGE), KVM, the hypervisor, and a usable glidex-netd with
# Open vSwitch; without them it is skipped and the rest still runs. See
# crates/glidex-ui/e2e/README.md.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
e2e="$repo/crates/glidex-ui/e2e"
ui="$repo/crates/glidex-ui/ui"
hypervisor=all
image=${GLIDEX_TEST_IMAGE:-}

usage() { sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    -H|--hypervisor) hypervisor=${2:?--hypervisor needs a value}; shift 2 ;;
    -i|--image) image=${2:?--image needs a path}; shift 2 ;;
    -h|--help) usage ;;
    --) shift; break ;;
    *) echo "unknown option: $1" >&2; usage 1 >&2 ;;
  esac
done

case "$hypervisor" in
  all) projects=(--project=ui --project=cloudhypervisor --project=qemu) ;;
  ch|cloudhypervisor) projects=(--project=ui --project=cloudhypervisor) ;;
  qemu) projects=(--project=ui --project=qemu) ;;
  *) echo "--hypervisor must be ch, cloudhypervisor, qemu or all" >&2; exit 1 ;;
esac

die() { echo "error: $*" >&2; exit 1; }
command -v bun >/dev/null || die "bun is not installed (cargo run -p glidex-install, or https://bun.sh)"
command -v cargo >/dev/null || die "cargo is not installed"

# The suite starts its own servers and never reuses running ones.
api_port=${E2E_API_PORT:-8851}
ui_port=${E2E_UI_PORT:-5174}
for port in "$api_port" "$ui_port"; do
  if ss -ltnH "sport = :$port" 2>/dev/null | grep -q .; then
    die "port $port is in use (a previous run still going?); set E2E_API_PORT / E2E_UI_PORT"
  fi
done

if [ -n "$image" ]; then
  image=${image/#\~/$HOME}
  [ -f "$image" ] || die "image $image does not exist"
  export GLIDEX_TEST_IMAGE=$image
else
  echo "note: no cloud image (-i / GLIDEX_TEST_IMAGE); the VM boot test will be skipped" >&2
fi

[ -d "$ui/node_modules" ] || (cd "$ui" && bun install --frozen-lockfile)
[ -d "$e2e/node_modules" ] || (cd "$e2e" && bun install --frozen-lockfile)
# A no-op once the matching browser is downloaded.
(cd "$e2e" && bunx playwright install chromium >/dev/null)

cd "$e2e"
exec bunx playwright test "${projects[@]}" "$@"

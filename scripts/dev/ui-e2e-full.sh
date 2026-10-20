#!/bin/bash
# Dev/test only: the whole web UI e2e suite, the tests that need the host's
# glidex-netd and a cloud image included. scripts/dev/ui-e2e.sh skips those
# when something is missing; here a skipped test fails the run.
#
#   scripts/dev/ui-e2e-full.sh -i ~/images/ubuntu.img          # both hypervisors
#   scripts/dev/ui-e2e-full.sh -i ~/images/ubuntu.img -H ch    # any ui-e2e.sh option
#
# The image is -i or GLIDEX_TEST_IMAGE. glidex-netd takes full requests from
# its socket's group: if you aren't in it, the suite runs with that group as
# your primary group for this run only (sudo setpriv; your memberships are
# left as they are). The tests create and delete networks and VMs through
# the host's glidex-netd and Open vSwitch.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

# The image is required here; everything else goes to ui-e2e.sh.
image=${GLIDEX_TEST_IMAGE:-}
args=()
playwright=()
while [ $# -gt 0 ]; do
  case "$1" in
    -i|--image) image=${2:?--image needs a path}; shift 2 ;;
    -h|--help) usage ;;
    --) shift; playwright=("$@"); break ;;
    *) args+=("$1"); shift ;;
  esac
done

[ -n "$image" ] || die "the VM boot test needs a cloud image: -i <path> or GLIDEX_TEST_IMAGE"
image=${image/#\~/$HOME}
[ -f "$image" ] || die "image $image does not exist"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm is not usable by $(id -un) (the kvm group?)"
sock="${GLIDEX_NETD_RUN_DIR:-/run/glidex}/netd.sock"
[ -S "$sock" ] || die "no glidex-netd socket at $sock (is glidex-netd running?)"
netd_group=$(stat -c %G "$sock")

# The scratch control plane's admin group stays yours when the primary
# group changes (scripts/start-control-plane.sh).
export GLIDEX_E2E_GROUP=${GLIDEX_E2E_GROUP:-$(id -gn)}
run=()
if ! id -nG | tr ' ' '\n' | grep -qx "$netd_group"; then
  command -v setpriv >/dev/null || die "setpriv (util-linux) is needed to run with the $netd_group group"
  echo "note: running with $netd_group as the primary group (sudo setpriv), for full access to glidex-netd" >&2
  keep=()
  for v in PATH HOME USER XDG_RUNTIME_DIR CARGO_HOME RUSTUP_HOME CARGO_TARGET_DIR \
           E2E_API_PORT E2E_UI_PORT E2E_UI_SERVER_PORT GLIDEX_E2E_HOME GLIDEX_E2E_GROUP GLIDEX_E2E_LOCK GLIDEX_NETD_RUN_DIR; do
    [ -n "${!v:-}" ] && keep+=("$v=${!v}")
  done
  run=(sudo env "${keep[@]}" setpriv --reuid="$(id -u)" --regid="$netd_group" --init-groups --)
fi

# The list output as usual, and a JSON report to count the skipped tests.
report=$(mktemp --suffix=.json)
trap 'rm -f "$report"' EXIT
status=0
"${run[@]}" env PLAYWRIGHT_JSON_OUTPUT_NAME="$report" \
  "$repo/scripts/dev/ui-e2e.sh" -i "$image" "${args[@]}" -- --reporter=list,json "${playwright[@]}" || status=$?

skipped=$(bun -e 'const r = require(process.argv[1]); console.log(r.stats?.skipped ?? 0)' "$report" 2>/dev/null || echo "?")
if [ "$skipped" != 0 ]; then
  echo "error: $skipped test(s) skipped: the full suite runs every test (each says what it needs above)" >&2
  [ "$status" -ne 0 ] || status=1
fi
exit "$status"

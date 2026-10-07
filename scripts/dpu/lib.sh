#!/bin/bash
# Shared helpers for the D0 DPU bring-up scripts (spec/dpu.md §14, D0).
# Sourced, not run. No secrets live here: targets, keys and paths are
# always arguments, and nothing is hardcoded to a particular DPU.
#
# Conventions
#   - Every script can drive a DPU over SSH (--ssh <user@host>, key auth
#     only, BatchMode) or run on the DPU itself (--local).
#   - read-only commands go through `rd`; commands that change anything go
#     through `mut`, which only prints in --dry-run / --check mode.
#   - `changed` counts what a run did (or would do); --check exits 3 when
#     that is not zero, which is how "a second run changes nothing" is tested.

DPU_SSH=""            # user@host; empty with --local
DPU_LOCAL=0
DRY_RUN=0
CHECK=0
CHANGED=0
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new)
SSH_KEY=""

log()  { printf '%s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

# Parse the target options every script shares; leaves the rest in ARGS.
# Usage: parse_common "$@"; set -- "${ARGS[@]}"
parse_common() {
  ARGS=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --ssh)     DPU_SSH="${2:?--ssh needs user@host}"; shift 2 ;;
      --ssh-key) SSH_KEY="${2:?--ssh-key needs a path}"; shift 2 ;;
      --known-hosts) SSH_OPTS+=(-o "UserKnownHostsFile=${2:?--known-hosts needs a path}"); shift 2 ;;
      --local)   DPU_LOCAL=1; shift ;;
      --dry-run) DRY_RUN=1; shift ;;
      --check)   DRY_RUN=1; CHECK=1; shift ;;
      *)         ARGS+=("$1"); shift ;;
    esac
  done
  if [ -n "$SSH_KEY" ]; then
    [ -r "$SSH_KEY" ] || die "ssh key not readable: $SSH_KEY"
    SSH_OPTS+=(-i "$SSH_KEY" -o IdentitiesOnly=yes)
  fi
}

require_target() {
  if [ "$DPU_LOCAL" = 1 ]; then return 0; fi
  [ -n "$DPU_SSH" ] || die "give --ssh <user@host> (the DPU) or --local (running on the DPU)"
}

# Run a command on the DPU, as root via passwordless sudo (-n: never
# prompts, so a missing rule fails fast instead of hanging).
dpu() {
  if [ "$DPU_LOCAL" = 1 ]; then
    if [ "$(id -u)" = 0 ]; then "$@"; else sudo -n "$@"; fi
  else
    local cmd
    cmd="$(printf '%q ' "$@")"
    ssh "${SSH_OPTS[@]}" "$DPU_SSH" "sudo -n $cmd"
  fi
}

# Like dpu() but a shell snippet (pipes, redirects). The snippet runs as
# root in `sh -c`; keep it free of secrets.
dpu_sh() {
  if [ "$DPU_LOCAL" = 1 ]; then
    if [ "$(id -u)" = 0 ]; then sh -c "$1"; else sudo -n sh -c "$1"; fi
  else
    ssh "${SSH_OPTS[@]}" "$DPU_SSH" "sudo -n sh -c $(printf '%q' "$1")"
  fi
}

# Read-only probe: output on stdout, failure tolerated (empty output).
rd()    { dpu "$@" 2>/dev/null || true; }
rd_sh() { dpu_sh "$1" 2>/dev/null || true; }

# A step that changes the DPU. In --dry-run/--check it only prints.
# $1 = short description, the rest = the command (run through dpu()).
mut() {
  local why="$1"; shift
  CHANGED=$((CHANGED + 1))
  if [ "$DRY_RUN" = 1 ]; then
    log "  would: $why"
    log "         $*"
    return 0
  fi
  log "  do:    $why"
  dpu "$@"
}

mut_sh() {
  local why="$1" snippet="$2"
  CHANGED=$((CHANGED + 1))
  if [ "$DRY_RUN" = 1 ]; then
    log "  would: $why"
    log "         sh -c '$snippet'"
    return 0
  fi
  log "  do:    $why"
  dpu_sh "$snippet"
}

step()  { log ""; log "== $* =="; }
ok()    { log "  ok:    $*"; }
skip()  { log "  skip:  $*"; }
warn()  { log "  WARN:  $*"; }

now()   { date +%s; }

# Final status for scripts that count changes.
finish() {
  log ""
  if [ "$DRY_RUN" = 1 ]; then
    log "changes that would be made: $CHANGED"
  else
    log "changes made: $CHANGED"
  fi
  if [ "$CHECK" = 1 ] && [ "$CHANGED" -ne 0 ]; then exit 3; fi
  exit 0
}

# Remove what must not leave this machine from captured output: IPv4 and
# IPv6 addresses, MACs, UUIDs, serials, OVN/OVS system ids and the names
# of the machines involved. Pass extra names to hide as arguments.
scrub() {
  local sedargs=() n
  for n in "$@"; do
    [ -n "$n" ] && sedargs+=(-e "s/\\b$(printf '%s' "$n" | sed 's/[.[\*^$/]/\\&/g')\\b/<host>/g")
  done
  sed -E \
    -e 's/\b([0-9]{1,3}\.){3}[0-9]{1,3}(\/[0-9]{1,2})?\b/<ipv4>/g' \
    -e 's/\b([0-9a-fA-F]{2}[:-]){5}[0-9a-fA-F]{2}\b/<mac>/g' \
    -e 's/\b([0-9a-fA-F]{1,4}:){4,7}[0-9a-fA-F]{1,4}(%[a-z0-9]+)?(\/[0-9]{1,3})?/<ipv6>/g' \
    -e 's/[0-9a-fA-F]{0,4}(:[0-9a-fA-F]{1,4})*::([0-9a-fA-F]{1,4}(:[0-9a-fA-F]{1,4})*)?(%[a-z0-9]+)?(\/[0-9]{1,3})?/<ipv6>/g' \
    -e 's/MAC\([0-9a-fA-F]{12}/MAC(<mac>/g' \
    -e 's/\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b/<uuid>/g' \
    -e 's/(system-id|ovn-chassis-idx-[0-9a-f-]+|SerialNumber|serial|Serial|SN)([=: ]+)[^ ,}"]+/\1\2<redacted>/g' \
    -e 's/(PEER_MAC|PXE_ID)[ ]+[^ ]+/\1 <redacted>/g' \
    "${sedargs[@]}"
}

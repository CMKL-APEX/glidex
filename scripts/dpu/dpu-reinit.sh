#!/bin/bash
# D0: reinstall a DPU's operating system from a BFB over rshim, with a
# generated bf.cfg, and record what happens (spec/dpu.md §9.4, §14 D0).
# DESTRUCTIVE: the DPU's root filesystem is replaced. Run on the host that
# owns the DPU's rshim device. This is the reference the `dpu_reinit` job
# (D5b) is later checked against; it is not what ships.
#
#   dpu-reinit.sh --rshim rshim0 --bfb PATH --sha256 HEX --pubkey PATH \
#                 --ssh <user@dpu> --ssh-key PATH --log-dir DIR [options] --dry-run
#   ... same without --dry-run, plus --yes, to install
#
# Required
#   --rshim DEV            rshim device under /dev (e.g. rshim0)
#   --bfb PATH             the bundle; its sha256 is checked immediately before use
#   --sha256 HEX           expected checksum
#   --pubkey PATH          a PUBLIC key to authorize on the DPU (a private key is refused)
#   --ssh USER@HOST        how to reach the DPU afterwards (key auth only)
#   --log-dir DIR          where timings, logs and before/after pictures go (0700)
# Options
#   --expect-dev-name PCI  refuse unless rshim `misc` DEV_NAME equals this (recommended)
#   --ssh-key PATH         private key matching --pubkey, for the checks afterwards
#   --target-device DEV    OS target on the DPU (default /dev/nvme0n1)
#   --user NAME            the account glidex gets on the DPU, delivered as its own cloud-init seed
#                          (vendor-data); the shipped `ubuntu` account is left alone (default glidex)
#   --allow-fw-update      WITH_NIC_FW_UPDATE=yes (default no; the bundle default is yes)
#   --update-atf-uefi      UPDATE_ATF_UEFI=yes (default no; the bundle default is yes)
#   --ca-cert PATH         install this CA *certificate* (public) on the DPU
#   --agent-package PATH   install this archive under /opt/glidex on the DPU
#   --keep-password-login  do NOT disable SSH password login on the DPU (leave the image's
#                          default), so an operator can investigate by hand; key-only SSH
#                          failing is then reported, not fatal
#   --wait-secs N          how long to wait for SSH afterwards (default 900)
#   --timeout-secs N       hard limit for bfb-install (default 3600)
#   --yes                  required to install; without it, or with --dry-run, nothing is written
#
# Credentials: no password, token or private key is ever put in bf.cfg or
# a log. bf.cfg is created 0600 in a 0700 temp directory and deleted on exit.
# Firmware (mlxconfig) is never changed; it is only read, before and after.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$here/lib.sh"
# shellcheck source=bfcfg.sh
. "$here/bfcfg.sh"
need sha256sum; need jq

RSHIM=""; BFB=""; SHA=""; PUBKEY=""; LOGDIR=""; EXPECT_DEV=""; TARGET=/dev/nvme0n1
KEEP_PW=0; OSUSER=glidex; FW=no; ATF=no; CACERT=""; AGENT=""; CLEAR_EXP=0; WAIT=900; TIMEOUT=3600; YES=0

parse_common "$@"; set -- "${ARGS[@]}"
while [ $# -gt 0 ]; do
  case "$1" in
    --rshim)            RSHIM="${2:?}"; shift 2 ;;
    --bfb)              BFB="${2:?}"; shift 2 ;;
    --sha256)           SHA="${2:?}"; shift 2 ;;
    --pubkey)           PUBKEY="${2:?}"; shift 2 ;;
    --log-dir)          LOGDIR="${2:?}"; shift 2 ;;
    --expect-dev-name)  EXPECT_DEV="${2:?}"; shift 2 ;;
    --target-device)    TARGET="${2:?}"; shift 2 ;;
    --user)             OSUSER="${2:?}"; shift 2 ;;
    --allow-fw-update)  FW=yes; shift ;;
    --update-atf-uefi)  ATF=yes; shift ;;
    --ca-cert)          CACERT="${2:?}"; shift 2 ;;
    --agent-package)    AGENT="${2:?}"; shift 2 ;;
    --clear-pw-expiry)  die "--clear-pw-expiry cannot work from bf.cfg: the account does not exist at hook time (spec/dpu.md §0 finding 23)" ;;
    --keep-password-login) KEEP_PW=1; shift ;;
    --wait-secs)        WAIT="${2:?}"; shift 2 ;;
    --timeout-secs)     TIMEOUT="${2:?}"; shift 2 ;;
    --yes)              YES=1; shift ;;
    -h|--help)          sed -n '2,38p' "$0"; exit 0 ;;
    *)                  die "unknown option: $1" ;;
  esac
done

# ------------------------------------------------------------------ validate
[[ "$RSHIM" =~ ^rshim[0-9]+$ ]]     || die "--rshim must look like rshim0"
[ -n "$BFB" ] && [ -r "$BFB" ]      || die "--bfb: file not readable"
[[ "$SHA" =~ ^[0-9a-fA-F]{64}$ ]]   || die "--sha256 must be 64 hex characters"
[ -r "$PUBKEY" ]                    || die "--pubkey: file not readable"
[ -n "$LOGDIR" ]                    || die "--log-dir is required"
[[ "$TARGET" =~ ^/dev/[a-z0-9/]+$ ]]|| die "--target-device must be a /dev path"
[[ "$OSUSER" =~ ^[a-z_][a-z0-9_-]*$ ]] || die "--user: not a valid account name"
[[ "$WAIT" =~ ^[0-9]+$ ]] && [[ "$TIMEOUT" =~ ^[0-9]+$ ]] || die "--wait-secs/--timeout-secs must be numbers"
[ -n "$DPU_SSH" ]                   || die "--ssh <user@dpu> is required (it is how the result is verified)"
for f in "$CACERT" "$AGENT"; do [ -z "$f" ] || [ -r "$f" ] || die "not readable: $f"; done

# A public key only: one line, ssh-/ecdsa-/sk- prefix, no private-key armor.
if grep -qE 'PRIVATE KEY' "$PUBKEY"; then die "--pubkey contains a private key; refusing to continue"; fi
[ "$(grep -cE '^(ssh-|ecdsa-|sk-)' "$PUBKEY")" = 1 ] || die "--pubkey must hold exactly one public key line"
PUBLINE="$(grep -E '^(ssh-|ecdsa-|sk-)' "$PUBKEY" | head -1)"
[[ "$PUBLINE" =~ ^(ssh-|ecdsa-|sk-)[A-Za-z0-9@.-]*\ [A-Za-z0-9+/=]+(\ [A-Za-z0-9@._+=:/\ -]*)?$ ]] \
  || die "--pubkey: the key line has characters outside [A-Za-z0-9+/=@._:-] (it is expanded inside the generated hook)"
if [ -n "$CACERT" ] && ! grep -q 'BEGIN CERTIFICATE' "$CACERT"; then die "--ca-cert must be a certificate"; fi
if [ -n "$CACERT" ] && grep -q 'PRIVATE KEY' "$CACERT"; then die "--ca-cert contains a private key; refusing"; fi

SUDO=""; [ "$(id -u)" = 0 ] || SUDO="sudo -n"
need bfb-install

# ------------------------------------------------------------ rshim identity
step "preflight"
misc="$($SUDO cat "/dev/$RSHIM/misc" 2>/dev/null)" || die "cannot read /dev/$RSHIM/misc (is the device there, and sudo -n allowed?)"
field() { printf '%s\n' "$misc" | sed -nE "s/^$1 +//p" | head -1; }
bf_mode="$(field BF_MODE)"; dev_info="$(field DEV_INFO)"; dev_name="$(field DEV_NAME)"; boot_mode="$(field BOOT_MODE)"
[ "${bf_mode%% *}" = DPU ]          || die "rshim reports BF_MODE '$bf_mode', not DPU mode"
case "$dev_info" in BlueField-3*) ok "device: $dev_info, $bf_mode" ;; *) die "unexpected DEV_INFO '$dev_info' (this tooling targets BlueField-3)" ;; esac
if [ -n "$EXPECT_DEV" ]; then
  [ "$dev_name" = "$EXPECT_DEV" ] || die "DEV_NAME is '$dev_name', expected '$EXPECT_DEV': wrong device?"
  ok "DEV_NAME matches the expected function"
else
  warn "no --expect-dev-name; this run would write to DEV_NAME '$dev_name'. Confirm it is the DPU you mean."
fi
[ "$(systemctl is-active rshim 2>/dev/null || true)" = active ] && ok "rshim service active" || warn "rshim service not active"
log "  boot mode before: $boot_mode"

got="$(sha256sum "$BFB" | awk '{print $1}')"
[ "${got,,}" = "${SHA,,}" ] || die "BFB checksum mismatch (got $got): refusing to write anything"
ok "BFB checksum verified ($(basename "$BFB"))"

umask 077
mkdir -p "$LOGDIR"; chmod 700 "$LOGDIR"
WORK="$(mktemp -d)"; chmod 700 "$WORK"
cleanup() { rm -rf "$WORK"; if [ -n "${CAPPID:-}" ]; then kill "$CAPPID" 2>/dev/null || true; fi; }
trap cleanup EXIT

# ------------------------------------------------------------------- bf.cfg
CFG="$WORK/bf.cfg"
gen_bf_cfg > "$CFG"
chmod 600 "$CFG"

if [ "$DRY_RUN" = 1 ] || [ "$YES" != 1 ]; then
  step "plan (nothing is written)"
  log "  would run: bfb-install -r $RSHIM -c <bf.cfg> -b $(basename "$BFB")   (hard limit ${TIMEOUT}s)"
  log "  then wait up to ${WAIT}s for SSH on the DPU, and record the O13/O15/O16/O17 findings in $LOGDIR"
  log ""
  log "  generated bf.cfg:"
  sed -e "s#^#    | #" "$CFG" | sed -E "s#(ssh-[a-z0-9-]+|ecdsa-[a-z0-9-]+|sk-[a-z0-9@.-]+) [A-Za-z0-9+/=]{16,}[^']*#\1 <public key>#"
  [ "$YES" = 1 ] || log "  (this run needs --yes to install)"
  exit 0
fi
[ "$AGENT" = "" ] || warn "--agent-package: only the directory is created in D0; the agent does not exist yet"

# ------------------------------------------------------ before-picture (O13)
step "before-picture"
START="$(now)"
snap() { # snap <tag>
  local t="$1"
  {
    echo "# $t $(date -u +%FT%TZ)"; dpu bfver 2>&1 | head -12; echo "-- bfb_version.json"; dpu cat /etc/bfb_version.json 2>&1
    echo "-- mlxconfig"; dpu mlxconfig -d /dev/mst/mt41692_pciconf0 q VIRTIO_NET_EMULATION_ENABLE VIRTIO_NET_EMULATION_NUM_PF VIRTIO_NET_EMULATION_NUM_VF VIRTIO_NET_EMULATION_NUM_MSIX VIRTIO_NET_EMULATION_NUM_VF_MSIX PCI_SWITCH_EMULATION_ENABLE PER_PF_NUM_SF PF_TOTAL_SF PF_SF_BAR_SIZE SRIOV_EN NUM_OF_VFS INTERNAL_CPU_MODEL 2>&1
    echo "-- block devices"; dpu_sh 'lsblk -o NAME,SIZE,TYPE,MOUNTPOINT 2>&1; findmnt -no SOURCE / 2>&1' 2>&1
    echo "-- boot entries"; dpu_sh 'efibootmgr 2>&1 | head -6' 2>&1
  } 2>&1 | scrub "${DPU_SSH#*@}" "$(hostname)" > "$LOGDIR/$t.txt" || true
}
if dpu true 2>/dev/null; then snap before; ok "saved $LOGDIR/before.txt"; else warn "DPU not reachable over SSH now; no before-picture"; fi
printf '%s\n' "$misc" | scrub "$(hostname)" > "$LOGDIR/rshim-misc-before.txt"

# --------------------------------------------------------------- the install
step "bfb-install (about 10 minutes expected)"
T0="$(now)"
set +e
timeout "$TIMEOUT" $SUDO bfb-install -r "$RSHIM" -c "$CFG" -b "$BFB" 2>&1 | scrub "$(hostname)" | while IFS= read -r l; do printf '%s %s\n' "$(( $(now) - T0 ))s" "$l"; done | tee "$LOGDIR/bfb-install.log"
rc=${PIPESTATUS[0]}
set -e
T1="$(now)"
log "  bfb-install exit=$rc after $((T1 - T0))s"
[ "$rc" = 0 ] || die "bfb-install failed (exit $rc); log: $LOGDIR/bfb-install.log. The DPU is in an unknown state; nothing was retried."

# ------------------------------------------------------- wait + checks (O17)
step "wait for the DPU (SSH, key only)"
# A reinstall regenerates the DPU's SSH host keys, so the entry in the
# operator's known_hosts no longer matches. Trust the new key on first use
# (the link is point-to-point, host to DPU) into a file private to this
# run; the operator's known_hosts is not touched.
# (ssh takes the FIRST value of an option, so any earlier --known-hosts must be
# dropped, not stacked, or it would keep rejecting the new key.)
_opts=(); _i=0
while [ "$_i" -lt "${#SSH_OPTS[@]}" ]; do
  if [ "${SSH_OPTS[$_i]}" = "-o" ] && [[ "${SSH_OPTS[$((_i + 1))]:-}" == UserKnownHostsFile=* ]]; then _i=$((_i + 2)); continue; fi
  _opts+=("${SSH_OPTS[$_i]}"); _i=$((_i + 1))
done
SSH_OPTS=("${_opts[@]}" -o "UserKnownHostsFile=$LOGDIR/known_hosts" -o StrictHostKeyChecking=accept-new)
# Before the install --ssh named an account of the old image; afterwards the
# account glidex created is the one to use (the image's own account is untouched).
DPU_SSH="$OSUSER@${DPU_SSH#*@}"
reach=""; deadline=$(( $(now) + WAIT ))
while [ "$(now)" -lt "$deadline" ]; do
  if ssh "${SSH_OPTS[@]}" -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no "$DPU_SSH" true 2>"$WORK/ssh.err"; then reach=$(( $(now) - T1 )); break; fi
  sleep 10
done
if [ -z "$reach" ]; then
  printf 'last ssh error: %s\n' "$(scrub < "$WORK/ssh.err" | tail -1)" >&2
  if [ "$KEEP_PW" = 1 ]; then
    END="$(now)"
    jq -n --argjson install "$((T1 - T0))" --argjson total "$((END - START))" --arg fw "$FW" --arg atf "$ATF" --arg target "$TARGET" \
      '{install_seconds:$install, ssh_ready_after_install_seconds:null, total_seconds:$total, with_nic_fw_update:$fw,
        update_atf_uefi:$atf, target_device:$target, password_login:"left as shipped",
        note:"key-only SSH did not work within the wait; investigate on the DPU (hook log: /var/log/glidex-bfcfg.log)"}' | tee "$LOGDIR/summary.json"
    warn "key-only SSH not working (see above); the install itself succeeded. Password login was left enabled for investigation."
    log "The image's default password applies and must be changed at first login; do not leave it in place."
    exit 0
  fi
  die "no key-only SSH within ${WAIT}s of the install finishing. Check the console ($RSHIM/console) and $LOGDIR. Note O17 (first-login password gate)."
fi
ok "key-only, non-interactive SSH works $reach s after the install returned (O17)"

sleep 5
snap after; ok "saved $LOGDIR/after.txt"
printf '%s\n' "$($SUDO cat "/dev/$RSHIM/misc" 2>/dev/null)" | scrub "$(hostname)" > "$LOGDIR/rshim-misc-after.txt"
dpu_sh 'cat /var/log/glidex-bfcfg.log 2>&1' 2>&1 | scrub "${DPU_SSH#*@}" > "$LOGDIR/bfcfg-hook.log" || true
ok "hook log: $LOGDIR/bfcfg-hook.log"

# --------------------------------------------------- the account has no password
# The glidex account must not carry any usable password: `passwd -S` must say L
# (locked) and the first character of its shadow field must be '!' or '*' (no
# hash). Only that character and the status letters are read, never the hash.
step "account checks"
gl_status="$(dpu passwd -S "$OSUSER" 2>/dev/null | awk '{print $2}')"
gl_hash1="$(dpu awk -F: -v u="$OSUSER" '$1==u{print substr($2,1,1)}' /etc/shadow 2>/dev/null || true)"
ub_status="$(dpu passwd -S ubuntu 2>/dev/null | awk '{print $2}')"
ub_exists="$(dpu getent passwd ubuntu 2>/dev/null | cut -d: -f1)"
LOCKED=false
if [ "$gl_status" = L ] && { [ "$gl_hash1" = '!' ] || [ "$gl_hash1" = '*' ]; }; then LOCKED=true; fi
{ echo "account $OSUSER: passwd -S status='${gl_status:-?}' (L = locked), shadow field starts with '${gl_hash1:-?}' ('!' or '*' = no hash)";
  echo "account ubuntu (the image's own, untouched by glidex): exists=$([ -n "$ub_exists" ] && echo yes || echo NO) status='${ub_status:-?}'"; } | tee "$LOGDIR/accounts.txt" >&2
if [ "$LOCKED" = true ]; then ok "$OSUSER has no usable password (locked)"; else warn "$OSUSER does NOT look locked: treat this as a failure"; fi

# ------------------------------------------------------------------- summary
END="$(now)"
jq -n --argjson install "$((T1 - T0))" --argjson ssh_ready "$reach" --argjson total "$((END - START))" \
  --arg fw "$FW" --arg atf "$ATF" --arg target "$TARGET" --arg acct "$OSUSER" --arg gl_status "${gl_status:-}" \
  --arg locked "$LOCKED" --arg ub_status "${ub_status:-}" \
  --arg boot_before "$boot_mode" --arg boot_after "$(grep -E '^BOOT_MODE' "$LOGDIR/rshim-misc-after.txt" | sed -E 's/^BOOT_MODE +//')" \
  '{install_seconds:$install, ssh_ready_after_install_seconds:$ssh_ready, total_seconds:$total,
    with_nic_fw_update:$fw, update_atf_uefi:$atf, target_device:$target,
    glidex_account:$acct, glidex_password_status:$gl_status, glidex_password_locked:($locked=="true"), ubuntu_password_status:$ub_status,
    boot_mode_before:$boot_before, boot_mode_after:$boot_after,
    note:"compare before.txt and after.txt for O13 (mlxconfig) and O16 (devices); O17 is the SSH result above"}' | tee "$LOGDIR/summary.json"
log ""
[ "$LOCKED" = true ] || die "the $OSUSER account has a usable password (see $LOGDIR/accounts.txt); it must have none"
log "Next: diff $LOGDIR/before.txt $LOGDIR/after.txt, then run dpu-probe.sh and dpu-prepare.sh --check."
log "The bootstrap key stays authorized on the DPU until you remove it (D5b automates this)."

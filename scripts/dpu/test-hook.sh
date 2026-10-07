#!/bin/bash
# D0: offline test of the bf.cfg hook (bfcfg.sh) against a real cloud-init
# seed, with /mnt redirected to a temporary directory. No root, nothing is
# installed anywhere. Run before every reinstall that changes the hook, and
# for every new bundle version (the shipped seed's shape may change).
#
#   test-hook.sh --seed FILE
#
# FILE is the shipped user-data of the bundle's NoCloud seed, e.g. read from a
# running DPU:  ssh <dpu> sudo cat /var/lib/cloud/seed/nocloud-net/user-data
# It holds the image's default credential: keep it out of git, logs and chat.
# The temp copy lives under TMPDIR and is removed on exit.
#
# What is checked (spec/dpu.md section 9.4.1), in both modes (secure and
# --keep-password-login):
#   - the shipped user-data is not modified at all (the image's `ubuntu`
#     account is left alone);
#   - glidex's account comes from a second seed file, vendor-data, which is a
#     first-boot SHELL SCRIPT (a second `users:` list does not merge: finding 26):
#     valid bash, placeholders substituted, no mention of `ubuntu`, no command
#     that sets a password;
#   - that script is EXECUTED in a sandbox with stubbed useradd/passwd/chown/
#     visudo: it creates a system account, locks the password, installs the key
#     (mode 0600, owned by the account) and a validated sudoers entry, and a second
#     run changes nothing (idempotent);
#   - an existing vendor-data is never overwritten;
#   - the root-owned fallback key file is world-readable (a root-owned 0600 key
#     file is unreadable by the account and silently ignored, which broke runs
#     1-3) and the sshd drop-in matches the mode;
#   - nothing is created under any home directory at install time;
#   - the hook's log masks the key.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
. "$here/lib.sh"
# shellcheck source=bfcfg.sh
. "$here/bfcfg.sh"
need python3

SEED=""
parse_common "$@"; set -- "${ARGS[@]}"
while [ $# -gt 0 ]; do
  case "$1" in
    --seed) SEED="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done
[ -r "$SEED" ] || die "give --seed FILE (the shipped user-data)"

umask 077
T="$(mktemp -d)"
trap 'rm -rf "${T:?}"' EXIT

KEY='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTEST1 hook-test-key'
FAILS=0
check() { # check "description" <command...>
  local d="$1"; shift
  if "$@" >/dev/null 2>&1; then log "  ok:   $d"; else log "  FAIL: $d"; FAILS=$((FAILS + 1)); fi
}

# Run the generated hook with /mnt redirected into $1.
run_hook() { # run_hook <tree> <keep_pw 0|1> <account> [preexisting-vendor-data]
  local tree="$1" keep="$2" acct="$3" pre="${4:-}"
  mkdir -p "$tree/var/lib/cloud/seed/nocloud-net" "$tree/etc/ssh/sshd_config.d" "$tree/var/log" "$tree/home"
  cp "$SEED" "$tree/var/lib/cloud/seed/nocloud-net/user-data"
  [ -z "$pre" ] || printf '%s\n' "$pre" > "$tree/var/lib/cloud/seed/nocloud-net/vendor-data"
  echo "root:x:0:0:root:/root:/bin/bash" > "$tree/etc/passwd"
  FW=no; ATF=no; TARGET=/dev/nvme0n1; OSUSER="$acct"; PUBLINE="$KEY"; KEEP_PW="$keep"; CACERT=""; AGENT=""
  gen_bf_cfg > "$tree/bf.cfg"
  sed -e "s#/mnt#$tree#g" "$tree/bf.cfg" > "$tree/hook.sh"
  bash -n "$tree/hook.sh"
  ( source "$tree/hook.sh"; bfb_modify_os ) > "$tree/hook.out" 2>&1 || true
}
SD() { echo "$1/var/lib/cloud/seed/nocloud-net"; }

# Execute the first-boot script in a sandbox: GLIDEX_ROOT redirects its paths,
# and the commands that need root or touch accounts are stubs that record their
# arguments. `id` succeeds only after the stubbed useradd has "created" the account.
run_vendor_script() { # run_vendor_script <script> <root>
  local script="$1" root="$2" stubs="$2/stubs"
  mkdir -p "$stubs" "$root/var/log" "$root/etc/sudoers.d"
  : > "$root/calls.log"
  for c in useradd passwd chown visudo; do
    printf '#!/bin/bash\necho "%s $*" >> "%s/calls.log"\n[ "%s" = useradd ] && touch "%s/created"\nexit 0\n' "$c" "$root" "$c" "$root" > "$stubs/$c"
  done
  printf '#!/bin/bash\n[ -e "%s/created" ]\n' "$root" > "$stubs/id"
  printf '#!/bin/bash\necho "install $*" >> "%s/calls.log"\nfor a in "$@"; do last="$a"; done\nmkdir -p "$last"\n' "$root" > "$stubs/install"
  chmod +x "$stubs"/*
  GLIDEX_ROOT="$root" PATH="$stubs:$PATH" bash "$script"
}

for mode in secure keep; do
  keep=0; [ "$mode" = keep ] && keep=1
  tree="$T/$mode"; run_hook "$tree" "$keep" glidex
  V="$(SD "$tree")/vendor-data"
  step "mode: $mode"
  check "shipped user-data untouched, byte for byte"          cmp -s "$SEED" "$(SD "$tree")/user-data"
  check "vendor-data is a bash script"                        bash -c "[ \"\$(head -1 '$V')\" = '#!/bin/bash' ]"
  check "script has valid syntax"                             bash -n "$V"
  check "placeholders substituted, account is glidex"         bash -c "! grep -q '@USER@\\|@KEY@' '$V' && grep -q '^U=glidex\$' '$V'"
  check "key present exactly once"                            bash -c "[ \"\$(grep -c 'AAAAC3NzaC1lZDI1NTE5AAAAITEST' '$V')\" = 1 ]"
  check "script never sets a password"                        bash -c "! grep -qE 'chpasswd|--password|useradd .* -p |passwd [^-]|plain_text' '$V'"
  check "script locks the password"                           grep -q '^passwd -l "\$U"$' "$V"
  check "creates a system account (uid below 1000)"           grep -q 'useradd --system' "$V"
  check "script does not mention ubuntu"                      bash -c "! grep -qi ubuntu '$V'"

  box="$T/box-$mode"; mkdir -p "$box"
  run_vendor_script "$V" "$box" > "$box/run1.out" 2>&1 || true
  check "sandbox run succeeded"                               grep -q 'glidex bootstrap done' "$box/var/log/glidex-bootstrap.log"
  check "useradd called once, as a system account with its own home" bash -c "[ \"\$(grep -c '^useradd' '$box/calls.log')\" = 1 ] && grep -q '^useradd --system --create-home --home-dir /var/lib/glidex' '$box/calls.log'"
  check "password locked via passwd -l"                       grep -q '^passwd -l glidex' "$box/calls.log"
  check "key file: mode 0600, content is the key"             bash -c "[ \"\$(stat -c %a '$box/var/lib/glidex/.ssh/authorized_keys')\" = 600 ] && [ \"\$(cat '$box/var/lib/glidex/.ssh/authorized_keys')\" = '$KEY' ]"
  check "key file handed to the account"                      grep -q '^chown glidex:glidex .*authorized_keys' "$box/calls.log"
  check ".ssh directory created for the account, mode 0700"   grep -q '^install -d -m 0700 -o glidex -g glidex .*\.ssh' "$box/calls.log"
  check "sudoers entry written, mode 0440, validated"         bash -c "[ \"\$(cat '$box/etc/sudoers.d/90-glidex')\" = 'glidex ALL=(ALL) NOPASSWD:ALL' ] && [ \"\$(stat -c %a '$box/etc/sudoers.d/90-glidex')\" = 440 ] && grep -q '^visudo -cf' '$box/calls.log'"
  : > "$box/calls.log"; run_vendor_script "$V" "$box" > "$box/run2.out" 2>&1 || true
  check "second run is idempotent (no second useradd)"        bash -c "! grep -q '^useradd' '$box/calls.log'"

  check "fallback key file is world-readable, one line"       bash -c "[ \"\$(stat -c %a '$tree/etc/ssh/authorized_keys.d/glidex')\" = 644 ] && [ \"\$(wc -l < '$tree/etc/ssh/authorized_keys.d/glidex')\" = 1 ]"
  check "AuthorizedKeysFile drop-in names the system path"    grep -q 'authorized_keys.d/%u' "$tree/etc/ssh/sshd_config.d/10-glidex.conf"
  if [ "$mode" = secure ]; then
    check "sshd drop-in disables password login"              grep -q '^PasswordAuthentication no' "$tree/etc/ssh/sshd_config.d/10-glidex.conf"
  else
    check "sshd drop-in leaves password login alone"          bash -c "! grep -q 'PasswordAuthentication' '$tree/etc/ssh/sshd_config.d/10-glidex.conf'"
  fi
  check "nothing created under any home directory"            bash -c "[ -z \"\$(ls -A '$tree/home')\" ]"
  check "log says vendor-data was written"                    grep -q 'vendor-data (first-boot script) written' "$tree/var/log/glidex-bfcfg.log"
  check "log does not contain the key"                        bash -c "! grep -q 'AAAAC3\\|TESTTEST' '$tree/var/log/glidex-bfcfg.log'"
done

step "an existing vendor-data is never overwritten"
tree="$T/preexisting"; run_hook "$tree" 0 glidex "#cloud-config
# operator's own vendor-data"
check "operator's file left as it was"        grep -q "operator's own vendor-data" "$(SD "$tree")/vendor-data"
check "the hook says it did not write it"     grep -q 'vendor-data NOT written' "$tree/var/log/glidex-bfcfg.log"
check "fallback key path still installed"     test -s "$tree/etc/ssh/authorized_keys.d/glidex"

log ""
if [ "$FAILS" = 0 ]; then log "all hook checks passed"; else log "$FAILS check(s) FAILED"; exit 1; fi

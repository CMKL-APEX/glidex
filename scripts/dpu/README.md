# D0: DPU bring-up scripts

Operator-run tooling for milestone **D0** of [spec/31-dpu.md](../../spec/31-dpu.md)
(§14). It proves the DPU preparation sequence (§9.3) and the BFB reinstall
(§9.4) from a clean image **before** any Rust encodes them, answers the
open questions the spec lists, and produces the raw command output that the
`glidex-dpu` parsing tests (D1) will use as fixtures. Nothing here ships.

| Script | Does | Changes anything? |
|---|---|---|
| `dpu-probe.sh` | collects the §9.2 capability fields as JSON; `--fixtures DIR` also saves every raw output, scrubbed | no |
| `dpu-prepare.sh` | the §9.3 sequence as idempotent shell; `--check` / `--dry-run` first; `--virtio-net` also clears the stock SF 0 for the controller (finding 29) | only with `--yes` |
| `test-hook.sh` | offline test of the `bf.cfg` hook against a real seed; no hardware | no |
| `bfcfg.sh` | the `bf.cfg` generator (sourced by the two above) | – |
| `dpu-reinit.sh` | generates `bf.cfg`, runs `bfb-install` over rshim, records timings and before/after pictures | **destructive**, only with `--yes` |
| `lib.sh` | shared helpers: SSH or local execution, dry-run, scrubbing | – |

## Rules

- **No secrets in or from these scripts.** Targets, keys and paths are
  arguments; nothing is hardcoded. `dpu-reinit.sh` accepts a *public* key
  only (it refuses anything containing a private key), never writes a
  password, token or private key into `bf.cfg` or a log, and deletes
  `bf.cfg` on exit. Do not paste credentials into tickets or chat; if one
  is ever exposed, treat it as compromised, rotate it, and check the DPU's
  logs for use.
- Reach the DPU **over the host's management link** (the `tmfifo` network),
  not the underlay: restarting `ovs-vswitchd` drops the underlay bridge's
  address (spec §0 finding 6) and would cut an SSH session that runs
  through it.
- Run `dpu-reinit.sh` only on a DPU that can be wiped, in a window where no
  VM uses it. It refuses a wrong device, a wrong checksum, and a private
  key; it does not check for running VMs (that is D5b's job), so you do.
- Firmware (`mlxconfig`) is **never changed by these scripts**: it is only
  read, before and after. Staging firmware values and the cold reset
  (host first, then the BlueField) are manual steps (§9.1).
- Captured output is scrubbed of IPv4/IPv6 addresses, MACs, UUIDs, serials,
  OVS system ids and host names (`lib.sh: scrub`). **Read every file before
  you commit it.** Scrubbing is a safety net, not a guarantee.

## Prerequisites

On the host: `jq`, `ssh`, `sha256sum`; for `dpu-reinit.sh` also `rshim`
running, `bfb-install`, passwordless `sudo -n` for the rshim device and
`bfb-install`, and the BFB file with its published checksum. On the DPU:
key-based SSH for an account with passwordless `sudo -n`.

## Sequence

All commands take `--ssh <user@dpu>` (add `--ssh-key PATH` if needed) or
`--local` on the DPU itself.

1. **Baseline.** `dpu-probe.sh --ssh … --host --fixtures out/fx > probe-before.json`
2. **Dry run of the prepare script.** `dpu-prepare.sh --ssh … --check`
   shows what it would change, and exits 3 if anything. On a configured
   DPU the answer should be small.
3. **Reinstall dry run.** `dpu-reinit.sh … --dry-run` verifies the rshim
   identity and the BFB checksum and prints the generated `bf.cfg`.
4. **Reinstall (D0 acceptance 3, O14–O17).**
   `dpu-reinit.sh … --yes` (about 10 minutes). Read `after.txt`,
   `bfcfg-hook.log`, `summary.json`, `rshim-misc-*.txt` in `--log-dir`.
   If key-only SSH does not work, repeat with `--clear-pw-expiry` (O17).
5. **Prepare from clean (acceptance 1).**
   `dpu-prepare.sh --ssh … --yes --persist --int-bridge br-int …` (see
   `--help` for the underlay and OVN options), then
   `dpu-prepare.sh --ssh … --check`: it **must** report 0 changes.
6. **Probe again.** `dpu-probe.sh … > probe-after.json`; the §9.2 fields
   should have no gaps (virtio fields excepted until firmware is staged).
7. **O13 (acceptance 2).** With the operator's chosen virtio-net values
   staged and the cold reset done, `dpu-reinit.sh` leaves
   `mlxconfig` output in `before.txt` and `after.txt`: diff them.
8. **O1–O3 (acceptance 4)** need the virtio-net firmware; record the
   representor bridge, the working `virtnet` flags and the minimal
   parameter set.

## What to record

Put each result in spec/31-dpu.md §0 as a numbered finding and close or edit
the matching open question in §15: durations (O15), which devices the BFB
wrote and what `BOOT_MODE` reads (O16), where `authorized_keys` and units
must go and whether key-only first login works (O14, O17), whether
firmware values survive (O13). Copy the reviewed `--fixtures` files to
`crates/glidex-dpu/tests/fixtures/` when that crate exists (D1).

## Status

- `dpu-probe.sh` and `dpu-prepare.sh` have run against a real BlueField-3
  DPU on a clean bundle. From that state one `--yes` run applied the
  sequence and a second `--check` reported **0 changes** (spec §0
  findings 16–18). Probe bugs found on the way (map keys after `{`,
  `devlink` needing a device, OVS's literal `[]`) are fixed.
- `dpu-reinit.sh` has been run **five times** against a real DPU (spec §0
  findings 19-28). **Run 5 worked end to end**: about 5 minutes for the
  install, then key-only SSH as `glidex` about 2 minutes later. The account
  is created at first boot by a vendor-data shell script (a second `users:`
  list does not merge), has no usable password (`passwd -S` `L`, shadow `!`)
  and passwordless sudo; the image's `ubuntu` account and `user-data` are
  untouched. Staged firmware values survive a reinstall but are not active
  until the operator's cold reset.
- Not yet covered: firmware staging and O1–O3 (need the virtio-net
  values), and the `--ovn-*` / `--ssl-*` options of `dpu-prepare.sh`.

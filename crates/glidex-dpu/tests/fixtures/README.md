# DPU probe fixtures

Raw command outputs from the test BlueField-3 (B3140L, DPU mode, DOCA
3.5.0 bundle, OVS-DOCA, `virtio-net-controller` v26.07.10) and its host,
captured on 2026-11-16 with

```bash
scripts/dpu/dpu-probe.sh --ssh <user@dpu> --host --fixtures <dir>
```

for D0 acceptance (5) of [spec/31-dpu.md](../../../../spec/31-dpu.md): the
canned inputs for the `glidex-dpu` probe-parsing tests (D1). Each
`dpu-<name>.txt` is one command the probe runs on the DPU (see `cap` in
the script), each `host-*.txt` one read on the host, and `probe.json` is
what the script derived from them.

Addresses, MACs, UUIDs, system IDs, the card's serial (in `vuid`) and
host names are replaced with `<ipv4>`, `<mac>`, `<uuid>`, `<redacted>`,
`<serial>` and `<host>` (`scrub` in `scripts/dpu/lib.sh`).

| Set | State |
|---|---|
| `pf-only/` | After `dpu-prepare.sh --virtio-net --int-bridge br-int --persist` and a DPU reboot (spec §0 findings 33, 34): 1024 × 2 MB pages, `ovsbr1` and `br-int` on `doca`, no stock SF, the controller serving the static PF `61:00.1` with no VFs. |
| `one-vf/` | The same, plus one VF (finding 35): the PF bound to `pci-pf-stub` with `sriov_drivers_autoprobe=0` and `sriov_numvfs=1`; VF 0 at `61:02.3` with its MAC and MTU set by `virtnet modify` and its representor `en3f0pf0sf3000` on `br-int`. |

`dpu-mlxconfig.txt` shows the *Next Boot* column only (the probe queries
without `-e`); the values were active as well (finding 29).

# Progress

What is built, what is in flight and what comes next, across the areas.
Each area's spec stays the authority for its details: the *Status*
column links to the section that tracks it. How to change things is in
[AGENTS.md](../AGENTS.md).

**Keep it current.** A PR that starts work adds its row to
[*In flight*](#in-flight); the PR that finishes a milestone moves that
area's row and removes its *In flight* row. A PR that installs on a test
host updates [*Hosts*](#hosts). Keep rows to a line or two: details go
in the spec. Milestone ids repeat between specs (`G`, `M`, `S`), so
always give the file.

## In flight

Claimed work: an open PR, or a branch being written for one. Check
`gh pr list` and `git branch -r` too; this table can lag a few hours.

| Area | Milestone | Branch / PR | Notes |
|---|---|---|---|
| Datastores | S26 stale hyperstor holds | `feat/hyperstor-stale-holds` | A disk held by another node's `hyperstor-vhost` when its VM starts is preempted; nodes let go of holds for VMs placed elsewhere. Needs hyperstor's per-volume fencing (D150, D153) |
| Tenancy and directories | T6 leftovers | #192, #193, `feat/t6-two-org-e2e` | Cluster logout ends the directory's chain (D72), changed UI address re-registered (D73); audit per directory (D74); the two-control-plane UI e2e. The board's *Built* row moves when they merge |
| DPU integration | D0 hardware bring-up | `spec/dpu` | Firmware active and the virtio-net PF served after the stock SF is removed (findings 29–31); next: enabling VFs, the representor bridge (O1–O3) |

## Areas

| Area | Spec | Built | Next | Status in the spec |
|---|---|---|---|---|
| Tenancy and directories | [19-tenancy.md](19-tenancy.md) | T0–T6 (T6 through task 8b, #168; T5 kind `jwt`) | T6b directory standby, T7 fleets, T2b Entra and GitHub, T4b LDAP/Graph/GitHub sync: all **not scheduled** | §23 Implementation status |
| Workload identity | [29-workload-identity.md](29-workload-identity.md) | WI0, WI1a, WI1b-1, WI1b-2's first part (metadata answers from the leader, #176) | WI1b-2's second part (tc/eBPF instead of the OpenFlow mark; designed, prototyped); then WI1b-3, WI1c, WI2, WI3, WI4 | §15 Implementation status |
| Managed applications | [26-apps.md](26-apps.md) | Nothing (draft) | A0 (needs WI1) | §14 Milestones |
| Datastores | [24-storage.md](24-storage.md) | S0–S6 (`dir`, `shared-dir`, `lvm`), S8 (`hyperstor`, #177) | S7 CSI adapter (design, gated); per-datastore quotas (queued) | §16 Milestones |
| GPU VMs | [27-gpu-vms.md](27-gpu-vms.md) | GV1–GV3; GV6's code (#206, #211); the create form's GPUs section, with GPUs other VMs hold greyed (#210) | GV6's reboot test, docs (G15) and a pool's `bar_mib`; GV4 system VMs; GV5 Fabric Manager and NVLink (`nvlink: true` refused until then) | §14 Implementation notes |
| Host topology | [22-host-topology.md](22-host-topology.md) | HT0–HT3, most of HT4; HT4b and HT5 per §17's notes (the status line names HT0–HT4) | HT4 `match.labels`; HT4b `max` on `tenant_grants`; HT5 per-VF MAC/VLAN, VFs across reboots | §17 Implementation notes |
| Clustering | [14-clustering.md](14-clustering.md), [16-clustering-ui.md](16-clustering-ui.md) | C0–C8; UI U0–U7 | C7's leftovers (scale run, `--reset`, `--seal-ca-key`, renewal under load, rolling upgrade, recovery exercise, OVN CA rotation); §18 later items need their own design | clustering §20 Implementation notes; clustering-ui §10 |
| Networking | [09-networking.md](09-networking.md) | M1–M8 | M5 full installer run; M7 a DPDK NIC uplink; M8 source build on a host | §0 Implementation status |
| DPU integration | [31-dpu.md](31-dpu.md) | D0 scripts (`scripts/dpu/`); D0 acceptance (1) and (3), (2) half | D0 (2) and (4) on hardware; D1 `glidex-dpu` core and D2 internal OVN in parallel | §14 Milestones |
| Metering | [13-metering.md](13-metering.md) | M0–M4 (M4 not yet run on a host) | M4 host acceptance; §15.8 "After M4" | §15 Milestones |
| Reconciliation | [12-reconciliation.md](12-reconciliation.md) | M1–M4 | M4 on an installed KVM host | §18 Milestones |
| Kernel boot | [23-kernel-boot.md](23-kernel-boot.md) | K0–K4; extraction on a node of the kernel's architecture (#153) | §13 changes to other specs | §12 Milestones |
| Security | [11-security.md](11-security.md) | S0–S6 | On a real host: nftables rules, systemd sandboxing with VFIO, hugepages and taps, a real PAM stack, OIDC against the production IdP | §17 Implementation status |
| RPC API | [18-grpc.md](18-grpc.md) | G0–G5 | — | §15 Implementation status |
| gxctl auth | [17-gxctl-auth.md](17-gxctl-auth.md) | G1–G5 | — | §12 Implementation plan |
| Passkeys | [25-passkeys.md](25-passkeys.md) | All | A real-device check | §7 Implementation status |
| Host page | [28-host-page.md](28-host-page.md) | W1–W6 (#127–#129) | — | §7 Tasks |
| Tailscale certificates | [21-tailscale-tls.md](21-tailscale-tls.md) | TS0–TS2; certificates shared by the node (#171) | TS3 waits on tailscale-rs | §10 Milestones |

Shared code split out recently: `glidex-auth` (#163) and
`@glidex/ui-kit` (#164), which hyperstor's services reuse, and
`glidex-raftstore` (#141).

## Checks waiting on a person

Things only a real account, device or host can show, not run yet (see
[20-idp-checks.md](20-idp-checks.md)):

- Google Workspace directory sync (T4): deferred by the operator.
- GitHub Actions workload federation (T5): needs a test repository and a
  tunnel.
- Google Workspace, Google consumer and JumpCloud OIDC presets (T2): no
  result row yet.
- Passkeys on a real device; SSO through a server's UI from an agent's
  redirect (host page).
- Metering M4 and reconciliation M4 host runs; security's real-host list
  (above).

## Hosts

The shared test hosts and what they are for. Install or reconfigure only
when asked; say here what you left.

| Host | Role | Runs | For |
|---|---|---|---|
| gpu-dev2 | **Standalone** server since 2026-10-28, with Pylon (`--with-pylon`; `pylond` on); OVN installed, off; the Tailscale certificate on. Found empty that day: the earlier cluster's state was gone. Kernel 7.0 since 2026-10-29. **`scx_pylon` is off** (`glidex-pylond.service.d/50-no-scx.conf`, `--no-scx`): attached, it froze, reset or panicked the host four times on 2026-10-29 (CMKL-APEX/pylon#15, #16; kdump dump in `/var/crash/202610291231/`, root only). Installed by hand on 2026-10-29, old binaries as `*.prev`: `pylond` from pylon#16 at 50a497a87, and Glidex from #210's branch (UI at `/usr/local/share/glidex/ui`, old one as `ui.prev`); its control plane, `gxctl` and UI updated to fe98c73 on 2026-10-30, the 253b205 ones kept as `*.253b205`. Left from the GV6 check: a `glidex-vm@.service.d/50-memlock.conf` drop-in (`LimitMEMLOCK=infinity`, now in the unit template too), kdump on (`USE_KDUMP=1`), `/etc/sysctl.d/90-glidex-gv6-debug.conf` (panic on lockups) and credential `gpucheck`. No VMs: the test VMs were deleted at 15:23. GPUs: `03:00.0` on `vfio-pci` with BAR0 at 8 GiB until reboot; `43:00.0` was bound back to `amdgpu` by hand at 18:14 (BAR0 at 32 GiB), while netd's database still has its prepare record | #210's branch at fe98c73 (2026-10-30), merged as 2e6d852 | UI e2e, Pylon, GPU passthrough |
| prism1 | Was an agent of gpu-dev2's cluster, which is gone since gpu-dev2 was found empty (2026-10-28); not touched since. Rejoining takes a reinstall with `--join`; two sockets | #168's build (2026-10-22) | NUMA and passthrough (host topology) |
| spark-1 | **Standalone** since 2026-10-22: its own directory, tenant `spark-lab` was admitted on gpu-dev2's cluster as another organization (T6 check); that admission went with gpu-dev2's state (2026-10-28), so the check needs it admitted again | #168's build (2026-10-22) | aarch64; the two-organization check. Rejoining the cluster takes a reinstall with `--join`, and the aarch64 images re-imported |

## Known flakes

A failure listed here may be rerun once; say in the PR that you did.

- Cluster tests on CI: timing (agents' readiness, departures). Several
  fixed (#158, #161, #178); a rare `build` failure still passes on a
  rerun.

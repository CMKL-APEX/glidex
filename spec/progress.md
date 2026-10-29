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
| Tenancy and directories | T6 leftovers | #192, #193, `feat/t6-two-org-e2e` | Cluster logout ends the directory's chain (D72), changed UI address re-registered (D73); audit per directory (D74); the two-control-plane UI e2e. The board's *Built* row moves when they merge |
| Pylon | Fix: elastic vCPUs in the UI | #200 | An elastic Pylon VM's vCPU count follows max in the create form and the Scale CPU dialog; public docs for Pylon and elastic CPU |

## Areas

| Area | Spec | Built | Next | Status in the spec |
|---|---|---|---|---|
| Tenancy and directories | [tenancy.md](tenancy.md) | T0–T6 (T6 through task 8b, #168; T5 kind `jwt`) | T6b directory standby, T7 fleets, T2b Entra and GitHub, T4b LDAP/Graph/GitHub sync: all **not scheduled** | §23 Implementation status |
| Workload identity | [workload-identity.md](workload-identity.md) | WI0, WI1a, WI1b-1, WI1b-2's first part (metadata answers from the leader, #176) | WI1b-2's second part (tc/eBPF instead of the OpenFlow mark; designed, prototyped); then WI1b-3, WI1c, WI2, WI3, WI4 | §15 Implementation status |
| Managed applications | [apps.md](apps.md) | Nothing (draft) | A0 (needs WI1) | §14 Milestones |
| Datastores | [storage.md](storage.md) | S0–S6 (`dir`, `shared-dir`, `lvm`), S8 (`hyperstor`, #177) | S7 CSI adapter (design, gated); per-datastore quotas (queued) | §16 Milestones |
| GPU VMs | [gpu-vms.md](gpu-vms.md) | GV1–GV3 | GV4 system VMs; GV5 Fabric Manager and NVLink (`nvlink: true` refused until then) | §14 Implementation notes |
| Host topology | [host-topology.md](host-topology.md) | HT0–HT3, most of HT4; HT4b and HT5 per §17's notes (the status line names HT0–HT4) | HT4 `match.labels`; HT4b `max` on `tenant_grants`; HT5 per-VF MAC/VLAN, VFs across reboots | §17 Implementation notes |
| Clustering | [clustering.md](clustering.md), [clustering-ui.md](clustering-ui.md) | C0–C8; UI U0–U7 | C7's leftovers (scale run, `--reset`, `--seal-ca-key`, renewal under load, rolling upgrade, recovery exercise, OVN CA rotation); §18 later items need their own design | clustering §20 Implementation notes; clustering-ui §10 |
| Networking | [networking.md](networking.md) | M1–M8 | M5 full installer run; M7 a DPDK NIC uplink; M8 source build on a host | §0 Implementation status |
| Metering | [metering.md](metering.md) | M0–M4 (M4 not yet run on a host) | M4 host acceptance; §15.8 "After M4" | §15 Milestones |
| Reconciliation | [reconciliation.md](reconciliation.md) | M1–M4 | M4 on an installed KVM host | §18 Milestones |
| Kernel boot | [kernel-boot.md](kernel-boot.md) | K0–K4; extraction on a node of the kernel's architecture (#153) | §13 changes to other specs | §12 Milestones |
| Security | [security.md](security.md) | S0–S6 | On a real host: nftables rules, systemd sandboxing with VFIO, hugepages and taps, a real PAM stack, OIDC against the production IdP | §17 Implementation status |
| RPC API | [grpc.md](grpc.md) | G0–G5 | — | §15 Implementation status |
| gxctl auth | [gxctl-auth.md](gxctl-auth.md) | G1–G5 | — | §12 Implementation plan |
| Passkeys | [passkeys.md](passkeys.md) | All | A real-device check | §7 Implementation status |
| Host page | [host-page.md](host-page.md) | W1–W6 (#127–#129) | — | §7 Tasks |
| Tailscale certificates | [tailscale-tls.md](tailscale-tls.md) | TS0–TS2; certificates shared by the node (#171) | TS3 waits on tailscale-rs | §10 Milestones |

Shared code split out recently: `glidex-auth` (#163) and
`@glidex/ui-kit` (#164), which hyperstor's services reuse, and
`glidex-raftstore` (#141).

## Checks waiting on a person

Things only a real account, device or host can show, not run yet (see
[idp-checks.md](idp-checks.md)):

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
| gpu-dev2 | **Standalone** server since 2026-10-28, with Pylon (`--with-pylon`; `pylond` on; `scx_pylon` not attached: the unit doesn't pass `--enable-scx`, though the kernel, 7.0 since a reboot on 2026-10-29, has `sched_ext`); OVN installed, off; the Tailscale certificate on. Found empty that day: the earlier cluster's state was gone | #200's branch (2026-10-28) | UI e2e, Pylon, GPU passthrough |
| prism1 | Was an agent of gpu-dev2's cluster, which is gone since gpu-dev2 was found empty (2026-10-28); not touched since. Rejoining takes a reinstall with `--join`; two sockets | #168's build (2026-10-22) | NUMA and passthrough (host topology) |
| spark-1 | **Standalone** since 2026-10-22: its own directory, tenant `spark-lab` was admitted on gpu-dev2's cluster as another organization (T6 check); that admission went with gpu-dev2's state (2026-10-28), so the check needs it admitted again | #168's build (2026-10-22) | aarch64; the two-organization check. Rejoining the cluster takes a reinstall with `--join`, and the aarch64 images re-imported |

## Known flakes

A failure listed here may be rerun once; say in the PR that you did.

- Cluster tests on CI: timing (agents' readiness, departures). Several
  fixed (#158, #161, #178); a rare `build` failure still passes on a
  rerun.

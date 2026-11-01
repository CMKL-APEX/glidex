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

## Areas

| Area | Spec | Built | Next | Status in the spec |
|---|---|---|---|---|
| Tenancy and directories | [19-tenancy.md](19-tenancy.md) | T0–T6 (T6 through task 8b, #168; T5 kind `jwt`); T6's leftovers: cluster logout ends the directory's chain and a changed UI address is re-registered (D72, D73, #192), audit per hosted directory (D74, #193), the two-organization UI e2e (#194) | T6b directory standby, T7 fleets, T2b Entra and GitHub, T4b LDAP/Graph/GitHub sync: all **not scheduled** | §23 Implementation status |
| Workload identity | [29-workload-identity.md](29-workload-identity.md) | WI0, WI1a, WI1b-1, WI1b-2's first part (metadata answers from the leader, #176) | WI1b-2's second part (tc/eBPF instead of the OpenFlow mark; designed, prototyped); then WI1b-3, WI1c, WI2, WI3, WI4 | §15 Implementation status |
| Managed applications | [26-apps.md](26-apps.md) | Nothing (draft) | A0 (needs WI1) | §14 Milestones |
| Datastores | [24-storage.md](24-storage.md) | S0–S6 (`dir`, `shared-dir`, `lvm`), S8 (`hyperstor`, #177); S26 stale hyperstor holds preempted (#203); hyperstor pools' space and `--no-overcommit` on them (S27, #213; S27 open for review); full hyperstor pools refuse new disks (S30, open for review) | S7 CSI adapter (design, gated); per-datastore quotas (queued) | §16 Milestones |
| GPU VMs | [27-gpu-vms.md](27-gpu-vms.md) | GV1–GV3; GV6's code (#206, #211); the create form's GPUs section, with GPUs other VMs hold greyed (#210) | GV6's reboot test, docs (G15) and a pool's `bar_mib`; GV4 system VMs; GV5 Fabric Manager and NVLink (`nvlink: true` refused until then) | §14 Implementation notes |
| Host topology | [22-host-topology.md](22-host-topology.md) | HT0–HT3, most of HT4; HT4b and HT5 per §17's notes (the status line names HT0–HT4) | HT4 `match.labels`; HT4b `max` on `tenant_grants`; HT5 per-VF MAC/VLAN, VFs across reboots | §17 Implementation notes |
| Clustering | [14-clustering.md](14-clustering.md), [16-clustering-ui.md](16-clustering-ui.md) | C0–C8; UI U0–U7 | C7's leftovers (scale run, `--reset`, `--seal-ca-key`, renewal under load, rolling upgrade, recovery exercise, OVN CA rotation); §18 later items need their own design | clustering §20 Implementation notes; clustering-ui §10 |
| Networking | [09-networking.md](09-networking.md) | M1–M8 | M5 full installer run; M7 a DPDK NIC uplink; M8 source build on a host | §0 Implementation status |
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
| gpu-dev2 | **Server** of the prism cluster since 2026-10-31 (with prism1 and prism2; advertises `172.19.36.18:8842`), with Pylon (`--with-pylon`; `pylond` on); OVN installed, off; the Tailscale certificate on. Before joining, its networks `wan` and `lab` and credentials `akkarit` and `gpucheck` were deleted (no VMs used them): a plain join refuses a host that has them. Kernel 7.0 since 2026-10-29. **`scx_pylon` is off** (`glidex-pylond.service.d/50-no-scx.conf`, `--no-scx`): attached, it froze, reset or panicked the host four times on 2026-10-29 (CMKL-APEX/pylon#15, #16; kdump dump in `/var/crash/202610291231/`, root only). `pylon` and `pylond` updated by the installer on 2026-10-31 to the build in `~/pylon` (branch `pylon/scx-host-fast-path`); That replaced pylon#16's `pylond` from 2026-10-29, which isn't kept; `pylond.prev` is the one before that. Binaries from earlier hand installs are kept as `*.prev` and `*.253b205`. Left from the GV6 check: a `glidex-vm@.service.d/50-memlock.conf` drop-in (`LimitMEMLOCK=infinity`, now in the unit template too), kdump on (`USE_KDUMP=1`) and `/etc/sysctl.d/90-glidex-gv6-debug.conf` (panic on lockups). GPUs: `03:00.0` on `vfio-pci` with BAR0 at 8 GiB until reboot; `43:00.0` was bound back to `amdgpu` by hand on 2026-10-29 (BAR0 at 32 GiB), while netd's database still has its prepare record | main at 169316c (2026-11-01, through #237, so with #233, #236 (Force off) and #237 (memory and data disks from the UI and gxctl)): built once on prism2 from main (`~/glidex-deploy`) and installed with `--prebuilt` on the other two (`~/glidex-install-lb`), so the three serve the same UI files; `GLIDEX_UI_CLIENT_IP_HEADER=CF-Connecting-IP` in `glidex-ui.service.d/client-ip.conf` | UI e2e, Pylon, GPU passthrough, clustering |
| prism1 | **Server** of the prism cluster since 2026-10-31 (with prism2 and gpu-dev2; advertises `172.16.102.21:8842`); the Tailscale certificate on; two sockets. Purged and reinstalled standalone on 2026-10-30; the uninstall had left OVN set up for an older cluster: `ovn-controller` was disabled and the old `ovn-*` settings and tunnel port removed by hand (OVS's earlier state saved in the installing user's home). Installed from `~/glidex-install-lb` (`~/glidex` is another session's `dpu` branch). Its control plane hung from 16:45 to 18:11 on 2026-10-31, as prism2's had (fixed by #221; restarted). An OpenNebula host too: its VMs hold all the GPUs | main at 169316c (2026-11-01, through #237, so with #233, #236 (Force off) and #237 (memory and data disks from the UI and gxctl)): built once on prism2 from main (`~/glidex-deploy`) and installed with `--prebuilt` on the other two (`~/glidex-install-lb`), so the three serve the same UI files; `GLIDEX_UI_CLIENT_IP_HEADER=CF-Connecting-IP` in `glidex-ui.service.d/client-ip.conf` | NUMA and passthrough (host topology), clustering |
| prism2 | **Server** of the prism cluster and its first (2026-10-31, `gxctl cluster init`; advertises `172.16.102.22:8842`; database before it kept as `glidex.db.pre-cluster-1793406062`); OVN installed, off, at 26.03.0 since 2026-10-31 like prism1 and gpu-dev2 (upgraded by hand from Ubuntu's 24.03.8 with prism1's Cloud Archive source and pin, `glidex-uca-ovn`: the installer matches OVN only to a cluster whose OVN is on); the Tailscale certificate on since 2026-10-31 (`--tailscale-cert`; before that its `ts.net` name got `421`). Its VM `v1` and disks came along. Datastore `hs`, the default, is a pool of the hyperstor pilot cluster (map service on prism2, prism1 and gpu-dev2); prism2 also runs that cluster's `hyperstor-meta`, on ports 8849–8851 (8851 is the UI e2e's default API port: set `E2E_API_PORT` there). Its control plane hung from 07:56 to 15:53 on 2026-10-31: a store write waited for the lane on a Tokio worker and the runtime stopped driving I/O (fixed by #221; restarted) | main at 169316c (2026-11-01, through #237, so with #233, #236 (Force off) and #237 (memory and data disks from the UI and gxctl)): built once on prism2 from main (`~/glidex-deploy`) and installed with `--prebuilt` on the other two (`~/glidex-install-lb`), so the three serve the same UI files; `GLIDEX_UI_CLIENT_IP_HEADER=CF-Connecting-IP` in `glidex-ui.service.d/client-ip.conf` | hyperstor datastores, clustering |
| spark-1 | **Standalone** since 2026-10-22: its own directory, tenant `spark-lab` was admitted on gpu-dev2's cluster as another organization (T6 check); that admission went with gpu-dev2's state (2026-10-28), so the check needs it admitted again | #168's build (2026-10-22) | aarch64; the two-organization check. Rejoining the cluster takes a reinstall with `--join`, and the aarch64 images re-imported |

**`glidex.cmkl.ai`** reaches the prism cluster's three servers through one
Cloudflare tunnel, a `cloudflared` replica on each server; Cloudflare picks
a server per request, with no affinity. Set up for it: `cluster.ui_url` on
all three; the client-IP header above; Cloudflare's Browser Cache TTL set to
respect the servers' headers and its Web Analytics injection off. Each
server's password sign-in for `akkarit` is linked to one user (sign-ins are
per server). Since #223 and #224, account changes, passkeys and the console
work whichever server a request reaches: in a browser on 2026-10-31,
adding a passkey and opening a VM console worked through `glidex.cmkl.ai`.

## Known flakes

A failure listed here may be rerun once; say in the PR that you did.

- Cluster tests on CI: timing (agents' readiness, departures). Several
  fixed (#158, #161, #178); a rare `build` failure still passes on a
  rerun.
- `instance::tests::each_check_alone_shows_life` (control plane, unit):
  failed once on 2026-10-31 while two workspace builds ran side by side;
  passed 3 out of 3 reruns.
- `datastore::hyperstor::tests::regions_go_through_hyperstorctl` (control
  plane, unit): failed once on 2026-10-31 with `Text file busy (os error
  26)`: it writes a script and runs it at once, and a fork in a parallel
  test can inherit the open file. Passed 5 out of 5 reruns and a full
  rerun of the library.

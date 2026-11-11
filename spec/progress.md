# Progress

What is built and what comes next, across the areas.
Each area's spec stays the authority for its details: the *Status*
column links to the section that tracks it. How to change things is in
[AGENTS.md](../AGENTS.md).

**Keep it current.** The PR that finishes a milestone moves that area's
row. Keep rows to a line or two: details go in the spec. Milestone ids
repeat between specs (`G`, `M`, `S`), so always give the file.

What changes many times a day lives on GitHub, not here, so that PRs
don't conflict over it:

| What | Where |
|---|---|
| Work in flight | Open and draft PRs (`gh pr list`); work claimed before its PR exists, an issue labelled [`in-flight`](https://github.com/CMKL-APEX/glidex/issues?q=is%3Aopen+label%3Ain-flight) |
| The shared test hosts: what each is, runs and was left with | One issue per host, labelled [`host`](https://github.com/CMKL-APEX/glidex/issues?q=is%3Aopen+label%3Ahost): the body is its current state, a comment records each install or change |
| Checks waiting on a person | Issues labelled [`check`](https://github.com/CMKL-APEX/glidex/issues?q=is%3Aopen+label%3Acheck); the results go in [20-idp-checks.md](20-idp-checks.md) or the area's spec |
| Known flakes | Issues labelled [`flake`](https://github.com/CMKL-APEX/glidex/issues?q=is%3Aopen+label%3Aflake), closed when fixed |

## Areas

| Area | Spec | Built | Next | Status in the spec |
|---|---|---|---|---|
| Tenancy and directories | [19-tenancy.md](19-tenancy.md) | T0–T6 (T6 through task 8b, #168; T5 kind `jwt`); T6's leftovers: cluster logout ends the directory's chain and a changed UI address is re-registered (D72, D73, #192), audit per hosted directory (D74, #193), the two-organization UI e2e (#194); T8 verified addresses, `allowed_emails` on `oidc` and pre-added users (D75–D77, #243, #244, #246; real check passed 2026-11-03); T9 self-service tenants, tasks 1–4 and 6: directory settings and the display label, users' own tenants, automatic admission with a default cap, open pools, a tenant at first sign-in (D78–D82, #260–#267; task 5's real check passed 2026-11-05); T10 members and roles by verified email (D83, checked with T9) | T6b directory standby, T7 fleets, T2b Entra and GitHub, T4b LDAP/Graph/GitHub sync: all **not scheduled** | §23 Implementation status |
| Workload identity | [29-workload-identity.md](29-workload-identity.md) | WI0, WI1a, WI1b-1, WI1b-2's first part (metadata answers from the leader, #176) | WI1b-2's second part (tc/eBPF instead of the OpenFlow mark; designed, prototyped); then WI1b-3, WI1c, WI2, WI3, WI4 | §15 Implementation status |
| Managed applications | [26-apps.md](26-apps.md) | Nothing (draft) | A0 (needs WI1) | §14 Milestones |
| Datastores | [24-storage.md](24-storage.md) | S0–S6 (`dir`, `shared-dir`, `lvm`), S8 (`hyperstor`, #177); S26 stale hyperstor holds preempted (#203), and a node's own after a reboot (S31); a driver that can't tell whether a volume exists no longer marks it missing (S32); hyperstor pools' space and `--no-overcommit` on them (S27, #213; S27 open for review); full hyperstor pools refuse new disks (S30, open for review) | S7 CSI adapter (design, gated); per-datastore quotas (queued) | §16 Milestones |
| GPU VMs | [27-gpu-vms.md](27-gpu-vms.md) | GV1–GV3; GV6's code (#206, #211; a pool's `bar_mib` #330, the AMD docs #331, Cloud Hypervisor's per-segment check #333); the create form's GPUs section, with GPUs other VMs hold greyed (#210); G20, a pool's cards claimed before their host driver loads (#345; the reboot test passed with it on gpu-dev2, 2026-11-09, and a 1-GPU VM after it once pools were replicated, HT6) | GV4 system VMs; GV5 Fabric Manager and NVLink (`nvlink: true` refused until then) | §14 Implementation notes |
| Host topology | [22-host-topology.md](22-host-topology.md) | HT0–HT3, most of HT4; HT6, replicated and discovered pools with discovered NICs (#399–#420; the acceptance run on gpu-dev2 passed 2026-11-09); HT4b and HT5 per §17's notes | HT4 `match.labels`; HT4b `max` on `tenant_grants`; HT5 per-VF MAC/VLAN, VFs across reboots | §17 Implementation notes |
| Clustering | [14-clustering.md](14-clustering.md), [16-clustering-ui.md](16-clustering-ui.md) | C0–C10; UI U0–U7; the stall watch (D30) | the history tier's spec (D26, after C9); C7's leftovers (scale run, `--reset`, `--seal-ca-key`, renewal under load, rolling upgrade, recovery exercise, OVN CA rotation); §18 later items need their own design | clustering §20 Implementation notes; clustering-ui §10 |
| Networking | [09-networking.md](09-networking.md) | M1–M9 (M9 source routes, #271 #273 #274; checked on prism1, loose and strict `rp_filter`) | M9 with hyperstor endpoints; M5 full installer run; M7 a DPDK NIC uplink; M8 source build on a host; later, not planned: DHCP and DNS without dnsmasq, network boot (§15 Future work) | §0 Implementation status |
| Metering | [13-metering.md](13-metering.md) | M0–M5 (M4's host run passed 2026-11-07; M5, the current hour on a cluster VM's charts, #339 #340) | M5's host check, not run yet; then §15.9 | §15 Milestones |
| Reconciliation | [12-reconciliation.md](12-reconciliation.md) | M1–M4 (M4 checked on an installed KVM host, 2026-11-07); M5 renaming a VM (D21, #326–#328) | — | §18 Milestones |
| Kernel boot | [23-kernel-boot.md](23-kernel-boot.md) | K0–K4; extraction on a node of the kernel's architecture (#153) | §13 changes to other specs | §12 Milestones |
| Pylon | [30-pylon.md](30-pylon.md) | P0–P4 (P4's host e2e not run yet); P5, Pylon pinned to release `v0.0.8-rc1` (`--with-pylon` only, `--pylon-from` for a local build) | Publishing the `v0.0.8-rc1` release; an install from it on a host | §11 Implementation status |
| Security | [11-security.md](11-security.md) | S0–S6 | On a real host: nftables rules, systemd sandboxing with VFIO, hugepages and taps, a real PAM stack, OIDC against the production IdP | §17 Implementation status |
| RPC API | [18-grpc.md](18-grpc.md) | G0–G5 | — | §15 Implementation status |
| gxctl auth | [17-gxctl-auth.md](17-gxctl-auth.md) | G1–G5 | — | §12 Implementation plan |
| Passkeys | [25-passkeys.md](25-passkeys.md) | All | A real-device check | §7 Implementation status |
| Host page | [28-host-page.md](28-host-page.md) | W1–W6 (#127–#129) | — | §7 Tasks |
| Tailscale certificates | [21-tailscale-tls.md](21-tailscale-tls.md) | TS0–TS2; certificates shared by the node (#171) | TS3 waits on tailscale-rs | §10 Milestones |

Shared code split out recently: `glidex-auth` (#163) and
`@glidex/ui-kit` (#164), which hyperstor's services reuse, and
`glidex-raftstore` (#141).

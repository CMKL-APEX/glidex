# Glidex Design Specification

This directory is the authoritative design reference for Glidex — a
control plane for managing KVM-based microVMs across multiple
hypervisor backends (Cloud-Hypervisor, QEMU).

The top-level `README.md` is a *user* document. Documents here are for
contributors: they capture **invariants**, **contracts between
components**, and **why** the code is shaped the way it is — things a
fresh reader cannot infer just by reading the source.

## Contents

| Document | Scope |
|---|---|
| [architecture.md](architecture.md) | Processes, layers, and request/data flow |
| [data-model.md](data-model.md) | VM envelope (`spec`/`status`), VmConfig, phases, persistence schema |
| [reconciliation.md](reconciliation.md) | Desired state, the VM controller, detached VM instances (`glidex-vm-shim`, `glidex-vm@` units), adoption |
| [rest-api.md](rest-api.md) | HTTP endpoints, payloads, error model, console WebSocket |
| [hypervisors.md](hypervisors.md) | Hypervisor driver contract and per-backend command lines |
| [kernel-boot.md](kernel-boot.md) | Direct kernel boot on par with firmware boot (draft): kernel images (kernel + initramfs + cmdline) extracted from the cloud image by a short-lived extractor VM, from a URL or the host; initramfs for both backends; console device chosen by `console=`; generated seeds and credentials on kernel boot; milestones K0–K4 |
| [images.md](images.md) | Image catalog + verified download, managed disks: create/delete, grow/shrink, root-partition extend |
| [credentials.md](credentials.md) | Credential store: guest logins for cloud-init, hashing, security rules |
| [networking.md](networking.md) | VM networking: `glidex-ovs` + root `glidex-netd` — OVS install (distro or pinned source), bridges, uplinks with IP migration, NAT + DHCP, vhost-user/tap VM ports; host-test findings in §0 |
| [console.md](console.md) | The one console per VM (`ttyS0`/`hvc0` on a PTY), its proxy in the shim, listener invariant, log append/rotation, the WebSocket and RPC clients, the browser terminal (ghostty-web/xterm.js), known gaps |
| [cli.md](cli.md) | `gxctl` interactive CLI, command semantics, console attach loop |
| [web-ui.md](web-ui.md) | Vite + React UI structure, routes, API client, dev-proxy |
| [storage.md](storage.md) | Datastores (S0–S5 built, S6–S7 design): cluster-wide storage records with node-local (`dir`, today's directories as `local`) and shared (`shared-dir`, vendor-mounted NFS/CephFS) drivers behind a CSI-shaped driver trait, placement by datastore topology, a worker node per volume, image copies per datastore with relative backing paths, re-placing stopped or lost VMs, disk move, block drivers and a CSI adapter as later gated milestones, S0 fixes, milestones S0–S7 |
| [installer.md](installer.md) | `glidex-install` bootstrap flow and what it brings up |
| [metering.md](metering.md) | Resource usage metering (design): CPU/memory/disk meters, per-VM and per-network traffic from OVS bridge ports with an external/internal split on NAT networks, billing-month totals 95th-percentile bandwidth and disk IOPS/throughput/latency, hourly exactly-once ledger, `/usage` API |
| [cluster-runbook.md](cluster-runbook.md) | Operating a cluster: removing, forgetting and rejoining nodes, rotating the CA, rolling upgrades, recovering a lost quorum |
| [clustering.md](clustering.md) | Multi-host clusters (design): control-plane store replicated with embedded Raft (leader-computed write sets), server and node roles, node lifecycle (remove, forget, rejoin, leave with VMs and import them into another cluster), scheduler with sticky local-storage placement, cluster networks on OVN (isolated, NAT via an HA edge router or per-project VPC routers, provider), cluster IPAM, node PKI with CA rotation, conntrack-based external metering, scale envelope |
| [clustering-ui.md](clustering-ui.md) | Cluster operations in the web UI (plan): capability checks on the right resource type, node awareness in existing pages, placement and capacity, the Cluster page with membership, imports, CA and lifecycle operations, OVN views, the server additions they need, milestones U0–U5 |
| [host-topology.md](host-topology.md) | Host topology and device placement (design): each node's NUMA nodes, CPUs, per-node memory and hugepages and PCIe tree read from sysfs (summary replicated, full tree on request), device readiness and preparation for VFIO, NUMA policy and dedicated CPUs in the VM spec for both hypervisors, NUMA-aware scheduling, hugepage accounting per page size and NUMA node, VFIO claims per node and IOMMU group, device health, device pools for GPUs, NICs and DPUs at cluster and tenant level with quotas and metering, SR-IOV VFs, milestones HT0–HT5 |
| [gpu-vms.md](gpu-vms.md) | GPU VMs (design): GPU requests by count with sizing from the pool, `aligned` NUMA placement across a VM's GPUs' nodes and a guest layout that mirrors the host, NVLink on NVSwitch boards through Fabric Manager's shared NVSwitch mode in a system (service) VM, and `--vfio-devices` / `--sriov` across reboots; milestones GV1–GV5 |
| [security.md](security.md) | Authentication (peer uid, PAM, OIDC, tokens), projects/teams, Cedar authorization policies, netd policy, hardening (draft) |
| [gxctl-auth.md](gxctl-auth.md) | `gxctl auth login` and its config file (`~/.config/glidex/config.json`): login profiles per server/cluster, TLS trust ladder with fingerprint pinning (TOFU) for self-signed control planes, the contained `insecure` bypass, identity generation and administration, server-side additions (`/auth/server-info`, `/auth/token`) |
| [tailscale-tls.md](tailscale-tls.md) | Browser-trusted HTTPS for the web UI on a tailnet (implemented; TS3 waits on tailscale-rs): Tailscale certificates fetched from `tailscaled`'s local API and chosen by SNI, renewal by polling, reloadable certificates in `glidex-tls`, `Host`/`Origin`/HSTS, `TS_PERMIT_CERT_UID`, Certificate Transparency exposure, tailscale-rs as a later phase, installer grant with refusal, same-origin check for the UI peer, milestones TS0–TS3 |
| [tenancy.md](tenancy.md) | Tenants, directories and operators (design): user tenants owning projects that allocate VMs/disks/networks from several clusters; operator tenants owning clusters, nodes and site policies; per-organization directories (users, teams, connectors to the org's own IdP: PAM, LDAP, OIDC, Google, Entra ID, GitHub, JumpCloud) that are OIDC providers, SSO by sharing a directory; pairwise directory↔cluster trust by handshake or brokered by fleets (registries with their own directory and per-organization accounts, claim-code registration, admission policies, labels by provenance, clusters moving between fleets), two-sided admission, filtered feeds, audience-bound tokens, partitions; Cedar boundaries (tenant, operator, directory owner); SCIM and pull sync; directories hosted by operators; workload federation; REST and RPC; milestones T0–T7 |
| [idp-checks.md](idp-checks.md) | Hand checks of the connector presets against real identity providers (Google Workspace and consumer accounts, JumpCloud), with no accounts or secrets in the repository |
| [grpc.md](grpc.md) | RPC API (design; G0 implemented): gRPC, gRPC-Web and Connect via `connectrpc` next to REST on the same listeners, browser on Connect-ES, one transport-neutral service layer under both, proto source of truth in `glidex-api`, per-RPC operation table (Cedar action, read/write, REST twin), error-code mapping, cluster forwarding, port order G0–G5 |

## Goals

1. **Uniform microVM control across hypervisors.** A single REST API
   and CLI drive Cloud-Hypervisor and QEMU identically
   from the caller's point of view, including VFIO PCI passthrough
   and interactive console I/O.
2. **Durable desired state; VMs survive control-plane restarts.** The
   API records what the user wants (`spec`) before acknowledging it; a
   control loop moves the host toward it and reports what it sees
   (`status`). Each VM runs under its own `glidex-vm-shim`, outside the
   control plane's process tree, so stopping, crashing or upgrading the
   control plane leaves guests running; on startup it adopts them
   ([reconciliation.md](reconciliation.md)).
3. **Observable console.** Every VM's serial output is captured to a
   log file *and* broadcast to any number of concurrent clients
   (CLI + browser) over the same Unix socket. Clients that connect
   after the guest has died still get a useful view.
4. **Minimal host dependencies, maximal out-of-the-box experience.**
   One `cargo run -p glidex-install` stands the system up. Cloud-
   Hypervisor boots stock distro cloud images through the EDK2 UEFI
   firmware the installer downloads to `~/.glidex/CLOUDHV.fd`,
   provisioned by an auto-generated cloud-init seed.

## Non-goals

- Clustering / multi-host orchestration. Planned in
  [clustering.md](clustering.md); this non-goal is lifted when its
  milestones land.
- Built-in identity provider. Users authenticate with local accounts
  (PAM, peer credentials) or an external OIDC IdP; authorization is
  Cedar policy. See [security.md](security.md).
- Static guest addressing, and persistent (netplan/NetworkManager) host network
  changes; see [networking.md](networking.md).

## Repository layout

```
glidex/
├── Cargo.toml                       # Workspace root
├── README.md                        # User-facing readme
├── spec/                            # (this directory)
├── packaging/                       # systemd units (netd, authd, control plane, UI, glidex-vm@), polkit rule
└── crates/
    ├── glidex-api/                  # RPC protos, generated messages/services/clients, operation table (grpc.md)
    ├── glidex-control-plane/        # REST and RPC server, controllers, hypervisor drivers, gxctl
    ├── glidex-vm-shim/              # per-VM supervisor: hypervisor parent, console, exit record
    ├── glidex-hv-client/            # CH HTTP and QMP clients (shim + control plane)
    ├── glidex-ovs/                  # host networking library (OVS, NAT, VM ports)
    ├── glidex-netd/                 # root networking helper daemon + protocol client
    ├── glidex-authd/                # root PAM helper daemon (security.md §5.3) + client
    ├── glidex-tls/                  # HTTPS listeners, self-signed certificates, client trust (security.md §5.1)
    ├── glidex-install/              # `cargo run -p glidex-install` bootstrapper
    └── glidex-ui/                   # Vite+React UI; the Rust bin serves it and proxies /api
```

Conventions that apply across documents:

- File paths in these docs are relative to the repo root.
- Code references use `path:line` so they navigate in most tooling.
- Any claim about runtime behavior that is not obvious from the
  source should be backed by a `Why:` or `Invariant:` note.

# Glidex Design Specification

This directory is the authoritative design reference for Glidex — a
control plane for managing KVM-based microVMs across multiple
hypervisor backends (Cloud-Hypervisor, QEMU).

The top-level `README.md` is a *user* document. Documents here are for
contributors: they capture **invariants**, **contracts between
components**, and **why** the code is shaped the way it is — things a
fresh reader cannot infer just by reading the source.

## Contents

Each spec's file name starts with a number, in the order the specs were
added. A new spec takes the next number (`31-…`); a spec's number never
changes and is never reused. Cite a spec by its file name and section
or decision, such as `27-gpu-vms.md §10` or `19-tenancy.md D48`.
`progress.md`, this file and `SUMMARY.md` have no number.

| Document | Scope |
|---|---|
| [progress.md](progress.md) | What is built, in flight and next across the areas; the test hosts; known flakes. Kept current by each PR ([AGENTS.md](../AGENTS.md)) |
| [00-architecture.md](00-architecture.md) | Processes, layers, and request/data flow |
| [03-data-model.md](03-data-model.md) | VM envelope (`spec`/`status`), VmConfig, phases, persistence schema |
| [12-reconciliation.md](12-reconciliation.md) | Desired state, the VM controller, detached VM instances (`glidex-vm-shim`, `glidex-vm@` units), adoption |
| [06-rest-api.md](06-rest-api.md) | HTTP endpoints, payloads, error model, console WebSocket |
| [04-hypervisors.md](04-hypervisors.md) | Hypervisor driver contract and per-backend command lines |
| [30-pylon.md](30-pylon.md) | Pylon hypervisor (`CMKL-APEX/pylon`, `pylond`, `scx_pylon`) and generalized CPU elasticity (`min_vcpus`, `max_vcpus`, `cpu_weight`) with ACPI CPU hotplug fallback on Cloud Hypervisor and QEMU; milestones P0–P4 |
| [23-kernel-boot.md](23-kernel-boot.md) | Direct kernel boot on par with firmware boot (draft): kernel images (kernel + initramfs + cmdline) extracted from the cloud image by a short-lived extractor VM, from a URL or the host; initramfs for both backends; console device chosen by `console=`; generated seeds and credentials on kernel boot; milestones K0–K4 |
| [10-images.md](10-images.md) | Image catalog + verified download, managed disks: create/delete, grow/shrink, root-partition extend |
| [08-credentials.md](08-credentials.md) | Credential store: guest logins for cloud-init, hashing, security rules |
| [09-networking.md](09-networking.md) | VM networking: `glidex-ovs` + root `glidex-netd` — OVS install (distro or pinned source), bridges, uplinks with IP migration, NAT + DHCP, vhost-user/tap VM ports; host-test findings in §0 |
| [31-dpu.md](31-dpu.md) | DPU integration (design): virtio-net ports on OVS-DOCA switched by glidex's own single-node OVN (NB/SB/northd as glidex-owned units, private CA, host chassis), a DPU agent, VFIO attach, `dpu_*`/`ovn_*` netd ops, DPU reinitialization from a BFB over rshim (§9.4), host OVS-DPDK vhost-user fallback on the same logical switch; hardware findings in §0; milestones D0–D8 |
| [02-console.md](02-console.md) | The one console per VM (`ttyS0`/`hvc0` on a PTY), its proxy in the shim, listener invariant, log append/rotation, the WebSocket and RPC clients, the browser terminal (ghostty-web/xterm.js), known gaps |
| [01-cli.md](01-cli.md) | `gxctl` interactive CLI, command semantics, console attach loop |
| [07-web-ui.md](07-web-ui.md) | Vite + React UI structure, routes, API client, dev-proxy |
| [24-storage.md](24-storage.md) | Datastores (S0–S6 and S8 built, S7 design): cluster-wide storage records with node-local (`dir`, today's directories as `local`; `lvm`) and shared (`shared-dir`, vendor-mounted NFS/CephFS; `hyperstor`, a hyperstor cluster's volumes over vhost-user-blk) drivers behind a CSI-shaped driver trait, placement by datastore topology, a worker node per volume, image copies per datastore with relative backing paths, re-placing stopped or lost VMs, disk move, block drivers and a CSI adapter as later gated milestones, S0 fixes, milestones S0–S8 |
| [05-installer.md](05-installer.md) | `glidex-install` bootstrap flow and what it brings up |
| [13-metering.md](13-metering.md) | Resource usage metering (design): CPU/memory/disk meters, per-VM and per-network traffic from OVS bridge ports with an external/internal split on NAT networks, billing-month totals 95th-percentile bandwidth and disk IOPS/throughput/latency, hourly exactly-once ledger, `/usage` API |
| [15-cluster-runbook.md](15-cluster-runbook.md) | Operating a cluster: removing, forgetting and rejoining nodes, rotating the CA, rolling upgrades, recovering a lost quorum |
| [14-clustering.md](14-clustering.md) | Multi-host clusters (design): control-plane store replicated with embedded Raft (leader-computed write sets), server and node roles, node lifecycle (remove, forget, rejoin, leave with VMs and import them into another cluster), scheduler with sticky local-storage placement, cluster networks on OVN (isolated, NAT via an HA edge router or per-project VPC routers, provider), cluster IPAM, node PKI with CA rotation, conntrack-based external metering, scale envelope |
| [16-clustering-ui.md](16-clustering-ui.md) | Cluster operations in the web UI (plan): capability checks on the right resource type, node awareness in existing pages, placement and capacity, the Cluster page with membership, imports, CA and lifecycle operations, OVN views, the server additions they need, milestones U0–U5 |
| [28-host-page.md](28-host-page.md) | Host page and the UI on agents (implemented): a page per host for its OVS, bridges, uplinks, devices and maintenance, standalone or from any server; the target-node header; servers publishing their UI address (`cluster.ui_url` for a load balancer); agents redirecting browsers to a server |
| [22-host-topology.md](22-host-topology.md) | Host topology and device placement (design): each node's NUMA nodes, CPUs, per-node memory and hugepages and PCIe tree read from sysfs (summary replicated, full tree on request), device readiness and preparation for VFIO, NUMA policy and dedicated CPUs in the VM spec for both hypervisors, NUMA-aware scheduling, hugepage accounting per page size and NUMA node, VFIO claims per node and IOMMU group, device health, device pools for GPUs, NICs and DPUs at cluster and tenant level with quotas and metering, SR-IOV VFs, milestones HT0–HT5 |
| [27-gpu-vms.md](27-gpu-vms.md) | GPU VMs (design): GPU requests by count with sizing from the pool, `aligned` NUMA placement across a VM's GPUs' nodes and a guest layout that mirrors the host, NVLink on NVSwitch boards through Fabric Manager's shared NVSwitch mode in a system (service) VM, and `--vfio-devices` / `--sriov` across reboots; milestones GV1–GV5 |
| [11-security.md](11-security.md) | Authentication (peer uid, PAM, OIDC, tokens), projects/teams, Cedar authorization policies, netd policy, hardening (draft) |
| [17-gxctl-auth.md](17-gxctl-auth.md) | `gxctl auth login` and its config file (`~/.config/glidex/config.json`): login profiles per server/cluster, TLS trust ladder with fingerprint pinning (TOFU) for self-signed control planes, the contained `insecure` bypass, identity generation and administration, server-side additions (`/auth/server-info`, `/auth/token`) |
| [21-tailscale-tls.md](21-tailscale-tls.md) | Browser-trusted HTTPS for the web UI on a tailnet (implemented; TS3 waits on tailscale-rs): Tailscale certificates fetched from `tailscaled`'s local API and chosen by SNI, renewal by polling, reloadable certificates in `glidex-tls`, `Host`/`Origin`/HSTS, `TS_PERMIT_CERT_UID`, Certificate Transparency exposure, tailscale-rs as a later phase, installer grant with refusal, same-origin check for the UI peer, milestones TS0–TS3 |
| [19-tenancy.md](19-tenancy.md) | Tenants, directories and operators (design): user tenants owning projects that allocate VMs/disks/networks from several clusters; operator tenants owning clusters, nodes and site policies; per-organization directories (users, teams, connectors to the org's own IdP: PAM, LDAP, OIDC, Google, Entra ID, GitHub, JumpCloud) that are OIDC providers, SSO by sharing a directory; pairwise directory↔cluster trust by handshake or brokered by fleets (registries with their own directory and per-organization accounts, claim-code registration, admission policies, labels by provenance, clusters moving between fleets), two-sided admission, filtered feeds, audience-bound tokens, partitions; Cedar boundaries (tenant, operator, directory owner); SCIM and pull sync; directories hosted by operators; workload federation; REST and RPC; milestones T0–T7 |
| [29-workload-identity.md](29-workload-identity.md) | Workload identity (draft): the directory as issuer of short-lived, audience-bound tokens for its VMs (SPIFFE IDs, OIDC discovery and JWKS), a per-node `glidex-metad` serving a link-local metadata endpoint (VMs identified by port) and vsock, per-directory SPIFFE trust domains, 15 min tokens refreshed at 80%, service accounts attached to VMs (`useServiceAccount`), glidex tokens without stored secrets, the T6 shape (delegated cluster keys) and room for agent delegation; milestones WI0–WI4 |
| [26-apps.md](26-apps.md) | Managed applications (draft): Kubernetes, PostgreSQL and later applications as a layer above a generic core: labels, owners, `user_data`, fixed and allowed addresses, disk hot-plug, and opaque `App`/`AppInstance` resources; the `glidex-apps` service acting as each instance's own service account by impersonation; a generic Apps UI rendered from descriptors (JSON Schema); single-VM PostgreSQL pilot; Kubernetes through Cluster API; milestones A0–A4 |
| [20-idp-checks.md](20-idp-checks.md) | Hand checks of the connector presets against real identity providers (Google Workspace and consumer accounts, JumpCloud), with no accounts or secrets in the repository |
| [18-grpc.md](18-grpc.md) | RPC API (implemented, G0–G5): gRPC, gRPC-Web and Connect via `connectrpc` next to REST on the same listeners, browser on Connect-ES, one transport-neutral service layer under both, proto source of truth in `glidex-api`, per-RPC operation table (Cedar action, read/write, REST twin), error-code mapping, cluster forwarding, port order G0–G5 |

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
   ([12-reconciliation.md](12-reconciliation.md)).
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

- Built-in identity provider. Users authenticate with local accounts
  (PAM, peer credentials) or an external OIDC IdP; authorization is
  Cedar policy. See [11-security.md](11-security.md).
- Static guest addressing, and persistent (netplan/NetworkManager) host network
  changes; see [09-networking.md](09-networking.md).

## Repository layout

```
glidex/
├── Cargo.toml                       # Workspace root
├── README.md                        # User-facing readme
├── spec/                            # (this directory)
├── packaging/                       # systemd units (netd, authd, control plane, UI, glidex-vm@), polkit rule
└── crates/
    ├── glidex-api/                  # RPC protos, generated messages/services/clients, operation table (18-grpc.md)
    ├── glidex-control-plane/        # REST and RPC server, controllers, hypervisor drivers, gxctl
    ├── glidex-vm-shim/              # per-VM supervisor: hypervisor parent, console, exit record
    ├── glidex-hv-client/            # CH HTTP and QMP clients (shim + control plane)
    ├── glidex-raftstore/            # replicated ReDB store on openraft, write sets (14-clustering.md)
    ├── glidex-ovs/                  # host networking library (OVS, NAT, VM ports)
    ├── glidex-ovn/                  # OVN northbound management for cluster networks (14-clustering.md)
    ├── glidex-netd/                 # root networking helper daemon + protocol client
    ├── glidex-authd/                # root PAM helper daemon (11-security.md §5.3) + client
    ├── glidex-stord/                # root storage helper: LVM volumes, device access (24-storage.md)
    ├── glidex-tls/                  # HTTPS listeners, self-signed certificates, client trust (11-security.md §5.1)
    ├── glidex-install/              # `cargo run -p glidex-install` bootstrapper
    ├── glidex-e2e/                  # end-to-end tests driving real installs (nested VMs)
    └── glidex-ui/                   # Vite+React UI; the Rust bin serves it and proxies /api
```

Conventions that apply across documents:

- File paths in these docs are relative to the repo root.
- Code references use `path:line` so they navigate in most tooling.
- Any claim about runtime behavior that is not obvious from the
  source should be backed by a `Why:` or `Invariant:` note.

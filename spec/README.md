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
| [images.md](images.md) | Image catalog + verified download, managed disks: create/delete, grow/shrink, root-partition extend |
| [credentials.md](credentials.md) | Credential store: guest logins for cloud-init, hashing, security rules |
| [networking.md](networking.md) | VM networking: `glidex-ovs` + root `glidex-netd` — OVS install (distro or pinned source), bridges, uplinks with IP migration, NAT + DHCP, vhost-user/tap VM ports; host-test findings in §0 |
| [console.md](console.md) | Console proxy in the shim, listener invariant, log append/rotation, WebSocket bridge, xterm |
| [cli.md](cli.md) | `gxctl` interactive CLI, command semantics, console attach loop |
| [web-ui.md](web-ui.md) | Vite + React UI structure, routes, API client, dev-proxy |
| [installer.md](installer.md) | `glidex-install` bootstrap flow and what it brings up |
| [metering.md](metering.md) | Resource usage metering (design): CPU/memory/disk meters, per-VM and per-network traffic from OVS bridge ports with an external/internal split on NAT networks, billing-month totals 95th-percentile bandwidth and disk IOPS/throughput/latency, hourly exactly-once ledger, `/usage` API |
| [clustering.md](clustering.md) | Multi-host clusters (design): control-plane store replicated with embedded Raft (leader-computed write sets), server and node roles, node lifecycle (remove, forget, rejoin, leave with VMs and import them into another cluster), scheduler with sticky local-storage placement, cluster networks on OVN (isolated, NAT via an HA edge router or per-project VPC routers, provider), cluster IPAM, node PKI with CA rotation, conntrack-based external metering, scale envelope |
| [security.md](security.md) | Authentication (peer uid, PAM, OIDC, tokens), projects/teams, Cedar authorization policies, netd policy, hardening (draft) |

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
    ├── glidex-control-plane/        # REST server, controllers, hypervisor drivers, gxctl
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

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
| [data-model.md](data-model.md) | VM/VmConfig/VmState types, persistence schema, reconciliation |
| [rest-api.md](rest-api.md) | HTTP endpoints, payloads, error model, console WebSocket |
| [hypervisors.md](hypervisors.md) | Hypervisor trait contract and per-backend implementations |
| [credentials.md](credentials.md) | Credential store: guest logins for cloud-init, hashing, security rules |
| [networking.md](networking.md) | VM networking: `glidex-ovs` + root `glidex-netd` — OVS install (distro or pinned source), bridges, uplinks with IP migration, NAT + DHCP, vhost-user/tap VM ports; host-test findings in §0 |
| [console.md](console.md) | Console proxy thread, listener invariant, WebSocket bridge, xterm |
| [cli.md](cli.md) | `gxctl` interactive CLI, command semantics, console attach loop |
| [web-ui.md](web-ui.md) | Vite + React UI structure, routes, API client, dev-proxy |
| [installer.md](installer.md) | `glidex-install` bootstrap flow and what it brings up |

## Goals

1. **Uniform microVM control across hypervisors.** A single REST API
   and CLI drive Cloud-Hypervisor and QEMU identically
   from the caller's point of view, including VFIO PCI passthrough
   and interactive console I/O.
2. **Durable VM state.** A VM survives control-plane restarts: its
   config is persisted before being acknowledged, and process-level
   orphaning is reconciled on startup.
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

- Clustering / multi-host orchestration.
- User authentication and authorization on the REST API. The control
  plane listens on loopback only by default (`127.0.0.1:8841` and
  `[::1]:8841`; `GLIDEX_LISTEN` overrides) because anyone who can reach it
  can manage VMs and, through glidex-netd, host networking. There are no
  accounts, tokens, or ACLs.
- Networking beyond Cloud Hypervisor VMs (QEMU NICs), static guest
  addressing, and persistent (netplan/NetworkManager) host network
  changes; see [networking.md](networking.md).

## Repository layout

```
glidex/
├── Cargo.toml                       # Workspace root
├── README.md                        # User-facing readme
├── spec/                            # (this directory)
├── packaging/                       # systemd unit for glidex-netd
└── crates/
    ├── glidex-control-plane/        # REST server + hypervisor backends + gxctl
    ├── glidex-ovs/                  # host networking library (OVS, NAT, VM ports)
    ├── glidex-netd/                 # root networking helper daemon + protocol client
    ├── glidex-install/              # `cargo run -p glidex-install` bootstrapper
    └── glidex-ui/                   # Vite+React UI; the Rust bin launches `bun run dev`
```

Conventions that apply across documents:

- File paths in these docs are relative to the repo root.
- Code references use `path:line` so they navigate in most tooling.
- Any claim about runtime behavior that is not obvious from the
  source should be backed by a `Why:` or `Invariant:` note.

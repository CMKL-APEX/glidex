# Glidex

A Rust control plane for KVM virtual machines on
[Cloud Hypervisor](https://www.cloudhypervisor.org/) (default) or
[QEMU](https://www.qemu.org/), with a REST API, a web UI and a CLI (`gxctl`).

## Features

- **Two hypervisors, one feature set**: Cloud Hypervisor and QEMU both boot
  distro cloud images via UEFI (`CLOUDHV.fd` / OVMF), take networks, disks
  and VFIO devices, and support pause/resume and graceful shutdown
- **Images and disks**: pull checksum-verified cloud images from a built-in
  catalog; thin root disks per VM, grown with their root partition
- **Guest logins**: stored credentials (password hash + SSH keys) applied by
  an auto-generated cloud-init seed
- **Networking**: Open vSwitch via a root helper (`glidex-netd`): NAT
  networks with DHCP, bridged uplinks (kernel, AF_XDP, DPDK), tap and
  vhost-user NICs
- **Console**: serial console in the browser or `gxctl`, logged to disk,
  shared by any number of clients
- **Projects and access**: projects with quotas and members, Cedar
  policies, API tokens and OIDC, an audit log
- **Clustering**: many hosts as one. Servers keep the cluster's state in a
  Raft-replicated store; agents only run VMs. Nodes join with a one-time
  token over mutual TLS (the cluster's own CA, rotated without downtime);
  a scheduler places VMs and explains its choice; nodes drain, leave and
  rejoin; a standalone host's VMs and disks can be imported into a
  cluster, and a node detached with its resources. Every node serves the
  web UI and relays it to a server
- **Cluster networks on OVN**: networks that span every node, with
  Geneve tunnels between them (and their status in the UI). NAT networks
  leave through a shared edge on failover gateway nodes, with defaults
  suggested from what the nodes report; a project can have its own
  routers, each with its own outside address, and attach NAT or isolated
  networks to them. Host uplinks, physical networks and each node's
  `br-int` datapath are set from the UI, and an uplink that took the
  host's address is re-applied, or rolled back, safely at boot

## Documentation

The user and operator documentation is in [docs/public](docs/public/src/SUMMARY.md)
(an [mdBook](https://rust-lang.github.io/mdBook/): `mdbook serve docs/public`):
a [quick start](docs/public/src/quick-start.md), a
[user guide](docs/public/src/user-guide/README.md), an
[operator guide](docs/public/src/operator-guide/README.md), and references
for [gxctl](docs/public/src/reference/gxctl.md) and the
[API](docs/public/src/api/README.md). The design specs for contributors are
in [spec/](spec/README.md); [docs/README.md](docs/README.md) explains the
split.

## Install

It needs Linux with KVM (`/dev/kvm`), `sudo`, `git` and a Rust toolchain
(`cargo`, e.g. from [rustup.rs](https://rustup.rs)) to run the installer;
the installer keeps Rust up to date afterwards.

```bash
git clone https://github.com/CMKL-APEX/glidex.git && cd glidex
cargo run -p glidex-install                   # install, or update everything
cargo run -p glidex-install -- uninstall      # --dry-run shows what would go
```

The installer asks for your sudo password and, once, on a host on a
Tailscale tailnet with HTTPS certificates, whether the web UI should
use the node's Tailscale certificate (see [Run](#run)). It installs what is
missing and updates what is outdated: system packages, Rust, Bun, Cloud
Hypervisor and its firmware (pinned versions, downloads checked against
pinned sha256s), QEMU with OVMF, VM networking (Open vSwitch,
`glidex-netd`), and the glidex binaries. The control plane runs as a
sandboxed systemd unit under the `glidex` system user (data in
`/var/lib/glidex-control-plane/.glidex`), the web UI as `glidex-ui`, and
`glidex-authd` checks local passwords (PAM). You are added to
`glidex-users` and `glidex-admin` (gxctl on `/run/glidex-cp/api.sock`)
and taken out of `glidex` if an earlier install put you there; log out
and back in afterwards. Defaults need no config file;
`/etc/glidex/control-plane.json.example` lists the settings. Re-run it
to update. `--no-qemu`, `--no-networking` and `--no-services` opt out;
Open vSwitch uses the kernel datapath unless `--ovs-profile dpdk`
(choices are remembered for later runs; `--help` for all). See
[Installation](docs/public/src/operator-guide/installation.md).

## Run

With the systemd units, glidex already runs: the web UI is on
`https://<host>:5173` and the API on `https://<host>:8841`. To run it
yourself instead:

```bash
cargo run --bin glidex-control-plane   # API on https://localhost:8841
cargo run -p glidex-ui -- --dev        # web UI (Vite, hot reload) on https://localhost:5173
cargo run --bin gxctl                  # CLI
```

Both serve HTTPS on every address of the host. Unless you configure a
certificate (`tls` in `/etc/glidex/control-plane.json`;
`GLIDEX_UI_TLS_CERT` / `GLIDEX_UI_TLS_KEY` for the UI), each one
generates a self-signed certificate on first start and prints its SHA-256
fingerprint (so does the installer): compare it with what your browser
shows before accepting the warning. On a host on a Tailscale tailnet with
HTTPS certificates, the web UI can instead serve the node's Let's Encrypt
certificate at `https://<node>.<tailnet>.ts.net:5173`, trusted by browsers
with no warning (installer option `--tailscale-cert`; see
[Tailscale certificates](docs/public/src/operator-guide/tls.md#tailscale-certificates)).
`listen` / `GLIDEX_LISTEN` and
`GLIDEX_UI_LISTEN` narrow the addresses; plain HTTP (`"tls": "off"`,
`GLIDEX_UI_TLS=off`) is only allowed on loopback. The installer doesn't
touch the firewall.

The API also listens on `api.sock` in its run directory
(`/run/glidex-cp/api.sock` under systemd, for members of
`glidex-users`); every request is authenticated. See
[TLS](docs/public/src/operator-guide/tls.md) and
[Authentication](docs/public/src/operator-guide/authentication.md).

`gxctl` talks to the control plane's Unix socket (`/run/glidex-cp/api.sock`,
or the one a hand-started control plane creates under `$XDG_RUNTIME_DIR/glidex`)
and you're identified by your Unix account: no login. From another
machine, `gxctl auth login --url https://host:8841 --profile lab --oidc`
(or `--pam-user <user>`, or `--token` with the token on stdin) saves a
token in a login profile in `~/.config/glidex/config.json`, and pins the
server's certificate after showing you its fingerprint. Once a profile
exists, `gxctl` uses profiles instead of the local socket. Tokens expire
(`gxctl token create`: 90 days unless you ask otherwise). See
[Logging in](docs/public/src/user-guide/logging-in.md). `--project <p>` picks the
project; `whoami`, `project`, `token`, `binding`, `team`, `policy` and
`audit` manage access ([gxctl](docs/public/src/reference/gxctl.md),
[Access control](docs/public/src/operator-guide/access-control.md)).

## Your first VM

```
gxctl> image pull ubuntu-26.04
gxctl> credential-add                 # username, password and/or SSH keys
gxctl> create                         # pick a hypervisor, "image", the credential
gxctl> start my-vm
gxctl> connect my-vm                  # Ctrl+] detaches
gxctl> stop my-vm --graceful          # power button, then stop after 60 s
```

Or over the API, with a token (`gxctl token create`):

```bash
api() { curl --cacert /run/glidex-cp/tls.crt -H "Authorization: Bearer $GLIDEX_TOKEN" "$@"; }
api -X POST https://localhost:8841/images -H 'Content-Type: application/json' -d '{"catalog": "ubuntu-26.04"}'
api -X POST https://localhost:8841/vms -H 'Content-Type: application/json' -d '{
  "name": "my-vm", "vcpu_count": 2, "mem_size_mib": 2048,
  "hypervisor": "HYPERVISOR_QEMU", "image": "ubuntu-26.04", "root_disk_size_gib": 20,
  "credential": "alice", "networks": [{"network": "lab"}]
}'
api -X POST https://localhost:8841/vms/<id>/start
api -X POST 'https://localhost:8841/vms/<id>/stop?graceful_timeout_secs=60'
```

- Request bodies are proto JSON: values from a fixed list take their
  full names, so `hypervisor` is `HYPERVISOR_CLOUD_HYPERVISOR` (default)
  or `HYPERVISOR_QEMU` (`qemu` alone is refused). With an `image`
  or `root_disk`, the VM boots through UEFI firmware, a *firmware image*
  managed like the cloud images. The control plane imports the
  installer's pinned `CLOUDHV.fd` (`cloudhv-edk2`) and the host's OVMF
  (`ovmf`) once at its first start; otherwise pull them with
  `gxctl image pull --firmware cloudhv-edk2` / `--firmware ovmf`, or from
  the Images page. The newest one for the hypervisor
  is used unless you pass `firmware` (an image name or id); pass
  `kernel_image_path` for a direct kernel boot. Paths you pass are opened
  by the control plane, so under the units they must be readable by the
  `glidex` user (your home directory usually isn't).
- Without a stored credential, the guest's `cloud` user has no SSH keys
  and a locked password: nobody can log in, so add a credential.
  `cloud_init_path` replaces the generated seed.
- Give the root disk a size (`root_disk_size_gib`, or type one at
  `gxctl create`'s prompt): without one, it gets 10 GiB, or the image's
  size if that is larger.
- Networks: glidex creates none by itself. Add one first with
  `gxctl network-add` or the web UI's Networking page (e.g. a NAT network
  `lab`); a network named `default` is preselected in the VM form. Only
  your default project can use a new network until you grant it to others
  (`gxctl network grant`).

## A cluster

Every host gets glidex from the installer as above (with OVN, unless
`--no-ovn`). On the first host:

```bash
gxctl cluster init --force            # this host becomes a cluster of one (a server)
umask 077; gxctl cluster join-token --role agent > join.token   # shown once
```

Copy `join.token` to the next host over a secure channel (mode 0600), then
there:

```bash
cargo run -p glidex-install -- --join <first-host>:8842 --token-file join.token
```

The token is read from a file or stdin, never from the command line, and
used once. `--role server` adds a server (run three for a quorum), and
`--import` (with a token made with `--allow-import`) brings the host's
existing VMs and disks along once an administrator approves the plan. The web UI's Cluster page shows the
nodes, adds more (`gxctl cluster status`, `gxctl node …` from the CLI),
and turns OVN on for networks that span the nodes: its OVN settings
suggest the shared edge (physical network, gateway nodes, the LAN's
addresses) from what the nodes report. A project's routers are on its
project page. See [Clusters](docs/public/src/operator-guide/clusters.md).

## Tests

```bash
cargo test                                    # unit and API tests, no VMs

# Real VMs (KVM, the hypervisors, glidex-netd + OVS for the network tests):
GLIDEX_TEST_IMAGE=~/images/ubuntu-cloudimg.img \
  cargo test -p glidex-control-plane --test functional_tests -- --ignored --test-threads=1

# Web UI in headless Chromium, both hypervisors (-H ch / -H qemu for one):
scripts/dev/ui-e2e.sh -i ~/images/ubuntu-cloudimg.img

# A two-node cluster with OVN, nested in VMs of this host's glidex
# (nested KVM, ~9 GiB; GLIDEX_E2E_NETWORK names a node NAT network the
# host can reach, if there is no `default`):
cargo test -p glidex-e2e --test nested_cluster -- --ignored --nocapture
```

The boot, NAT, vhost-user and catalog tests run on both hypervisors
(`qemu_*` variants). The network tests need `glidex-netd`'s full access,
which only the `glidex` group has (the installer takes you out of it): run
them with it, e.g. `sudo -u $USER -g glidex env PATH=$PATH HOME=$HOME cargo test …`. Some need more setup: `GLIDEX_TEST_DPDK=1` with
OVS-DPDK for vhost-user, and a fake LAN (`sudo scripts/dev/fake-lan-setup.sh`,
`GLIDEX_TEST_LAN=1`) for the uplink tests. See [crates/glidex-ui/e2e/README.md](crates/glidex-ui/e2e/README.md)
for the UI suite.

## Layout

```
crates/
  glidex-control-plane/   server (REST API, VM state, hypervisor backends) and gxctl
  glidex-netd/            root helper for host networking
  glidex-ovs/             Open vSwitch, NAT, uplinks and VM ports
  glidex-ovn/             OVN northbound: cluster networks, the edge, routers
  glidex-e2e/             cluster end-to-end tests (nested VMs)
  glidex-install/         installer / uninstaller
  glidex-ui/ui/           web UI (Vite + React)
  glidex-ui/e2e/          web UI end-to-end tests (Playwright)
docs/                     user and operator docs (docs/public), book config
spec/                     design docs, start with spec/README.md
LICENSES/                 MIT and Apache-2.0 texts, COPYRIGHT
scripts/dev/              test helpers
```

## License

Copyright (c) 2025-2026 CMKL University and The Glidex Project Contributors.

Glidex is dual-licensed under either of

- the Apache License, Version 2.0 ([LICENSES/LICENSE-APACHE](LICENSES/LICENSE-APACHE)), or
- the MIT license ([LICENSES/LICENSE-MIT](LICENSES/LICENSE-MIT)),

at your option (SPDX: `MIT OR Apache-2.0`). See
[LICENSES/COPYRIGHT](LICENSES/COPYRIGHT) for who holds the copyright and
for vendored third-party code.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in Glidex by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

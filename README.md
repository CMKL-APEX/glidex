# Glidex

Glidex runs virtual machines on your own Linux hosts. A *virtual machine*
(VM) is a complete computer, with its own operating system, that runs as
a program on a real one. Glidex starts, stops and keeps track of your VMs
for you, on one host or on many hosts joined into a cluster.

It is for people who run their own servers (a lab, a team, a small
cloud) and want a simple way to hand out VMs: a web UI for everyday
work, the `gxctl` command-line tool for the terminal, and REST and RPC
APIs for scripts. Under the hood it uses KVM, with
[Cloud Hypervisor](https://www.cloudhypervisor.org/) (the default) or
[QEMU](https://www.qemu.org/). It is written in Rust.

The full documentation is at
<https://cmkl-apex.github.io/glidex/docs/>; its source is in
[docs/public](docs/public/src/SUMMARY.md).

## Features

**VMs**

- Cloud Hypervisor or QEMU, with the same features on both, including pause, resume and graceful shutdown ([Virtual machines](docs/public/src/user-guide/vms.md)).
- Boot through UEFI firmware, or boot a Linux kernel directly ([Kernel boot](docs/public/src/user-guide/kernel-boot.md)).
- A serial console in the browser or in `gxctl`, shared and logged to disk ([Console](docs/public/src/user-guide/console.md)).
- Guest logins (username, password, SSH keys) set up for you by cloud-init ([Guest credentials](docs/public/src/user-guide/credentials.md)).

**Images and storage**

- Checksum-verified cloud images (Ubuntu, Debian, Fedora) from a built-in catalog, or from any HTTPS URL ([Images and disks](docs/public/src/user-guide/images-and-disks.md)).
- A thin root disk per VM, plus data disks you attach to VMs.
- Datastores: the directories that hold disks and images, local to a node or shared by every node, with disks you can move between them ([Datastores](docs/public/src/operator-guide/datastores.md)).

**Networking**

- NAT, isolated and bridged networks on Open vSwitch ([Networks](docs/public/src/user-guide/networks.md), [Host networking](docs/public/src/operator-guide/host-networking.md)).
- Fast paths for bridged uplinks (kernel, AF_XDP, DPDK) and vhost-user VM ports.

**Hardware**

- PCI passthrough (VFIO) of devices such as GPUs and NICs, with device pools the scheduler picks from ([PCI passthrough](docs/public/src/operator-guide/pci-passthrough.md)).
- A view of each host's NUMA nodes, CPUs, memory and hugepages, and VMs on dedicated CPUs ([Host resources](docs/public/src/operator-guide/host-resources.md)).

**People and access**

- Projects with members, teams, roles and quotas, Cedar policies and an audit log ([Projects](docs/public/src/user-guide/projects.md), [Access control](docs/public/src/operator-guide/access-control.md)).
- Sign-in with local accounts, passkeys, OIDC single sign-on, Google or JumpCloud ([Authentication](docs/public/src/operator-guide/authentication.md#login-connectors), [Passkeys](docs/public/src/operator-guide/authentication.md#passkeys)).
- Users from your identity provider: SCIM provisioning, Google Workspace directory sync, and invitations ([SCIM](docs/public/src/operator-guide/authentication.md#provisioning-with-scim), [Invitations](docs/public/src/operator-guide/authentication.md#invitations)).
- HTTPS everywhere: a self-signed certificate by default, or your own, or a Tailscale one ([TLS](docs/public/src/operator-guide/tls.md)).
- Metered CPU, memory, disk and network use ([Usage](docs/public/src/user-guide/usage.md)).

**Clusters**

- Many hosts act as one: a replicated store, one-time join tokens over mutual TLS, a scheduler that explains where it put each VM, and nodes you can drain and rejoin ([Clusters](docs/public/src/operator-guide/clusters.md)).
- Networks that span every node with OVN, shared NAT gateways and per-project routers ([Cluster networks on OVN](docs/public/src/operator-guide/clusters.md#cluster-networks-on-ovn)).

## Quick start

This takes one Linux host from nothing to a running VM. It is the short
version of the [full quick start](docs/public/src/quick-start.md), which
explains each step.

### 1. Check the requirements

You need:

- Linux on x86_64 or aarch64 with KVM. Check that `/dev/kvm` exists:
  `ls -l /dev/kvm`.
- A package manager the installer knows: `apt-get`, `dnf`/`yum` or `pacman`.
- An account that can use `sudo`.
- `git`, and a Rust toolchain to run the installer. Get Rust from
  [rustup.rs](https://rustup.rs):

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

  Rust needs a C linker; on Debian or Ubuntu, `sudo apt install build-essential`.
- Internet access, to download packages, hypervisors and images.

### 2. Install

```bash
git clone https://github.com/CMKL-APEX/glidex.git && cd glidex
cargo run -p glidex-install
```

The installer asks for your sudo password. It installs the hypervisors,
Open vSwitch, the Glidex services and the web UI, and keeps Rust up to
date from then on. At the end it prints the web UI address and two
certificate fingerprints. Keep them for step 4.

Run the same command again later to update. See
[Installation](docs/public/src/operator-guide/installation.md) for its
options.

### 3. Log out and back in

The installer adds you to the `glidex-users` and `glidex-admin` groups.
They let you use `gxctl` and make you an administrator. Linux only
applies new groups to new logins, so log out and back in (or reconnect
your SSH session). Then check:

```bash
gxctl whoami
```

On the host, `gxctl` needs no login: it knows you by your Unix account.

### 4. Open the web UI

Open `https://host.example.org:5173` (your host's name or address). The
installer doesn't open firewall ports: to reach the UI from another
machine, allow TCP 5173 yourself.

The UI uses a self-signed certificate, so your browser shows a warning.
Before you accept it, compare the certificate's SHA-256 fingerprint in
the browser with the `web UI certificate SHA-256` line the installer
printed. Accept only if they match. See
[TLS](docs/public/src/operator-guide/tls.md) to use your own certificate
or a Tailscale one instead.

Sign in with your Unix username and password.

### 5. Create your first VM

Each step shows the `gxctl` command, then the same thing in the web UI.

1. **Create a NAT network.** A *NAT network* is a private network for
   your VMs: they get an address from Glidex and reach the internet
   through the host. Glidex creates none by itself. Name it `default`
   and VM forms pick it for you.

   ```bash
   gxctl network-add default
   ```

   Web UI: **Networking** → **Add Network**, name `default`, **Create**.

2. **Pull a cloud image.** A *cloud image* is a ready-made disk with an
   operating system, published by a Linux distribution for VMs like
   these.

   ```bash
   gxctl image catalog            # what you can pull
   gxctl image pull ubuntu-26.04
   ```

   Web UI: **Images** → **Cloud image catalog** → **Pull**.

3. **Add a credential.** A *credential* is the username, password
   and/or SSH keys you log in to the VM with. Without one, nobody can
   log in. Give it a password to log in on the console.

   ```bash
   gxctl credential-add           # asks for a username, password and SSH keys
   ```

   Web UI: **Credentials** → **Add Credential**.

4. **Create and start a VM.** `gxctl create` asks its questions one by
   one; press Enter to take the default. Give it a name such as
   `my-vm`, 2 vCPUs, 2048 MiB of memory and a 20 GiB root disk, and
   keep the defaults for the rest (the image, your credential and the
   `default` network).

   ```bash
   gxctl create
   gxctl start my-vm
   gxctl list
   ```

   Web UI: **VMs** → **+ Create VM**, fill in the form, check **Start it
   now**, **Create VM**.

5. **Open the console.** Wait for the first boot to finish, then log in
   with your credential. Press `Ctrl+]` to detach; the VM keeps running.

   ```bash
   gxctl connect my-vm
   ```

   Web UI: **Open Console** on the VM's card.

6. **Stop it.** `--graceful` presses the VM's power button and waits up
   to 60 seconds before it stops the VM.

   ```bash
   gxctl stop my-vm --graceful
   ```

   Web UI: **Shut down** on the VM's card.

`gxctl` with no arguments opens an interactive shell with Tab
completion; type `help` there. `gxctl delete my-vm` removes the VM and
its root disk.

### The same over the API

From another machine, use the API on port 8841 with a token
(`gxctl token create <name>` prints one) and the API's certificate
(`/run/glidex-cp/tls.crt` on the host). Values from a fixed list take
their full names, such as `HYPERVISOR_QEMU`:

```bash
curl --cacert tls.crt -H "Authorization: Bearer <token>" \
  -X POST https://host.example.org:8841/vms \
  -H 'Content-Type: application/json' -d '{
  "name": "my-vm", "vcpu_count": 2, "mem_size_mib": 2048,
  "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR", "image": "ubuntu-26.04",
  "root_disk_size_gib": 20, "credential": "<credential>",
  "networks": [{"network": "default"}]
}'
```

See the [REST API](docs/public/src/api/rest.md) for more.

## Next steps

- [Concepts](docs/public/src/concepts.md): how projects, images, disks,
  networks and VMs fit together.
- [User guide](docs/public/src/user-guide/README.md): VMs, images and
  disks, credentials, networks, projects, the console and usage.
  [Logging in](docs/public/src/user-guide/logging-in.md) covers `gxctl`
  from another machine.
- [Operator guide](docs/public/src/operator-guide/README.md): installing
  and running hosts, including
  [clusters](docs/public/src/operator-guide/clusters.md),
  [networking](docs/public/src/operator-guide/host-networking.md) and
  [authentication](docs/public/src/operator-guide/authentication.md).
- [gxctl reference](docs/public/src/reference/gxctl.md): every command.
- [API reference](docs/public/src/api/README.md): the REST and RPC APIs.

To read the docs locally: `mdbook serve docs/public`.

## Development

### Running from source

The installer runs Glidex as systemd services. To work on it, you can
run the pieces yourself instead. Stop the services first
(`sudo systemctl stop glidex-control-plane glidex-ui`), since they use
the same ports.

```bash
cargo run --bin glidex-control-plane   # API on https://localhost:8841
cargo run -p glidex-ui -- --dev        # web UI with hot reload on https://localhost:5173
cargo run --bin gxctl                  # CLI
```

A control plane you start yourself keeps its data in `~/.glidex` and its
socket in `$XDG_RUNTIME_DIR/glidex`, where `gxctl` finds it.

### Tests

```bash
cargo test                                    # unit and API tests, no VMs

# Real VMs (KVM, the hypervisors, glidex-netd + OVS for the network tests):
GLIDEX_TEST_IMAGE=~/images/ubuntu-cloudimg.img \
  cargo test -p glidex-control-plane --test functional_tests -- --ignored --test-threads=1

# Web UI in headless Chromium, both hypervisors (-H ch / -H qemu for one):
scripts/dev/ui-e2e.sh -i ~/images/ubuntu-cloudimg.img

# A two-node cluster with OVN, nested in VMs of this host's glidex
# (nested KVM, ~9 GiB of memory, 10-20 minutes):
cargo test -p glidex-e2e --test nested_cluster -- --ignored --nocapture
```

The network tests need the `glidex` group, which the installer takes you
out of: run them with
`sudo -u $USER -g glidex env PATH=$PATH HOME=$HOME cargo test …`. The
vhost-user tests need `GLIDEX_TEST_DPDK=1` and OVS-DPDK; the uplink tests
need a fake LAN (`sudo scripts/dev/fake-lan-setup.sh`,
`GLIDEX_TEST_LAN=1`). See
[crates/glidex-ui/e2e/README.md](crates/glidex-ui/e2e/README.md) for the
UI suite.

### Repository layout

```
crates/
  glidex-control-plane/   the server (API, VM state, scheduler) and gxctl
  glidex-api/             protos, generated RPC messages and clients
  glidex-vm-shim/         per-VM supervisor: runs the hypervisor, owns its console
  glidex-hv-client/       clients for Cloud Hypervisor's API and QEMU's QMP
  glidex-netd/            root helper for host networking
  glidex-ovs/             Open vSwitch, NAT, uplinks and VM ports
  glidex-ovn/             OVN: cluster networks, the edge, routers
  glidex-authd/           root helper that checks local passwords (PAM)
  glidex-tls/             TLS certificates
  glidex-raftstore/       replicated ReDB store on Raft (shared with hyperstor)
  glidex-auth/            sessions, tokens, Cedar engine, OIDC login (shared with hyperstor)
  glidex-install/         installer and uninstaller
  glidex-ui/ui/           web UI (Vite + React)
  glidex-ui/e2e/          web UI end-to-end tests (Playwright)
  glidex-e2e/             cluster end-to-end tests (nested VMs)
docs/                     user and operator docs (docs/public)
spec/                     design docs
LICENSES/                 MIT and Apache-2.0 texts, COPYRIGHT
scripts/dev/              test helpers
```

### Contributing

The design specs are in [spec/](spec/README.md); start with its README.
[docs/README.md](docs/README.md) explains how they differ from the user
docs.

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

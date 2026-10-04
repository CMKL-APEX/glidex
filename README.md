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

## Install

```bash
git clone https://github.com/CMKL-APEX/glidex.git && cd glidex
cargo run -p glidex-install                   # install, or update everything
cargo run -p glidex-install -- uninstall      # --dry-run shows what would go
```

The installer asks nothing but your sudo password. It installs what is
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
to update. `--no-qemu`, `--no-networking`, `--no-services` and
`--ovs-profile kernel` opt out (remembered for later runs; `--help` for
all). It needs Linux with KVM (`/dev/kvm`). See
[spec/installer.md](spec/installer.md).

## Run

With the systemd units, glidex already runs: the web UI is on
http://localhost:5173 and the API on http://localhost:8841. To run it
yourself instead:

```bash
cargo run --bin glidex-control-plane   # API on http://localhost:8841
cargo run -p glidex-ui -- --dev        # web UI (Vite, hot reload) on http://localhost:5173
cargo run --bin gxctl                  # CLI
```

The API listens on loopback and on `api.sock` in its run directory
(`/run/glidex-cp/api.sock` under systemd, for members of
`glidex-users`); every request is authenticated, and a non-loopback
listener (`GLIDEX_LISTEN`, or `listen` in `/etc/glidex/control-plane.json`)
needs TLS. See [spec/security.md](spec/security.md).

`gxctl` talks to the control plane's Unix socket (`/run/glidex-cp/api.sock`,
or the one a hand-started control plane creates under `$XDG_RUNTIME_DIR/glidex`)
and you're identified by your Unix account: no login. From another
machine, `gxctl --url https://host:8841 login --oidc` (or `login --token`)
saves a token in `~/.config/glidex/token`. `--project <p>` picks the
project; `whoami`, `project`, `token`, `binding`, `team`, `policy` and
`audit` manage access ([spec/cli.md](spec/cli.md),
[spec/security.md](spec/security.md)).

## Your first VM

```
gxctl> image pull ubuntu-26.04
gxctl> credential-add                 # username, password and/or SSH keys
gxctl> create                         # pick a hypervisor, "image", the credential
gxctl> start my-vm
gxctl> connect my-vm                  # Ctrl+] detaches
gxctl> stop my-vm --graceful          # power button, then stop after 60 s
```

Or over the API:

```bash
curl -X POST localhost:8841/images -H 'Content-Type: application/json' -d '{"catalog": "ubuntu-26.04"}'
curl -X POST localhost:8841/vms -H 'Content-Type: application/json' -d '{
  "name": "my-vm", "vcpu_count": 2, "mem_size_mib": 2048,
  "hypervisor": "qemu", "image": "ubuntu-26.04", "root_disk_size_gib": 20,
  "credential": "alice", "networks": [{"network": "default"}]
}'
curl -X POST localhost:8841/vms/<id>/start
curl -X POST 'localhost:8841/vms/<id>/stop?graceful_timeout_secs=60'
```

- `hypervisor` is `cloudhypervisor` (default) or `qemu`. With an `image`
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
- Without a stored credential, the guest user is `cloud` with the
  `~/.ssh/*.pub` keys of the user the control plane runs as (none for the
  `glidex` service user, so add a credential). `cloud_init_path` replaces the generated seed.
- Networks: `default` (NAT) is created when glidex-netd is available; add
  more with `gxctl network-add` or the web UI's Networking page.

More in [spec/cli.md](spec/cli.md), [spec/rest-api.md](spec/rest-api.md),
[spec/images.md](spec/images.md), [spec/credentials.md](spec/credentials.md)
and [spec/networking.md](spec/networking.md).

## Tests

```bash
cargo test                                    # unit and API tests, no VMs

# Real VMs (KVM, the hypervisors, glidex-netd + OVS for the network tests):
GLIDEX_TEST_IMAGE=~/images/ubuntu-cloudimg.img \
  cargo test -p glidex-control-plane --test functional_tests -- --ignored --test-threads=1

# Web UI in headless Chromium, both hypervisors (-H ch / -H qemu for one):
scripts/dev/ui-e2e.sh -i ~/images/ubuntu-cloudimg.img
```

The boot, NAT, vhost-user and catalog tests run on both hypervisors
(`qemu_*` variants). Some need more setup: `GLIDEX_TEST_DPDK=1` with
OVS-DPDK for vhost-user, and a fake LAN (`sudo scripts/dev/fake-lan-setup.sh`,
`GLIDEX_TEST_LAN=1`) for the uplink tests. See [crates/glidex-ui/e2e/README.md](crates/glidex-ui/e2e/README.md)
for the UI suite.

## Layout

```
crates/
  glidex-control-plane/   server (REST API, VM state, hypervisor backends) and gxctl
  glidex-netd/            root helper for host networking
  glidex-ovs/             Open vSwitch, NAT, uplinks and VM ports
  glidex-install/         installer / uninstaller
  glidex-ui/ui/           web UI (Vite + React)
  glidex-ui/e2e/          web UI end-to-end tests (Playwright)
spec/                     design docs, start with spec/README.md
scripts/dev/              test helpers
```

## License

MIT

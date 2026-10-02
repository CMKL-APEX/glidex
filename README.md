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
cargo run -p glidex-install                   # Rust, Bun, Cloud Hypervisor, firmware, build
cargo run -p glidex-install -- uninstall      # --dry-run shows what would go
```

The installer asks before each optional step: QEMU with OVMF, VM networking
(Open vSwitch, `glidex-netd`, the `glidex` group), and systemd units. It
needs Linux with KVM (`/dev/kvm`; add yourself to the `kvm` group). See
[spec/installer.md](spec/installer.md).

## Run

With the systemd units enabled, glidex already runs. Otherwise:

```bash
cargo run --bin glidex-control-plane   # API on http://localhost:8841
cargo run -p glidex-ui                 # web UI on http://localhost:5173
cargo run --bin gxctl                  # CLI
```

The API has no authentication and listens on loopback only
(`GLIDEX_LISTEN` changes that): only expose it to a trusted network.

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
  or `root_disk`, the VM boots through that hypervisor's default UEFI
  firmware (`~/.glidex/CLOUDHV.fd`, or OVMF from the `ovmf` /
  `edk2-ovmf` package); pass `firmware_path` to choose one, or
  `kernel_image_path` for a direct kernel boot.
- Without a stored credential, the guest user is `cloud` with the host's
  `~/.ssh/*.pub` keys. `cloud_init_path` replaces the generated seed.
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

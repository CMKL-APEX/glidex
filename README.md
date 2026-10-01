# Glidex

A Rust-based control plane for managing KVM virtual machines with [Cloud-Hypervisor](https://www.cloudhypervisor.org/) (default) and [QEMU](https://www.qemu.org/).

```
   _____ _ _     _
  / ____| (_)   | |
 | |  __| |_  __| | _____  __
 | | |_ | | |/ _` |/ _ \ \/ /
 | |__| | | | (_| |  __/>  <
  \_____|_|_|\__,_|\___/_/\_\
```

## Features

- **Multi-hypervisor support** - Control Cloud-Hypervisor and QEMU VMs through a unified interface
- **Cloud image boot** - Boot stock distro cloud images via UEFI firmware with an auto-generated cloud-init seed
- **Images and disks** - Pull verified cloud images (Ubuntu, Debian, Fedora, AlmaLinux) from a built-in catalog; create, grow, shrink and delete disks, with the root partition extended automatically
- **Credential store** - Stored guest logins (password hash + SSH keys) provisioned by cloud-init
- **VM networking** - Open vSwitch bridges via a root helper (`glidex-netd`): NAT networks with DHCP, bridged uplinks (kernel, AF_XDP, DPDK) with safe IP migration, tap and vhost-user VM ports
- **REST API** for VM lifecycle management (create, start, stop, pause, delete)
- **Web UI** - Vite + React web interface for VM management
- **Interactive CLI** (`gxctl`) with command history and Tab completion of commands and file paths
- **Interactive console** - connect to VM serial console with full I/O support
- **Console logging** - persistent logs of all VM console output
- **Multi-client support** - multiple CLI sessions can connect to the same VM console

## Quick Start

### Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/glidex.git
cd glidex

# Run the installer (installs Rust, Bun, hypervisors, builds the project)
cargo run -p glidex-install
```

The installer will:
1. Install Rust via rustup (if not present)
2. Install Bun for the UI dev server (if not present)
3. Install Cloud-Hypervisor (default hypervisor)
4. Download Cloud-Hypervisor's UEFI firmware (`CLOUDHV.fd`) to `~/.glidex/`
   for booting distro cloud images, plus `dosfstools`/`mtools`, and the disk
   tools for images and disks (`qemu-img`, `sgdisk`, `growpart`)
5. Optionally install QEMU
6. Check KVM access
7. Build the Glidex binaries
8. Optionally set up VM networking (Open vSwitch, `glidex-netd`, `glidex` group)
   and the host settings it needs: `net.ipv4.ip_forward=1`, and for OVS-DPDK
   hugepages, `vfio-pci` and DPDK init (persisted in `/etc/sysctl.d/90-glidex.conf`
   and `/etc/modules-load.d/glidex.conf`)
9. Optionally start glidex at boot (systemd units for `glidex-netd` and the control plane)
10. Install UI npm dependencies (`bun install`)

### Uninstall

```bash
cargo run -p glidex-install -- uninstall --dry-run   # show what would be removed
cargo run -p glidex-install -- uninstall             # stop services, restore host networking, remove glidex
```

Open vSwitch, DPDK settings, `cloud-hypervisor` and `~/.glidex` are kept
unless you add `--remove-ovs`, `--reset-dpdk`, `--remove-cloud-hypervisor`
or `--purge-user-data`. Sysctls the installer changed go back to their previous
values (hugepages stay while OVS keeps its DPDK settings).

### Manual Installation

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build the project
cargo build --release

# Binaries are in target/release/
#   - glidex-control-plane (server)
#   - gxctl (CLI)
```

### Running

If you enabled the systemd units, glidex already runs at boot
(`systemctl status glidex-netd glidex-control-plane`; logs with
`journalctl -u glidex-control-plane`). Otherwise:

1. **Start the control plane server:**

```bash
cargo run --bin glidex-control-plane
```

The server listens on `http://localhost:8841` by default.

2. **Option A: Start the Web UI:**

```bash
cargo run -p glidex-ui
```

Open http://localhost:5173 in your browser.

3. **Option B: Start the CLI:**

```bash
cargo run --bin gxctl
```

4. **Create and start a VM (via CLI):**

```
gxctl> create
VM name: my-vm
vCPU count [1]: 2
Memory (MiB) [512]: 2048
Hypervisor [cloudhypervisor/qemu] (default: cloudhypervisor):
UEFI firmware path ['none' for kernel boot] [/home/user/.glidex/CLOUDHV.fd]:
Boot disk from [image/disk/path] [image]:
Image (ubuntu-26.04) [ubuntu-26.04]:
Root disk size in GiB [10]: 20
Data disks (optional, comma-separated disk names):
cloud-init seed image (optional, default: auto-generated):
VFIO PCI devices (comma-separated, e.g. /sys/bus/pci/devices/0000:41:00.0):

gxctl> start my-vm
```

4. **Connect to the VM console:**

```
gxctl> connect my-vm
```

Press `Ctrl+]` to detach from the console.

5. **View console logs:**

```
gxctl> log my-vm
```

## CLI Commands

| Command | Description |
|---------|-------------|
| `list` | List all VMs |
| `get <name\|id>` | Show VM details |
| `create` | Create a new VM (interactive) |
| `start <name\|id>` | Start a VM |
| `stop <name\|id>` | Stop a VM |
| `pause <name\|id>` | Pause a running VM |
| `connect <name\|id>` | Connect to VM console (interactive) |
| `log <name\|id>` | Show VM serial console log |
| `delete <name\|id>` | Delete a VM |
| `health` | Check API server health |
| `help` | Show available commands |
| `exit` | Exit the CLI |

## REST API

### Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/health` | Health check |
| `GET` | `/vms` | List all VMs |
| `POST` | `/vms` | Create a new VM |
| `GET` | `/vms/{id}` | Get VM details |
| `DELETE` | `/vms/{id}` | Delete a VM |
| `POST` | `/vms/{id}/start` | Start a VM |
| `POST` | `/vms/{id}/stop` | Stop a VM |
| `POST` | `/vms/{id}/pause` | Pause a VM |
| `GET` | `/vms/{id}/console` | Get console connection info |

### Create VM Request

```json
{
  "name": "my-vm",
  "vcpu_count": 2,
  "mem_size_mib": 1024,
  "firmware_path": "~/.glidex/CLOUDHV.fd",
  "rootfs_path": "~/images/ubuntu-cloudimg.raw",
  "hypervisor": "cloudhypervisor"
}
```

For kernel boot, pass `kernel_image_path` (and optionally `kernel_args`)
instead of `firmware_path`.

The `hypervisor` field is optional and defaults to `"cloudhypervisor"`. Supported values:
- `"cloudhypervisor"` - Use Cloud-Hypervisor (default)
- `"qemu"` - Use QEMU (requires `qemu-system-x86_64`, kernel boot only)

### Example: Create and Start a VM with curl

```bash
# Create a VM
curl -X POST http://localhost:8841/vms \
  -H "Content-Type: application/json" \
  -d '{
    "name": "test-vm",
    "vcpu_count": 2,
    "mem_size_mib": 512,
    "firmware_path": "/home/user/.glidex/CLOUDHV.fd",
    "rootfs_path": "/home/user/images/ubuntu-cloudimg.raw"
  }'

# Start the VM
curl -X POST http://localhost:8841/vms/{vm-id}/start

# List VMs
curl http://localhost:8841/vms
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                   glidex-ui (Web UI)                        │
│  - Vite + React + TypeScript                                │
│  - Dashboard, VM management                                 │
│  - Real-time status updates                                 │
└─────────────────┬───────────────────────────────────────────┘
                  │ HTTP (REST API)
┌─────────────────┼───────────────────────────────────────────┐
│                 │        gxctl (CLI)                        │
│                 │  - Interactive shell                      │
│                 │  - Connects to console Unix socket        │
└─────────────────┼───────────────────────────────────────────┘
                  │ HTTP / Unix Socket
┌─────────────────▼───────────────────────────────────────────┐
│              glidex-control-plane (Server)                  │
│  - REST API (Axum)                                          │
│  - VM state management                                      │
│  - Hypervisor abstraction layer                             │
│  - Console proxy (PTY ↔ Unix socket)                        │
│  - Console logging                                          │
│  - Persistence (ReDB)                                       │
└─────────────────┬───────────────────────────────────────────┘
                  │ Unix Socket (Hypervisor API / QMP)
             ┌────┴────┐
             ▼         ▼
     ┌────────────┐ ┌────────┐
     │Cloud-Hypvsr│ │  QEMU  │
     │     VM     │ │   VM   │
     │ KVM-based  │ │  KVM   │
     └────────────┘ └────────┘
```

### Hypervisor Abstraction

The control plane uses a trait-based abstraction to support multiple hypervisors:

```
hypervisor/
├── mod.rs              # Hypervisor and HypervisorProcess traits
├── cloud_hypervisor.rs # Cloud-Hypervisor implementation
└── qemu.rs             # QEMU implementation (QMP over Unix socket)
```

Both hypervisors implement the same interface:
- `configure()` - Configure VM with CPU, memory, kernel, and disk
- `start()` - Boot the VM
- `pause()` - Pause the VM
- `resume()` - Resume a paused VM
- `kill()` - Terminate the VM process

### Console Architecture

For each running VM:
- A PTY (pseudo-terminal) carries the guest console. QEMU's serial port is
  attached to a PTY glidex creates; Cloud-Hypervisor allocates its own PTY
  (virtio console for kernel boot, serial port for firmware boot)
- A background thread reads from the PTY and:
  - Writes all output to a log file (`/tmp/{hypervisor}-{id}.log`)
  - Broadcasts to connected clients via Unix socket (`/tmp/{hypervisor}-{id}.console.sock`)
- Multiple clients can connect simultaneously

## Requirements

- **Linux** (all supported hypervisors are Linux-only)
- **KVM** enabled (`/dev/kvm` accessible)
- **Rust 1.85+** (for building)
- **Bun** (for the Vite + React dev server)
- **Cloud-Hypervisor 50.0+** (default hypervisor)
- **dosfstools** and **mtools** (to build cloud-init seed images)
- **QEMU** (`qemu-system-x86_64`, optional)

### Enabling KVM

```bash
# Check if KVM is available
ls -la /dev/kvm

# Add your user to the kvm group
sudo usermod -aG kvm $USER

# Log out and back in for the change to take effect
```

## Project Structure

```
glidex/
├── Cargo.toml                    # Workspace root
├── README.md
└── crates/
    ├── glidex-control-plane/     # Control plane server
    │   ├── src/
    │   │   ├── main.rs           # Server entry point
    │   │   ├── api.rs            # REST API routes and handlers
    │   │   ├── models.rs         # Data structures (VM, VmConfig, etc.)
    │   │   ├── state.rs          # VM state management
    │   │   ├── persistence.rs    # ReDB-based persistence
    │   │   ├── cloud_init.rs     # cloud-init NoCloud seed image builder
    │   │   ├── credentials.rs    # Credential store (guest logins, hashed)
    │   │   ├── hypervisor/       # Hypervisor abstraction layer
    │   │   │   ├── mod.rs        # Traits and HypervisorType enum
    │   │   │   ├── cloud_hypervisor.rs # Cloud-Hypervisor backend
    │   │   │   └── qemu.rs       # QEMU backend (QMP)
    │   │   └── bin/
    │   │       └── gxctl.rs      # CLI client
    │   └── tests/
    │       ├── api_tests.rs      # API integration tests
    │       ├── credential_tests.rs # Credential store API tests
    │       └── functional_tests.rs # Firmware boot / cloud-init tests
    ├── glidex-install/           # Installer (cargo run -p glidex-install)
    │   └── src/main.rs
    └── glidex-ui/                # Web UI (Vite + React)
        ├── src/main.rs           # Dev server launcher (bun run dev)
        └── ui/                   # Vite + React app
            ├── package.json
            ├── vite.config.ts
            └── src/
```

## Development

### Running Tests

```bash
cargo test
```

### Building in Debug Mode

```bash
cargo build
```

### Building for Release

```bash
cargo build --release
```

## Booting a Cloud Image

The installer downloads Cloud-Hypervisor's UEFI firmware to
`~/.glidex/CLOUDHV.fd`. With it, distro cloud images boot as is. Let glidex
fetch one: it downloads the vendor's current build and checks it against the
checksum the vendor publishes.

```
gxctl> image catalog
  ubuntu-26.04   Ubuntu 26.04 LTS (Resolute Raccoon)
  ...
gxctl> image pull ubuntu-26.04
gxctl> create            # answer "image" for the boot disk
```

or over the API: `POST /images {"catalog": "ubuntu-26.04"}`, then
`POST /vms {..., "image": "ubuntu-26.04", "root_disk_size_gib": 20}`. Each
such VM gets its own thin (linked) root disk, with the root partition
already extended to the requested size. It is deleted with the VM unless
you pass `?keep_disk=true`. Disks can be created on their own too, attached
to VMs as data disks, and resized while the VM is stopped:

```
gxctl> disk create scratch --size-gib 50
gxctl> disk resize my-vm-root 40        # grows the root partition too
gxctl> disk list
```

Shrinking only gives back unpartitioned space at the end of a disk; glidex
never shrinks filesystems. Images and disks live in `~/.glidex/images` and
`~/.glidex/disks`; see [spec/images.md](spec/images.md). A cloud image you
downloaded yourself also still works as a plain `rootfs_path` (raw or
qcow2).

On each start glidex generates a cloud-init seed that sets the hostname to
the VM name and creates a sudo user. The login comes from one of:

- **A stored credential** (recommended). Add one in the web UI
  (**Credentials** page) or with `gxctl credential-add`, then pick it when
  creating the VM. glidex keeps only a SHA-512-crypt hash of the password,
  never the password itself, and never returns the hash over the API.
  cloud-init applies it on the VM's first boot; later password changes
  don't affect guests that are already provisioned.
- **The host fallback**, when no credential is chosen: user `cloud` with the
  public keys in `~/.ssh/*.pub` of the user running the control plane, and a
  console password only if `GLIDEX_CLOUD_INIT_PASSWD_HASH` holds a crypt hash
  (e.g. `openssl passwd -6`). Without it, password login stays locked.

```
gxctl> credential-add
Username: alice
Leave the password empty for SSH-key-only login.
Password:
Confirm password:
SSH public key files (optional, comma-separated, e.g. ~/.ssh/id_ed25519.pub): ~/.ssh/id_ed25519.pub
Credential created: alice (password: set, SSH keys: 1)
```

The control-plane API has no authentication, so run it on a trusted host or
network: anyone who can reach port 8841 can manage credentials and VMs.

Pass your own seed image with `cloud_init_path` to override this.

### Running the VM tests

The `--ignored` functional tests boot real VMs:

```bash
GLIDEX_TEST_IMAGE=~/images/ubuntu-cloudimg.raw \
  cargo test -p glidex-control-plane --test functional_tests -- --ignored --test-threads=1
```

- `firmware_boot_with_generated_cloud_init` needs KVM and cloud-hypervisor.
- `catalog_image_boots_and_root_grows` downloads `ubuntu-26.04` (or
  `GLIDEX_TEST_CATALOG`) from the vendor, boots it from a managed 12 GiB
  disk, grows the disk to 16 GiB and checks the guest sees both; it needs
  network access and the disk tools, but not `GLIDEX_TEST_IMAGE`:
  `cargo test -p glidex-control-plane --test functional_tests catalog_image_boots_and_root_grows -- --ignored`
- `nat_network_e2e` also needs a running glidex-netd (the installer's VM
  networking step) and membership in the `glidex` group.
- `vhost_user_e2e` also needs OVS-DPDK initialized (dpdk profile); set
  `GLIDEX_TEST_DPDK=1`.
- `bridged_uplink_e2e` and `afxdp_uplink_e2e` need a fake LAN of veth pairs
  (no real NICs are touched): run `sudo scripts/dev/fake-lan-setup.sh`, set
  `GLIDEX_TEST_LAN=1`, and afterwards `sudo scripts/dev/fake-lan-teardown.sh`.

## License

MIT

## Acknowledgments

- [Cloud-Hypervisor](https://www.cloudhypervisor.org/) - microVM hypervisor
- [QEMU](https://www.qemu.org/) - machine emulator and virtualizer
- [Axum](https://github.com/tokio-rs/axum) - Web framework
- [Vite](https://vitejs.dev/) + [React](https://react.dev/) - Web UI stack
- [Tokio](https://tokio.rs/) - Async runtime
- [Tailwind CSS](https://tailwindcss.com/) - Utility-first CSS framework

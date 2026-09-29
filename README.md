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
- **Credential store** - Stored guest logins (password hash + SSH keys) provisioned by cloud-init
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
   for booting distro cloud images, plus `dosfstools`/`mtools`
5. Optionally install QEMU
6. Check KVM access
7. Build the Glidex binaries
8. Install UI npm dependencies (`bun install`)

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

1. **Start the control plane server:**

```bash
cargo run --bin glidex-control-plane
```

The server listens on `http://localhost:8080` by default.

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
Disk image path (UEFI-bootable, e.g. a raw cloud image): ~/images/ubuntu-cloudimg.raw
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
curl -X POST http://localhost:8080/vms \
  -H "Content-Type: application/json" \
  -d '{
    "name": "test-vm",
    "vcpu_count": 2,
    "mem_size_mib": 512,
    "firmware_path": "/home/user/.glidex/CLOUDHV.fd",
    "rootfs_path": "/home/user/images/ubuntu-cloudimg.raw"
  }'

# Start the VM
curl -X POST http://localhost:8080/vms/{vm-id}/start

# List VMs
curl http://localhost:8080/vms
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
`~/.glidex/CLOUDHV.fd`. With it, any distro cloud image boots; convert it
to raw first:

```bash
wget https://cloud-images.ubuntu.com/resolute/current/resolute-server-cloudimg-amd64.img
qemu-img convert -p -f qcow2 -O raw resolute-server-cloudimg-amd64.img ubuntu-cloudimg.raw
```

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
network: anyone who can reach port 8080 can manage credentials and VMs.

Pass your own seed image with `cloud_init_path` to override this.

### Running the boot test

```bash
GLIDEX_TEST_IMAGE=~/images/ubuntu-cloudimg.raw \
  cargo test -p glidex-control-plane --test functional_tests -- --ignored
```

## License

MIT

## Acknowledgments

- [Cloud-Hypervisor](https://www.cloudhypervisor.org/) - microVM hypervisor
- [QEMU](https://www.qemu.org/) - machine emulator and virtualizer
- [Axum](https://github.com/tokio-rs/axum) - Web framework
- [Vite](https://vitejs.dev/) + [React](https://react.dev/) - Web UI stack
- [Tokio](https://tokio.rs/) - Async runtime
- [Tailwind CSS](https://tailwindcss.com/) - Utility-first CSS framework

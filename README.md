# Glidex

A Rust-based control plane for managing microVMs with support for multiple hypervisors including [Firecracker](https://firecracker-microvm.github.io/), [Cloud-Hypervisor](https://www.cloudhypervisor.org/), and [QEMU](https://www.qemu.org/).

```
   _____ _ _     _
  / ____| (_)   | |
 | |  __| |_  __| | _____  __
 | | |_ | | |/ _` |/ _ \ \/ /
 | |__| | | | (_| |  __/>  <
  \_____|_|_|\__,_|\___/_/\_\
```

## Features

- **Multi-hypervisor support** - Control Firecracker, Cloud-Hypervisor, and QEMU VMs through a unified interface
- **REST API** for VM lifecycle management (create, start, stop, pause, delete)
- **Web UI** - Vite + React web interface for VM management
- **Interactive CLI** (`gxctl`) with command history and tab completion
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
5. Optionally install Firecracker and QEMU
6. Check KVM access
7. Build the Glidex binaries
8. Install UI npm dependencies (`bun install`)
9. Optionally download sample kernel and rootfs images to `~/.glidex/`

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
Memory (MiB) [512]: 1024
Kernel image path [~/.glidex/vmlinux.bin]:
Root filesystem path [~/.glidex/rootfs.ext4]:
Kernel arguments (optional):
Hypervisor [firecracker/cloudhypervisor/qemu] (default: qemu):

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
  "kernel_image_path": "/path/to/vmlinux.bin",
  "rootfs_path": "/path/to/rootfs.ext4",
  "kernel_args": "console=ttyS0 reboot=k panic=1 pci=off",
  "hypervisor": "cloudhypervisor"
}
```

The `hypervisor` field is optional and defaults to `"qemu"`. Supported values:
- `"qemu"` - Use QEMU (default, requires `qemu-system-x86_64`)
- `"cloudhypervisor"` - Use Cloud-Hypervisor
- `"firecracker"` - Use Firecracker hypervisor

### Example: Create and Start a VM with curl

```bash
# Create a VM
curl -X POST http://localhost:8080/vms \
  -H "Content-Type: application/json" \
  -d '{
    "name": "test-vm",
    "vcpu_count": 2,
    "mem_size_mib": 512,
    "kernel_image_path": "/home/user/.glidex/vmlinux.bin",
    "rootfs_path": "/home/user/.glidex/rootfs.ext4"
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
        ┌─────────┼─────────┐
        ▼         ▼         ▼
┌────────────┐ ┌────────────┐ ┌────────┐
│ Firecracker│ │Cloud-Hypvsr│ │  QEMU  │
│   microVM  │ │   microVM  │ │  VM    │
│ KVM-based  │ │ KVM-based  │ │  KVM   │
└────────────┘ └────────────┘ └────────┘
```

### Hypervisor Abstraction

The control plane uses a trait-based abstraction to support multiple hypervisors:

```
hypervisor/
├── mod.rs              # Hypervisor and HypervisorProcess traits
├── firecracker.rs      # Firecracker implementation
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

For each running VM (Firecracker):
- A PTY (pseudo-terminal) pair is created
- The hypervisor's stdin/stdout/stderr connect to the PTY slave
- A background thread reads from the PTY master and:
  - Writes all output to a log file (`/tmp/{hypervisor}-{id}.log`)
  - Broadcasts to connected clients via Unix socket (`/tmp/{hypervisor}-{id}.console.sock`)
- Multiple clients can connect simultaneously

For Cloud-Hypervisor, console output is captured via the `--console file` option.

## Requirements

- **Linux** (all supported hypervisors are Linux-only)
- **KVM** enabled (`/dev/kvm` accessible)
- **Rust 1.85+** (for building)
- **Bun** (for the Vite + React dev server)
- **Cloud-Hypervisor 50.0+** (default hypervisor)
- **Firecracker 1.14.0+** (optional)
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
    │   │   ├── hypervisor/       # Hypervisor abstraction layer
    │   │   │   ├── mod.rs        # Traits and HypervisorType enum
    │   │   │   ├── firecracker.rs    # Firecracker backend
    │   │   │   ├── cloud_hypervisor.rs # Cloud-Hypervisor backend
    │   │   │   └── qemu.rs       # QEMU backend (QMP)
    │   │   └── bin/
    │   │       └── gxctl.rs      # CLI client
    │   └── tests/
    │       └── api_tests.rs      # API integration tests
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

## Sample Kernel and RootFS

The installer can download official Firecracker CI images:

- **Kernel**: Linux kernel compiled for Firecracker
- **RootFS**: Ubuntu-based root filesystem (ext4)
- **SSH Key**: Generated key pair for VM access

Files are stored in `~/.glidex/`:
```
~/.glidex/
├── vmlinux.bin    # Linux kernel
├── rootfs.ext4    # Ubuntu root filesystem
├── vm_key         # SSH private key
└── vm_key.pub     # SSH public key
```

To SSH into a running VM (requires network configuration):
```bash
ssh -i ~/.glidex/vm_key root@<vm-ip>
```

## License

MIT

## Acknowledgments

- [Firecracker](https://firecracker-microvm.github.io/) - microVM hypervisor
- [Cloud-Hypervisor](https://www.cloudhypervisor.org/) - microVM hypervisor
- [Axum](https://github.com/tokio-rs/axum) - Web framework
- [Vite](https://vitejs.dev/) + [React](https://react.dev/) - Web UI stack
- [Tokio](https://tokio.rs/) - Async runtime
- [Tailwind CSS](https://tailwindcss.com/) - Utility-first CSS framework

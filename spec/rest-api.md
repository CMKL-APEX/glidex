# REST API

The control plane listens on `127.0.0.1:8841` and `[::1]:8841` by default (`GLIDEX_LISTEN` overrides). All request and
response bodies are JSON except for the console WebSocket.

## Endpoints

| Method | Path | Handler | Purpose |
|---|---|---|---|
| `GET` | `/health` | `health_check` | Liveness probe |
| `GET` | `/vms` | `list_vms` | List all VMs |
| `POST` | `/vms` | `create_vm` | Create a new VM |
| `GET` | `/vms/{id}` | `get_vm` | Get a VM by id |
| `DELETE` | `/vms/{id}` | `delete_vm` | Delete a VM (also stops it) |
| `POST` | `/vms/{id}/start` | `start_vm` | Start / resume a VM |
| `POST` | `/vms/{id}/stop` | `stop_vm` | Stop a VM |
| `POST` | `/vms/{id}/pause` | `pause_vm` | Pause a running VM |
| `GET` | `/vms/{id}/console` | `get_console_info` | Return console-socket path and availability |
| `GET` | `/vms/{id}/console/ws` | `console_ws` | WebSocket upgrade — see below |
| `POST` | `/vms/{id}/devices` | `attach_device` | Attach a VFIO PCI device |
| `DELETE` | `/vms/{id}/devices` | `detach_device` | Detach a VFIO PCI device |
| `GET` | `/pci-devices` | `list_pci_devices` | Enumerate host PCI devices |
| `GET` | `/credentials` | `list_credentials` | List stored guest logins (no hashes) |
| `POST` | `/credentials` | `create_credential` | Create a login; password is hashed on arrival |
| `GET` | `/credentials/{username}` | `get_credential` | Get one login (no hash) |
| `PUT` | `/credentials/{username}` | `update_credential` | Change password and/or replace SSH keys |
| `DELETE` | `/credentials/{username}` | `delete_credential` | Delete; `409` while a VM uses it |

All handlers live in `crates/glidex-control-plane/src/api.rs`.

## Payloads

### `POST /vms`

```json
{
  "name": "my-vm",
  "vcpu_count": 2,
  "mem_size_mib": 1024,
  "kernel_image_path": "/path/to/vmlinux",
  "rootfs_path": "/path/to/rootfs.ext4",
  "kernel_args": "console=ttyS0 root=/dev/vda reboot=k panic=1",
  "hypervisor": "qemu",
  "vfio_devices": ["/sys/bus/pci/devices/0000:41:00.0"]
}
```

- `kernel_args`, `hypervisor`, `vfio_devices` are optional.
- `firmware_path` (Cloud Hypervisor only) boots the disk through UEFI
  firmware instead of a kernel, e.g.
  `{"hypervisor": "cloudhypervisor", "firmware_path": "~/.glidex/CLOUDHV.fd",
  "rootfs_path": "~/images/ubuntu-cloudimg.raw", ...}`. `glidex-install`
  downloads the firmware to that path. Relative paths resolve against the
  control plane's working directory, so prefer absolute or `~` paths. `kernel_image_path`
  and `kernel_args` may then be omitted; they are ignored if given. The
  console is attached to the guest's serial port (`ttyS0`), which is where
  distro cloud images put their login prompt.
- `cloud_init_path` (Cloud Hypervisor only) attaches a NoCloud seed image
  read-only as the second disk. When omitted on a firmware boot, glidex
  generates one at `/tmp/cloud-hypervisor-<id>.cloudinit.img` on every start
  (needs `mkdosfs`/`mcopy` from dosfstools/mtools): hostname from the VM
  name, DHCP on `en*`, and a sudo user `cloud` with the control-plane
  user's `~/.ssh/*.pub` keys. Password login stays locked unless
  `GLIDEX_CLOUD_INIT_PASSWD_HASH` holds a crypt hash
  (`openssl passwd -6`), applied via `chpasswd` so it also works on a
  rootfs that was provisioned before.
- `~` is expanded server-side (see [data-model.md](data-model.md)).
- Response: `201 Created` with a `VmResponse`.

- `credential` (firmware boot only, no custom `cloud_init_path`) names a
  stored credential that the generated seed provisions as the guest login.
  Unknown names are `400 invalid_config`. See [credentials.md](credentials.md).

### `POST /credentials`, `PUT /credentials/{username}`

```json
{ "username": "alice", "password": "…", "ssh_authorized_keys": ["ssh-ed25519 AAAA… alice@host"] }
```

- `POST` needs a password, at least one key, or both. `PUT` takes the
  same body without `username`; omitted fields are unchanged and an empty
  key list clears the keys.
- Responses are `CredentialInfo`:
  `{ "username", "has_password", "ssh_authorized_keys", "created_at", "updated_at" }`.
  The password hash is never returned.

### `POST /vms/{id}/devices`, `DELETE /vms/{id}/devices`

```json
{ "device_path": "/sys/bus/pci/devices/0000:41:00.0" }
```

Semantics depend on VM state:

- **Running**: hot-plug via the hypervisor API. On success the
  config is updated and persisted; on persistence failure the
  hot-plug is rolled back.
- **Created / Stopped**: config-only mutation. The device will be
  present at next start.
- **Paused**: rejected with `invalid_state` (hot-plug while paused
  is a mess to reason about; require unpause first).

### `GET /vms/{id}/console`

```json
{
  "vm_id": "…",
  "console_socket_path": "/tmp/qemu-<id>.console.sock",
  "log_path": "/tmp/qemu-<id>.log",
  "available": true
}
```

`available` is `true` iff `state == Running`. This endpoint is used
by `gxctl` (which then `connect()`s to the Unix socket directly) and
by any client that wants to find the log file without opening the
WebSocket.

## Error model

All non-2xx responses are:

```json
{ "error": "<code>", "message": "<human readable>" }
```

Status code mapping (`api::error_to_response`):

**Invariant.** Request validation in `VmManager::create_vm` (zero vCPUs
or memory, missing kernel *and* firmware, `firmware_path` /
`cloud_init_path` on a non-Cloud-Hypervisor VM) returns
`HypervisorError::InvalidConfig`, which maps to `400`. Failures that are
not the caller's fault — e.g. `HypervisorError::CloudInit` when
`mkdosfs`/`mcopy` are missing at start time — stay `500`.

| `VmManagerError` variant | HTTP | `error` code |
|---|---|---|
| `VmNotFound` | `404` | `not_found` |
| `VmAlreadyExists` | `409` | `conflict` |
| `InvalidState` | `400` | `invalid_state` |
| `HypervisorError(InvalidConfig)` | `400` | `invalid_config` |
| `HypervisorError` (any other) | `500` | `hypervisor_error` |
| `PersistenceError` | `500` | `persistence_error` |
| `HypervisorNotAvailable` | `503` | `hypervisor_unavailable` |
| `Credential(NotFound)` | `404` | `not_found` |
| `Credential(AlreadyExists)`, `CredentialInUse` | `409` | `conflict` |
| `Credential(Invalid)` | `400` | `invalid_credential` |
| `Credential` (storage/hashing) | `500` | `credential_error` |

## Console WebSocket

### Protocol

`GET /vms/{id}/console/ws` upgrades to a WebSocket. The handler
`console_ws → bridge_console`:

1. Looks up the VM. If `VmNotFound`, responds `404`. Any other
   lookup error responds `500`.
2. Opens a `tokio::net::UnixStream` to the VM's `console_socket_path`.
   If the connect fails, sends a `Message::Text` containing the
   error string and then `Message::Close`.
3. Enters a `select!` loop:
   - Bytes read from the Unix socket → sent as `Message::Binary` to
     the browser.
   - `Message::Binary` / `Message::Text` from the browser → written
     to the Unix socket. Close / error / `None` from the browser
     ends the loop.

### Client expectations

- Use `binaryType = "arraybuffer"` on the `WebSocket`. The server
  only ever sends binary frames (except the very first frame in
  the connect-failed case, which is text).
- Writes are sent as binary; the server accepts either binary or
  text (text is treated as the UTF-8 byte sequence of its content).
- No framing — this is a byte stream of terminal data in both
  directions. The *only* transport semantics are WebSocket
  boundaries, which are irrelevant to the consumer.

### Replay-on-connect behavior

The console Unix socket on the server is listened on by a proxy
thread inside the hypervisor backend. That thread replays the
captured log file to every newly-accepted client before starting
live broadcast. So opening a WebSocket on a VM that has already
booted will immediately flush the boot-time output into your xterm.
See [console.md](console.md).

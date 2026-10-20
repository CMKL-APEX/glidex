# Quick start

This page takes one Linux host from nothing to a running VM: install
Glidex, log in, create a NAT network, pull a cloud image, add a login
for the guest, then create, start, use and stop a VM. Each step shows
`gxctl` and, where it helps, the web UI.

For more choices at any step, see the [User guide](user-guide/README.md)
and the [Operator guide](operator-guide/README.md).

## Requirements

- Linux on x86_64 or aarch64, with KVM enabled (`/dev/kvm` exists).
- A package manager the installer knows: `apt-get`, `dnf`/`yum` or
  `pacman`.
- An account with `sudo` rights. The installer asks for your sudo
  password and nothing else.
- `git`, and a Rust toolchain to run the installer from source (for
  example from [rustup.rs](https://rustup.rs)). The installer then
  installs or updates everything else, including Rust itself.
- Internet access, to download packages, the hypervisors and images.

Check KVM first:

```bash
ls -l /dev/kvm
```

If it's missing, enable virtualization in the firmware settings and load
the module (`sudo modprobe kvm_intel` or `sudo modprobe kvm_amd`).

## 1. Install

```bash
git clone https://github.com/CMKL-APEX/glidex.git && cd glidex
cargo run -p glidex-install
```

The installer installs what is missing and updates what is outdated:
system packages, Cloud Hypervisor and its UEFI firmware, QEMU with OVMF,
Open vSwitch for VM networking, and the Glidex binaries and web UI. It
sets up these systemd services:

- `glidex-control-plane`: the API, running as the `glidex` system user.
- `glidex-ui`: the web UI, running as `glidex-ui`.
- `glidex-authd`: checks local account passwords (PAM) for the web UI.

When it finishes, it prints a summary like this:

```text
glidex runs as systemd services (control plane as 'glidex', web UI as 'glidex-ui'):
     web UI:  https://host.example.org:5173
     API:     /run/glidex-cp/api.sock (members of glidex-users), https://host.example.org:8841 (tokens)
     web UI certificate SHA-256: 3A:7F:…:C2
     API certificate SHA-256: 91:0B:…:5E
```

Keep the two certificate fingerprints: you check them in step 3. The
installer doesn't open any firewall ports; if you'll use the web UI
from another machine, allow TCP 5173 (and 8841 for the API) yourself.

Re-run `cargo run -p glidex-install` at any time to update. See
[Installation](operator-guide/installation.md) for its options.

## 2. Log out and back in

The installer adds you to two groups:

- `glidex-users`: lets you use `gxctl` on this host and log in to the
  web UI with your account's password.
- `glidex-admin`: makes you an administrator of this Glidex host, and
  owner of its `default` project.

Group changes apply to new logins only, so log out and back in (or
reconnect your SSH session). Then check:

```bash
id -nG            # lists glidex-users and glidex-admin
gxctl whoami
```

On the host, `gxctl` talks to the control plane over its Unix socket
(`/run/glidex-cp/api.sock`) and identifies you by your Unix account:
there is no separate login.

`gxctl` with no arguments starts an interactive shell (`gxctl>`, with
Tab completion and `help`). With arguments it runs one command and
exits. This page uses the one-command form; inside the shell, type the
same words without `gxctl`.

## 3. Open the web UI

Print its address:

```bash
gxctl ui
```

Open `https://<host>:5173` in a browser. Unless an administrator
configured a certificate, the UI uses a self-signed one, so the browser
warns you. Before you accept the warning, open the certificate details
in the browser and compare its SHA-256 fingerprint with the
`web UI certificate SHA-256` line the installer printed. Accept only if
they match.

If you no longer have the installer's output, print the fingerprint on
the host:

```bash
sudo openssl x509 -in /var/lib/glidex-ui/tls/ui.crt -noout -fingerprint -sha256
```

Sign in with your Unix username and password. The header shows the
pages you can use (**VMs**, **Images**, **Disks**, **Credentials**,
**Networking**, **Projects** and, as an administrator, more) and a
**Project** selector; leave it on `default`.

To replace the self-signed certificates, see [TLS](operator-guide/tls.md).

## 4. Create a NAT network

Glidex creates no network by itself. Create a NAT network: VMs on it get
an address by DHCP and reach the outside through the host. Name it
`default` and the VM forms preselect it.

```bash
gxctl network-add default
```

`network-add` makes a NAT network unless you pass `--isolated` or
`--bridged <bridge>`. Its subnet is the first free `/24` in
`10.88.0.0/16` unless you pass `--subnet <cidr>`. Only your default
project (here, `default`) may use it until you grant it to others.

In the web UI: **Networking** → **Add Network**, enter the name
`default`, keep **Mode** at **NAT (DHCP + masquerade)**, and click
**Create**.

More in [Networks](user-guide/networks.md) and
[Host networking](operator-guide/host-networking.md).

## 5. Pull a cloud image

List the catalog, then pull an image. `gxctl` follows the download until
the image is verified and ready.

```bash
gxctl image catalog
gxctl image pull ubuntu-26.04
```

The catalog also lists UEFI firmware. The control plane imports the
firmware the installer set up (`cloudhv-edk2` for Cloud Hypervisor,
`ovmf` for QEMU) at its first start, so you don't need to pull it.
`gxctl image list` shows both.

In the web UI: **Images** → **Cloud image catalog**, then **Pull** on
the image you want.

More in [Images and disks](user-guide/images-and-disks.md).

## 6. Add a guest credential

A credential is a username with a password, SSH public keys, or both.
Glidex sets it up in the guest when the VM first boots. Without one, you
can't log in to the VM.

```bash
gxctl credential-add
```

It asks for a username (default: yours), a password (twice; leave it
empty for SSH-key-only login), and SSH public key files (default: your
`~/.ssh/*.pub`). Give it a password if you want to log in on the
console in step 8.

In the web UI: **Credentials** → **Add Credential**, fill in
**Username**, a password and/or **SSH public keys (optional)**, and
click **Add**.

More in [Guest credentials](user-guide/credentials.md).

## 7. Create and start a VM

`gxctl create` asks its questions one by one; press Enter to take the
default shown in brackets. For an Ubuntu VM on Cloud Hypervisor:

```text
$ gxctl create
VM name: my-vm
vCPU count [1]: 2
Memory (MiB) [512]: 2048
Hypervisor [cloudhypervisor/qemu] (default: cloudhypervisor):
UEFI firmware [...] [cloudhv-edk2]:
Boot disk from [image/disk/path] [image]:
Image (ubuntu-26.04) [ubuntu-26.04]:
Root disk size in GiB [10]: 20
Data disks (optional, comma-separated disk names):
cloud-init seed image (optional, default: auto-generated):
Login credential (...) [alice]:
Networks (comma-separated; available: default; 'none' for no NIC) [default]:
VFIO PCI devices (comma-separated, e.g. /sys/bus/pci/devices/0000:41:00.0):
VM created successfully!
```

Type the root disk size rather than taking the default: an empty answer
makes the disk the size of the image itself.
<!-- TODO(verify): gxctl prompts "Root disk size in GiB [10]" but an empty answer sends no size, and the server then sizes the disk to the image's virtual size, not 10 GiB. -->

The VM is created stopped. Start it; `start` waits until it runs:

```bash
gxctl start my-vm
gxctl list
```

In the web UI: **VMs** → **+ Create VM**. Fill in **VM Name**, set
**vCPU Count** and **Memory (MiB)**, keep **Boot Mode** at **UEFI
firmware (distro cloud image)** and **Boot Disk** at **New disk from an
image**, pick the **Image**, set **Root disk (GiB)**, pick your
**Login Credential**, and check the `default` network under
**Networks**. Check **Start it now** to skip the separate start, then
click **Create VM**. Without **Start it now**, click **Start** on the
VM's card.

More in [Virtual machines](user-guide/vms.md).

## 8. Open the console

```bash
gxctl connect my-vm
```

You're on the VM's serial console. The first boot takes a little while
for cloud-init to apply the credential; then log in with its username
and password. Press `Ctrl+]` to detach; the VM keeps running.
`gxctl log my-vm` prints what the console showed so far.

In the web UI: **Open Console** on the VM's card or its detail page.

Once the guest has an address on the NAT network (`gxctl get my-vm`
shows it), you can also SSH to it from the host with the keys in your
credential.

More in [The console](user-guide/console.md).

## 9. Stop the VM

```bash
gxctl stop my-vm --graceful
```

`--graceful` presses the guest's power button and waits up to 60 seconds
(`--graceful <secs>` to change that) for it to shut down, then stops it.
Without `--graceful`, `stop` ends the VM at once, like pulling the plug.

In the web UI: **Shut down** for the graceful stop, **Stop** for the
immediate one.

To remove the VM and the root disk made for it (it asks first; add
`--keep-disk` to keep the disk):

```bash
gxctl delete my-vm
```

## Next steps

- Read [Concepts](concepts.md) for how projects, images, disks,
  networks and VMs fit together.
- Use Glidex from another machine: [Logging in](user-guide/logging-in.md).
- Invite other people and set quotas: [Projects](user-guide/projects.md)
  and [Access control](operator-guide/access-control.md).
- Add more hosts: [Clusters](operator-guide/clusters.md).

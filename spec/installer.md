# `glidex-install` bootstrapper

Source: `crates/glidex-install/src/main.rs`. Run it with
`cargo run -p glidex-install` from the repo root. It replaces the
old `install.sh` shell script.

## Philosophy

This is the one place where "check if something is missing and fix
it" logic lives. Other crates assume their dependencies are present.
The installer is a **linear** script — no plugins, no reusable
library, one file of top-to-bottom procedural code — because its
only invocation mode is interactive bootstrap.

## Inputs it discovers

- `std::env::consts::OS` — rejects anything other than `linux`
  because none of the hypervisors target other OSes.
- `std::env::consts::ARCH` — accepts `x86_64` and `aarch64`,
  rejects the rest.
- `libc::geteuid()` — if root, installs binaries to `/usr/local/bin`;
  otherwise to `~/.local/bin` (created if missing).
- Presence of `curl`, `apt-get | dnf | yum | pacman`, `bun`,
  `rustup`, `mkdosfs`, `mcopy`, `cloud-hypervisor`,
  `qemu-system-x86_64`.

All detection goes through a `command_exists` helper that shells out
to `command -v`.

## Steps, in order

1. **Rust** — if `rustc` is missing, curls the rustup installer and
   runs it non-interactively. If `rustup` is present, runs
   `rustup update stable` for good measure. The installer does
   not re-exec into a new shell; a fresh `source $HOME/.cargo/env`
   is needed for subsequent shell invocations.
2. **Bun** — installed via `curl -fsSL https://bun.sh/install | bash`
   if missing. Bun is the UI's package manager and Vite runner.
3. **Cloud-Hypervisor** — downloads the arch-appropriate static
   binary from GitHub releases at the version pinned in
   `CLOUD_HYPERVISOR_VERSION` (currently `v50.0`), installs it via
   `install_binary` helper.
4. **UEFI firmware** — downloads Cloud-Hypervisor's EDK2 firmware
   from <https://github.com/cloud-hypervisor/edk2/releases> at the tag
   pinned in `EDK2_FIRMWARE_VERSION` (currently `ch-811ce5ea35`) into
   `~/.glidex/CLOUDHV.fd` (`CLOUDHV_EFI.fd` on aarch64), then ensures
   `dosfstools` + `mtools` are installed for cloud-init seed images.
   See below.
5. **QEMU** *(optional, prompts)* — delegates to the system
   package manager (`apt-get install qemu-system-x86 qemu-kvm`,
   `dnf install qemu-kvm`, etc). We don't ship a QEMU binary
   because its distribution story is already well handled by
   every Linux distro and the resulting tree is large.
6. **KVM access check** — verifies `/dev/kvm` exists and is
   read-writable. On failure it prints the appropriate
   `usermod -aG kvm $USER` instruction but does not make the
   change itself.
7. **Control-plane build** — runs `cargo build --release
   -p glidex-control-plane` and offers to install
   `glidex-control-plane` and `gxctl` into the install dir.
8. **UI dependencies** — runs `bun install` inside
   `crates/glidex-ui/ui`. Skipped with a warning if bun isn't
   on PATH.
9. **Usage blurb** — prints the 1-2-3 for starting the server,
   UI, and CLI.

## Binary installation helper

`install_binary(src, dst)` behaves differently based on privilege:

- Root: `install -m 0755 src dst`.
- Non-root: `fs::copy(src, dst)` first (works under `~/.local/bin`);
  on permission error, falls back to `sudo install -o root -g root
  -m 0755 src dst`.

This accommodates both dev setups (`~/.local/bin`) and system
installs (`/usr/local/bin`) without requiring the user to choose
up front.

## UEFI firmware

Firmware boot (see [hypervisors.md](hypervisors.md#firmware-boot)) needs
Cloud-Hypervisor's EDK2 build. It is downloaded rather than committed
to the repo, to keep a 4 MB binary out of git history.

- **Pinned, not `latest`.** The edk2 fork publishes a release per
  build (`ch-<commit>`), several a month. Pinning keeps installs
  reproducible and ties the version to one that has been boot-tested;
  bump `EDK2_FIRMWARE_VERSION` and the digests in `firmware_asset`
  together.
- **Verified.** The file is downloaded to `<name>.part` next to the
  destination, checked against the sha256 in `firmware_asset` (the
  digests GitHub publishes for the release assets), then renamed into
  place. A mismatch deletes the partial file and aborts.
- **Idempotent.** If the destination already has the expected sha256
  the download is skipped; if it differs (e.g. a hand-placed newer
  build) the user is asked before it is replaced, default no.
- **Consumers.** `hypervisor::cloud_hypervisor::default_firmware_path()`
  returns the same path; `gxctl create` offers it as the default,
  the control plane reports whether it exists at startup, and the
  end-to-end functional test uses it unless `GLIDEX_TEST_FIRMWARE`
  is set. The installer can't depend on the control-plane crate, so
  the file name is duplicated there.

## No sample kernel / rootfs

Earlier versions downloaded a kernel and built an ext4 rootfs from
Firecracker's CI artifacts. That was removed together with Firecracker
support. The out-of-the-box path is now firmware boot: the installer
provides `CLOUDHV.fd`, and any distro cloud image (converted to raw)
boots with an auto-generated cloud-init seed. Kernel boot still works
but needs a user-supplied kernel + rootfs.

## What it deliberately does not do

- **Does not write systemd units** or any other service
  supervisor config. Running the control plane is a shell job.
- **Does not configure networking** (bridges, iptables, DHCP).
- **Does not touch the ReDB file**. If one exists from a previous
  run it is left alone.
- **Does not modify `/etc`** beyond what a package-manager install
  of QEMU implicitly does.

## Dependencies

Intentionally minimal:

```toml
anyhow = "1"
colored = "3"
dirs = "6"
libc = "0.2"
tempfile = "3"
```

Downloads are done by shelling to `curl`, not by pulling in a HTTP
stack. The installer itself is therefore small and fast to build.

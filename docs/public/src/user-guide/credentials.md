# Guest credentials

Distro cloud images ship with no usable login: they wait for
[cloud-init](https://cloudinit.readthedocs.io/) to create one on first
boot. Glidex stores the logins you want as **credentials** and hands the
one a VM names to cloud-init.

A credential is a Linux username with a password, SSH public keys, or
both. Credentials belong to a project and are used by that project's
VMs; seeing and managing them needs the `editor` role.

## How a credential reaches the guest

When a VM boots a cloud image through UEFI firmware, Glidex writes a
small cloud-init seed volume for it (NoCloud, labelled `CIDATA`) and
attaches it. From the VM's credential, cloud-init creates the user in
the guest:

- with passwordless `sudo` and `/bin/bash`;
- with the credential's password, and SSH password logins on, if it has
  one; the password locked otherwise;
- with the credential's SSH keys as `authorized_keys`.

The seed also sets the guest's hostname from the VM's name, turns on
DHCP on each of its network interfaces, and grows the root filesystem
to fill its partition.

cloud-init applies the user **on the VM's first boot only**. Changing or
deleting a credential later does not change VMs that already booted;
change the login inside those guests instead.

### VMs without a credential

A VM created without a credential still gets a seed, with the guest user
`cloud`, no SSH keys and a locked password: nobody can log in, on the
network or on the [console](console.md). Glidex never adds keys or
passwords of its own. Pick a credential when you create the VM (see
[Virtual machines](vms.md)).

## Adding a credential

**Web UI:** **Credentials**, **Add Credential**:

- **Username**: lowercase letters, digits, `_` and `-`, starting with a
  letter or `_`, at most 32 characters.
- **Password** and **Confirm** (optional): 8 to 256 characters.
- **SSH public keys** (optional): one OpenSSH public key per line
  (`ssh-ed25519 AAAA… alice@laptop`).

A password, a key, or both is required. If you have a local account on
the host, the form fills in your username and the public keys in your
`~/.ssh/*.pub`.

**gxctl:**

```text
$ gxctl credential-add
Username [alice]:
Leave the password empty for SSH-key-only login.
Password:
Confirm password:
SSH public key files (comma-separated; 'none' for no keys) [~/.ssh/id_ed25519.pub]:
Credential created: alice (password: set, SSH keys: 1)
```

It offers your login name and your own `~/.ssh/*.pub` files as
defaults. Give `.pub` files, never private keys: `gxctl` refuses a file
that holds a private key.

The credential goes into the selected project (see
[Choosing a project](projects.md#choosing-a-project)); use
`gxctl --project lab credential-add` for another.

These usernames are refused because distro images already have them:
`root`, `ubuntu`, `daemon`, `bin`, `sys`, `sync`, `games`, `man`, `lp`,
`mail`, `news`, `uucp`, `proxy`, `www-data`, `backup`, `list`, `irc`,
`nobody`, `systemd-network`, `systemd-resolve`, `messagebus`, `sshd` and
`syslog`.

## Listing, changing and deleting credentials

**Web UI:** **Credentials** lists each username, whether it has a
password, its number of SSH keys and when it changed. **Edit** sets a
new password (leave it empty to keep the current one) or replaces the
keys; **Delete** removes it.

**gxctl:**

```bash
gxctl credentials                 # list
gxctl credential-passwd alice     # set a new password (asked twice, hidden)
gxctl credential-keys alice       # replace the SSH keys; empty removes them all
gxctl credential-rm alice
```

A credential must keep a password or at least one key, and can't be
deleted while a VM uses it.

Passwords are hashed (SHA-512-crypt) as soon as they arrive. Glidex
never stores, logs or returns the password itself, and the API never
returns the hash; the lists only say whether a password is set.

## Using a credential

When you create a VM that boots a cloud image:

- **Web UI**: choose it under **Login Credential**. If the project has a
  credential with your username, it is preselected.
- **gxctl** `create`: answer the *Login credential* prompt; the default
  is the credential named like you, if there is one, and `none` creates
  the VM without a login.
- **API**: `"credential": "alice"` in `POST /vms`.

A credential only works with the seed Glidex generates: a VM that boots a
kernel directly, or brings its own seed, can't use one.

## Your own cloud-init seed

To provision a guest your own way (other users, packages, files), give
the VM a cloud-init seed image you made, instead of the generated one:
`gxctl create` asks for a *cloud-init seed image*, and the API takes
`"cloud_init_path": "/srv/seeds/web.img"`. The control plane then
attaches that file as it is.

- It is a path on the host, opened by the control plane: it needs the
  host-wide `grant.host-paths` permission (ask an administrator), and
  the file must be readable by the service (under the systemd units, not
  in your home directory or `/tmp`).
- The VM can't use a credential, and Glidex doesn't add networking,
  hostname or root-partition growth: put what you need in the seed.

A seed is a small FAT or ISO volume labelled `CIDATA` holding
`user-data` and `meta-data` (and optionally `network-config`), for
example made with `cloud-localds` from the `cloud-image-utils` package.
<!-- TODO(verify): the cloud_init_path seed format is whatever the guest's cloud-init NoCloud datasource accepts; Glidex only attaches the file. Confirm the tooling suggestion against the test fixtures if any. -->

## Over the API

```bash
api() { curl --cacert host.crt -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' "$@"; }
api -X POST https://host.example.org:8841/credentials \
  -d '{"project": "lab", "username": "alice", "ssh_authorized_keys": ["ssh-ed25519 AAAA… alice@laptop"]}'
api 'https://host.example.org:8841/credentials?project=lab'
api -X DELETE 'https://host.example.org:8841/credentials/alice?project=lab'
```

See the [REST API](../api/rest.md) for updating a credential.

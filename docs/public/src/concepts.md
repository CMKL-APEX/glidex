# Concepts

This page explains the objects Glidex manages and the words the rest of
these docs use for them. Each entry links to the page with the details.

## Control plane

The **control plane** (`glidex-control-plane`) is the service that
holds the state of everything (VMs, disks, images, networks, projects,
users) and acts on it: it starts hypervisors, creates disks and asks the
host's network helper to set up networks. It serves:

- the **REST API** and the **RPC API** (gRPC, gRPC-Web and Connect) on
  HTTPS port 8841, and on a local Unix socket for `gxctl` on the host;
- the console of every VM.

The **web UI** (`glidex-ui`, HTTPS port 5173) and **`gxctl`** are
clients of that API: anything you do in one you can do in the other.
See the [API reference](api/README.md) and the
[`gxctl` reference](reference/gxctl.md).

The control plane doesn't sit between a VM and its hypervisor: with
the installer's systemd units, each VM runs in its own unit, so VMs keep
running when the control plane restarts or is upgraded.

## Nodes, servers and agents

A **node** is a host that runs VMs. A standalone host is a single node.

Several hosts can form a **cluster**. Each node has a role:

- A **server** keeps a copy of the cluster's state in a replicated
  (Raft) store and answers the API. Run one, or three or more for a
  quorum that survives the loss of a server.
- An **agent** only runs VMs. It has no API of its own; its web UI
  relays to the servers.

Nodes join with a one-time **join token** and talk to each other over
mutual TLS. A **scheduler** picks the node for each new VM, and explains
its choice. A node can be **drained** (no new VMs are placed on it),
removed, and rejoined. See [Clusters](operator-guide/clusters.md).

## Tenants and projects

A **project** is where VMs, disks, guest credentials and project
networks live. Each of these belongs to exactly one project. A project
has **members** with roles, and optional **quotas**: limits on VMs,
running VMs, vCPUs, memory, disk, networks and routers.

Every install starts with a project named `default`. In `gxctl`,
`--project <p>` picks a project for one command and `project use <p>`
sets your default; in the web UI, the **Project** selector in the header
does both. See [Projects](user-guide/projects.md).

A **tenant** is the organization that owns projects. A standalone host
has an operator tenant `admin`, which runs the host, and a tenant
`default`, which holds the `default` project. Most single-site installs
don't need more than that.

## Virtual machines

A **VM** has a name, a project, a hypervisor, vCPUs and memory, a boot
disk, zero or more data disks, network interfaces (one per network it
attaches to), optional PCI devices, and an optional guest credential.

Glidex separates what you ask for from what it last saw:

- The **desired state** (the spec) is what you want: the VM's
  configuration, and whether it should be **running**, **paused** or
  **stopped**. Start, stop, pause and edits change only this, and
  return at once.
- The **state** (the status) is what the control plane last observed:

  | State | Meaning |
  |---|---|
  | created | Never started. |
  | starting | The hypervisor is being launched. |
  | running | The guest is running. |
  | paused | The guest is paused. |
  | stopping | A stop is in progress. |
  | stopped | Not running. |
  | failed | It couldn't be started, or it crashed. |
  | unknown | The control plane can't tell whether its hypervisor is still running. |

A controller keeps working until the state matches the desired state.
While it does, the web UI shows both (for example, `Stopped → Running`)
and `gxctl start` waits for it. Some changes only apply at the next
start; the VM then says a restart is required.

Two settings say what happens when the VM stops on its own:

- **Restart policy** (**If It Crashes** in the web UI): restart it with
  backoff (the default) or leave it stopped.
- **On host boot** (**After a Host Reboot**): start it again if it was
  running, or leave it stopped.

Stopping is either **graceful** (press the guest's power button, wait,
then stop) or immediate. Each VM keeps a short list of **events**:
starts, exits, restarts. See [Virtual machines](user-guide/vms.md).

## Hypervisors

Each VM runs on one **hypervisor**:

- **Cloud Hypervisor** (`cloudhypervisor`), the default.
- **QEMU** (`qemu`).

Both support the same features in Glidex: UEFI boot of cloud images,
direct kernel boot, networks, disks, PCI passthrough (VFIO), pause and
resume, and graceful shutdown. You pick the hypervisor when you create
the VM.

## Images and firmware images

An **image** is a read-only file disks are made from. Images belong to
the host (or cluster), not to a project: everyone can use them, and
administrators with the image role pull and delete them.

- A **cloud image** is a bootable distribution disk image (qcow2 or
  raw). Pull one from the built-in **catalog** by key (for example
  `ubuntu-26.04`, `debian-13`, `fedora-cloud`) or from an HTTPS URL.
  Catalog images are checked against their published SHA-256; an image
  from a URL without a checksum is kept but marked **unverified**.
- A **firmware image** is the UEFI firmware a VM boots through, built
  for one hypervisor: `cloudhv-edk2` (`CLOUDHV.fd`) for Cloud
  Hypervisor, `ovmf` for QEMU. A new VM uses the newest firmware image
  for its hypervisor unless you choose another.

An image is **downloading**, **verifying**, **ready**, **failed** or
**missing** (its file is gone). See
[Images and disks](user-guide/images-and-disks.md).

## Disks

A **disk** is a virtual disk in a project. It is attached to at most one
VM at a time, as its boot (root) disk or as a data disk. A disk is:

- **blank**, of a size you choose, or
- made **from an image**: either **linked** (thin: a qcow2 overlay on
  the image, so it takes space only for what the VM writes) or a
  **full** copy.

When you create a VM from an image, Glidex makes a linked root disk for
it, grows it to the size you ask for, and extends the guest's root
partition to fill it. That disk belongs to the VM and is deleted with
it unless you keep it. Disks can be resized; a resize of a disk that
is in use is held until it can be applied.

## Guest credentials

A **guest credential** is a login for inside VMs: a username with a
password, SSH public keys, or both. Credentials belong to a project.
Glidex stores only a hash of the password.

When you create a VM with a credential, Glidex generates a cloud-init
seed that creates that user in the guest. It is applied on the VM's
first boot only: later changes to the credential don't reach VMs that
already booted. You can also give your own cloud-init seed instead. See
[Guest credentials](user-guide/credentials.md).

## Networks

A **network** connects VM interfaces. Its **mode** is one of:

- **NAT**: a private subnet with DHCP; VMs reach the outside through
  the host (masquerade).
- **Isolated**: VMs on it reach each other and nothing else.
- **Bridged**: VMs sit on an existing network of the host (in a
  cluster, a **provider** network on a physical network).

A **host network** is created by an administrator and offered to the
projects allowed to use it. A **project network** belongs to a project,
and its owner can share it with other projects.

On a standalone host, networks are Open vSwitch bridges on that host.
In a cluster with OVN enabled, a **cluster network** spans every node.
A VM's interface is a **tap** device (the default) or a **vhost-user**
port (with OVS-DPDK).

A network is **pending**, **ready** or **degraded**, or reports that the
host's network helper is unavailable. See [Networks](user-guide/networks.md)
and [Host networking](operator-guide/host-networking.md).

## Console

Every VM has a **serial console**. Any number of clients (browser tabs,
`gxctl connect`) can share it at once, and everything it prints is
logged to disk, so you can read what happened before you connected. See
[The console](user-guide/console.md).

## Users, teams and roles

A **user** is a person known to Glidex. Users log in through a **login
connector**: local accounts (PAM) or OIDC single sign-on. On the host
itself, `gxctl` identifies you by your Unix account. A **team** is a
group of users; teams can mirror Unix or identity-provider groups.

A **role binding** gives a user, team or token a **role** on a
resource. Project roles, each including the one before it:

| Role | Can |
|---|---|
| `viewer` | See the project, its VMs, disks and usage. |
| `operator` | Also start, stop and pause VMs, and use their consoles. |
| `editor` | Also create, change and delete VMs and disks, manage guest credentials, and attach networks. |
| `owner` | Also manage members, and manage and share project networks. |

Host roles cover the host or cluster itself: `auditor` (read-only,
including the audit log), `image-admin` (images), `net-admin` (host
networks and PCI devices), and `system-admin` (everything). See
[Access control](operator-guide/access-control.md).

## Tokens

An **API token** lets a script or another machine call the API. A
token has a name and always expires: after a default lifetime, or after
the number of days you choose, up to a limit the site sets.

- A **personal token** acts as you. It can be narrowed to some of your
  roles.
- A **service-account token** belongs to a project and has only the
  roles it is given.

The secret is shown once, when you create the token. Create tokens with
`gxctl token create` or on the web UI's **Tokens** page; revoke them the
same way. See [Logging in](user-guide/logging-in.md).

## Policies and the audit log

Every request is authorized by **Cedar policies**: built-in ones that
implement the roles above, plus optional **site policies** an
administrator adds to allow or forbid more. Some sensitive operations
also need a recent login. Changes, and refused requests, are recorded
in the **audit log**: who did what, to what, and with what result. See
[Access control](operator-guide/access-control.md).

## Usage

Glidex meters what each VM, disk and network uses (CPU, memory, disk
I/O and network traffic) and reports it per project, VM, disk, NIC or
network, by hour, day or billing month. See [Usage](user-guide/usage.md).

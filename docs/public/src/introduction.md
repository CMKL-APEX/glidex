# Introduction

Glidex is a control plane for KVM virtual machines. It runs VMs on
[Cloud Hypervisor](https://www.cloudhypervisor.org/) (the default) or
[QEMU](https://www.qemu.org/), and you drive it from a web UI, the
`gxctl` CLI, or its REST and RPC APIs. It runs on a single Linux host,
or on many hosts joined into one cluster.

## Features

- **Two hypervisors, one feature set.** Cloud Hypervisor and QEMU both
  boot distribution cloud images through UEFI firmware, take networks,
  disks and PCI (VFIO) devices, and support pause, resume and graceful
  shutdown.
- **Images and disks.** Pull checksum-verified cloud images (Ubuntu,
  Debian, Fedora) from a built-in catalog, or any image from an HTTPS
  URL. Each VM gets a thin root disk linked to its image, grown together
  with its root partition.
- **Guest logins.** Store a username with a password and/or SSH keys
  once, and pick it when you create a VM. Glidex puts it into a
  cloud-init seed it generates for the VM.
- **Networking.** NAT networks with DHCP, isolated networks, and
  bridged networks on the host's own LAN, built on Open vSwitch. Uplinks
  can use the kernel, AF_XDP or DPDK; VM ports are tap or vhost-user.
- **Console.** Each VM's serial console in the browser or in `gxctl`,
  shared by any number of clients and logged to disk.
- **Projects and access control.** Projects with members and quotas,
  roles and teams, API tokens, login with local accounts (PAM) or OIDC,
  Cedar policies, and an audit log.
- **Usage.** Metered CPU, memory, disk and network use, per project,
  VM, disk or network, by hour, day or billing month.
- **Clustering.** Many hosts act as one. Servers keep the cluster's
  state in a replicated store; agents only run VMs. Nodes join with a
  one-time token over mutual TLS, a scheduler places VMs and explains
  its choice, and nodes can be drained, removed and rejoined. Every node
  serves the web UI.
- **Cluster networks.** With OVN, networks span every node, NAT traffic
  leaves through shared gateway nodes, and projects can have their own
  routers.

## How these docs are organized

- [Quick start](quick-start.md): install Glidex on one host and boot
  your first VM.
- [Concepts](concepts.md): the objects Glidex manages and the words
  these docs use for them.
- [User guide](user-guide/README.md): everyday work with VMs, images,
  disks, credentials, networks, projects, the console and usage.
- [Operator guide](operator-guide/README.md): installing, configuring,
  securing and running hosts and clusters.
- [Developer guide](developer-guide/README.md): the architecture, design
  decisions, algorithms and performance figures, for contributors.
- [`gxctl` reference](reference/gxctl.md): the CLI's commands.
- [API reference](api/README.md): the REST and RPC APIs.

If you use VMs on a Glidex host someone else runs, start with
[Logging in](user-guide/logging-in.md). If you are setting up a host,
start with the [Quick start](quick-start.md).

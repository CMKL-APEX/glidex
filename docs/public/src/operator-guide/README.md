# Operator guide

This guide is for the people who install and run Glidex: they set up
hosts, decide who may log in and what they may do, connect VMs to the
network, join hosts into clusters, and keep it all running. Using VMs,
disks and networks inside a project is in the [User
guide](../user-guide/README.md).

## Components

On every host, the installer sets up these systemd units:

| Unit | Runs as | What it does |
|---|---|---|
| `glidex-control-plane.service` | `glidex` | The control plane: the REST and RPC API on port 8841 and on `/run/glidex-cp/api.sock`, the database, images and disks, and the controllers that keep every VM in the state asked for. |
| `glidex-ui.service` | `glidex-ui` | The web UI on port 5173. Serves the UI and passes its API calls to the control plane over `/run/glidex-cp/ui.sock`. Holds no rights of its own. On a cluster agent, it sends browsers to a server's web UI instead. |
| `glidex-authd.socket`, `glidex-authd.service` | root | Checks local passwords with PAM for web logins. Started on demand; answers only the control plane. |
| `glidex-netd.service` | root | Host networking: Open vSwitch bridges, NAT and DHCP, uplinks, VM ports, and binding NICs for DPDK and PCI passthrough. Takes changes only from the control plane. |
| `glidex-stord.service` | root | The storage helper for [LVM-thin datastores](datastores.md#lvm-thin): makes, activates, grows and removes Glidex's logical volumes, only in the volume groups listed in `/etc/glidex/stord.json` (none by default), and lets each VM's unit open exactly its own volumes. Takes requests only from the control plane; keeps no state. Installed and enabled with the services, idle until a volume group is allowed. |
| `glidex-vm@<id>.service` | `glidex` | One per running VM, in `glidex-vms.slice`: a small supervisor and the VM's hypervisor (Cloud Hypervisor or QEMU), in their own sandbox. Started and stopped by the control plane, never enabled. |

Open vSwitch runs under its own units, and on cluster members so does
OVN. The control plane needs `glidex-netd` only for VM networking and
`glidex-authd` only for password logins, and `glidex-stord` only for
LVM-thin datastores: without them VMs run without NICs, users log in
another way, and disks live in directories.

VMs belong to their own units, not to the control plane: restarting or
upgrading the control plane leaves them running, and it picks them up
again when it comes back. After a host reboot the control plane starts
the VMs that were running (see `reconcile.on_host_boot` in
[Configuration](configuration.md#reconcile-and-console)).

Check the services:

```bash
systemctl status glidex-control-plane glidex-ui glidex-authd.socket glidex-netd glidex-stord
journalctl -u glidex-control-plane -b
```

## Clients

| Client | Reaches | Authenticates with |
|---|---|---|
| Browser | `https://<host>:5173` (in a cluster, a server's) | Local password, single sign-on or a passkey |
| `gxctl` on the host | `/run/glidex-cp/api.sock` (not on a cluster agent) | Your Unix account (in `glidex-users`) |
| `gxctl` elsewhere, scripts | `https://<host>:8841` | An access token |

## Pages

- [Installation](installation.md): requirements, `glidex-install`, what
  it installs, users and groups, updating and uninstalling.
- [Configuration](configuration.md): `/etc/glidex/control-plane.json`,
  secrets, file locations and environment variables.
- [TLS certificates](tls.md): self-signed certificates and their
  fingerprints, your own certificate, Tailscale certificates, client
  trust.
- [Authentication](authentication.md): the local socket, local accounts,
  single sign-on, login connectors, access tokens and sessions.
- [Access control](access-control.md): roles, bindings, teams, tenants,
  quotas, site policies and the audit log.
- [Host networking](host-networking.md): Open vSwitch, bridges, uplinks
  and NAT on a host.
- [PCI passthrough](pci-passthrough.md): giving VMs host devices.
- [Datastores](datastores.md): where disks live; shared storage (NFS,
  CephFS) that lets VMs move between nodes, and LVM-thin volumes.
- [Clusters](clusters.md): creating a cluster, joining, draining and
  removing nodes, cluster networks.
- [Maintenance](maintenance.md): upgrades, backups and recovery.
- [Troubleshooting](troubleshooting.md): what to check when something
  doesn't work.

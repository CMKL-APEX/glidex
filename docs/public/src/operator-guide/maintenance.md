# Maintenance

Day-to-day care of a Glidex host: updates, what keeps running when the
services restart, backups, logs, and removing Glidex.

## Updating

Pull the new source and re-run the installer, from your checkout:

```bash
git pull
cargo run -p glidex-install
```

It reuses the choices saved in `/etc/glidex/install.conf`, updates what
is outdated, and restarts only the services whose binary or unit
changed. Running VMs keep running. Details are in
[Installation](installation.md#updating).

With the kernel OVS profile saved (the default), a re-run turns off
DPDK it finds on in Open vSwitch, which restarts `ovs-vswitchd`; with
bridges it waits for `--allow-ovs-restart`. If you turned DPDK on with
`gxctl` or the web UI and want to keep it, pass `--ovs-profile dpdk`
once. See [Back to the kernel profile](host-networking.md#back-to-the-kernel-profile).

On a cluster member, a re-run also upgrades OVN when the cluster's
`ovn-northd` is newer than the node's `ovn-controller`; see [OVN on
cluster members](installation.md#ovn-on-cluster-members). After you
upgrade OVN on the servers, re-run the installer on the other nodes.

Browsers check for a new web UI every time they load a page, so a
reload is enough to get it. A tab left open from before the update
keeps the old UI, and may fail to open pages it hadn't loaded yet:
reload it.

In a cluster, update one node at a time, agents first, then servers;
see [Rolling upgrades](clusters.md#rolling-upgrades).

## What survives a restart

Each running VM lives in its own systemd unit,
`glidex-vm@<vm-id>.service`, in the `glidex-vms.slice`. The unit runs
`glidex-vm-shim`, which supervises the hypervisor. These units are not
part of the control plane's service, so:

| Event | VMs |
| --- | --- |
| `glidex-control-plane` stops, restarts or is upgraded | Keep running. At start the control plane finds each running instance and adopts it; nothing is relaunched. |
| `glidex-ui` restarts | Not affected. |
| `glidex-stord` restarts | Keep running: they hold their volumes open. stord keeps no state. |
| `glidex-netd` restarts | Keep running. netd restores bridges, NAT and uplinks at start; vhost-user sockets survive in `/run/glidex/vhost`. |
| Host shutdown | Each VM gets its power button and `host_shutdown_grace_secs` (default 60, at most 300) to shut down, then is stopped. VMs stop before netd and Open vSwitch. |
| Host boot | The control plane relaunches the VMs that were meant to be running (`reconcile.on_host_boot`: `resume`, the default) or leaves them stopped (`stop`). A VM on LVM-thin volumes has them activated and allowed again as it starts. |

To see the VM units:

```bash
systemctl list-units 'glidex-vm@*'
systemctl status glidex-vm@<vm-id>
```

Don't start or stop these units yourself: the control plane does,
through polkit, and restarting a VM is its decision, never systemd's.
Use `gxctl start` / `stop` or the web UI.

`gxctl events <vm>` shows what happened to a VM: starts, exits,
restarts and adoptions.

## Where things are

| Path | What |
| --- | --- |
| `/var/lib/glidex-control-plane/.glidex/glidex.db` | The control plane's database: VMs, networks, projects, users, credentials, tokens. |
| `/var/lib/glidex-control-plane/.glidex/secrets.kek` | The key that seals connector secrets. Without it, sealed secrets in the database can't be opened. |
| `/var/lib/glidex-control-plane/.glidex/images/`, `disks/` | Cloud images, firmware and VM disks: the built-in `local` datastore. |
| `/srv/glidex/<name>/` | Registered [datastores](datastores.md): `disks/`, and on a shared datastore also `images/` (copies for linked disks), `.locks/` and the `.glidex-datastore` marker. |
| `/dev/<vg>/gx-d-<disk-id>`, `gx-i-<image-id>` | Disks and image volumes of [LVM-thin datastores](datastores.md#lvm-thin), in your volume groups. A volume no VM is using may be inactive, with no device node. |
| `/var/lib/glidex-control-plane/.glidex/tls/` | The API's self-signed certificate and key, if you didn't configure one. |
| `/var/lib/glidex-control-plane/.glidex/cluster/` | In a cluster: this node's identity, certificates and Raft log. |
| `/etc/glidex/` | Site configuration: `control-plane.json`, `netd.json`, `stord.json`, `policies/`, `install.conf`, and certificates you put there. |
| `/var/lib/glidex/netd.db` | `glidex-netd`'s state: bridges, uplinks (with the host addresses they moved), NAT networks. |
| `/var/lib/glidex/dnsmasq/` | DHCP leases of NAT networks. |
| `/var/lib/glidex-ui/` | The web UI's self-signed certificate. |
| `/run/glidex-cp/` | Runtime: `api.sock`, `ui.sock`, `tls.crt`, and per-VM directories `vms/<id>/` (sockets, console log, cloud-init seed). |
| `/run/glidex/` | `glidex-netd`'s sockets and vhost-user sockets. |

## Backups

What to keep, on every host:

- `/var/lib/glidex-control-plane/.glidex/glidex.db` and `secrets.kek`,
  together;
- `tls/` (or your own certificates), and `cluster/` on a cluster node;
- `/etc/glidex/`;
- `/var/lib/glidex/netd.db`;
- `images/` and `disks/` if you can't re-create them, the `disks/` of
  each datastore under `/srv/glidex`, and the `gx-d-…` volumes of
  LVM-thin datastores.

These files hold password hashes, token hashes and private keys. Keep
backups encrypted, readable only by administrators, and in your
approved backup system.

### A standalone host

The database is written while the control plane runs, so stop it for a
consistent copy. VMs keep running:

```bash
sudo systemctl stop glidex-ui glidex-control-plane
sudo tar -C /var/lib/glidex-control-plane -czf /root/glidex-$(date +%F).tar.gz \
  --exclude=.glidex/images --exclude=.glidex/disks .glidex
sudo tar -czf /root/glidex-etc-$(date +%F).tar.gz /etc/glidex /var/lib/glidex/netd.db
sudo systemctl start glidex-control-plane glidex-ui
```

Disks of running VMs change while you copy them. Copy a VM's disk while
the VM is stopped, or back up from inside the guest. `gxctl disk show`
gives a disk's datastore and the path of its file.

A disk on an LVM-thin datastore is the raw volume
`/dev/<vg>/gx-d-<disk-id>` on its node. Copy it while its VM is stopped,
with your usual LVM or block-device tools (activate it first with
`sudo lvchange -ay -K <vg>/gx-d-<disk-id>` if it has no device node).
Image volumes (`gx-i-…`) need no backup: Glidex makes them again from
the image.

<!-- TODO(verify): restoring an LVM-thin disk: into a volume of the same name in the same volume group, then whether stord re-owns it at the next activation (it does chown on activate). -->

A disk file on a datastore is named by the disk's id. A linked disk
names its image by a relative path (`../images/<image-id>.qcow2` on a
shared datastore), so copy a shared datastore's `images/` along with
its `disks/`, or restore into the same layout.

To restore, install Glidex, stop the two services, put the files back
(owned by `glidex:glidex`, mode 0600 for files, 0700 for directories),
and start them again.

### A cluster

The cluster's database is replicated on every server. Take snapshots
from a server:

```bash
gxctl cluster snapshot glidex-$(date +%F).gxsnap
```

The file is written 0600 and holds credential and token hashes. A
snapshot is what you recover from if a majority of servers is lost; see
[Clusters](clusters.md#backups-and-lost-quorum). The snapshot holds
records, not disk files. Disks on `local` and other node-local
datastores, LVM-thin included, stay on their nodes: back them up per
node as above. Disks on
a shared datastore are on the shared filesystem: back that up with the
storage system's own tools, or per disk while its VM is stopped.

If a node is lost for good, VMs whose disks are all on shared
datastores can run on another node once it is forgotten; see
[A node is gone for good](clusters.md#a-node-is-gone-for-good). VMs with
disks on the lost node's own storage need those disks restored from a
backup.

## Logs

Each service logs to the journal:

```bash
journalctl -u glidex-control-plane -f
journalctl -u glidex-netd
journalctl -u glidex-ui
journalctl -u glidex-authd
journalctl -u glidex-stord               # LVM-thin volumes, device access
journalctl -u glidex-metad               # the guests' metadata endpoint
journalctl -u glidex-pylond              # Pylon hosts
journalctl -u glidex-vfio-boot -b        # what the boot binding did
journalctl -u glidex-vm@<vm-id>          # one VM's shim and hypervisor
journalctl -u 'glidex-vm@*' --since today
```

More detail: set `RUST_LOG` in a drop-in, for example for the control
plane:

```bash
sudo systemctl edit glidex-control-plane
# [Service]
# Environment=RUST_LOG=debug
sudo systemctl restart glidex-control-plane
```

`glidex-netd`, `glidex-stord` and `glidex-ui` read `RUST_LOG` the same way. Remove it
when you are done: debug logs are large.

Other records:

- A VM's serial console log: `gxctl log <vm>`, or the console page. It
  is kept in the VM's runtime directory and rotated past 16 MiB
  (`console.log_max_bytes`); one older generation is kept.
- The audit log: `gxctl audit`, or the **Audit** page. Kept 90 days
  (`audit.retention_days`).

## Uninstall

From your checkout:

```bash
cargo run -p glidex-install -- uninstall --dry-run
cargo run -p glidex-install -- uninstall
```

It stops every VM (each gets its power button and grace period), the
services and `glidex-netd`, tears down host networking (uplinks give
the host's address back to its NIC), and removes units, binaries and
installer-written configuration. Your data in
`/var/lib/glidex-control-plane`, your files in `/etc/glidex`, Open
vSwitch, `cloud-hypervisor`, `pylon` and `pylond` stay unless you ask:

| Option | Also removes |
| --- | --- |
| `--remove-ovs` | Open vSwitch (packages or the source build in `/opt/glidex`). |
| `--reset-dpdk` | DPDK settings Glidex put in OVS. |
| `--remove-cloud-hypervisor` | The `cloud-hypervisor` binary. |
| `--remove-pylon` | The `pylon` and `pylond` binaries. |
| `--purge-user-data` | `/var/lib/glidex-control-plane` (VMs, credentials, images, disks), `~/.glidex`, and `control-plane.json`, `netd.json` and `policies/` in `/etc/glidex`. |

`-y` / `--yes` skips the confirmation. Back up first if you might want
the data back.

`/srv/glidex` and the [datastores](datastores.md) under it always stay,
with their disks, and so do LVM-thin volume groups and their volumes
(remove `gx-d-…` and `gx-i-…` with `lvremove`); unmount and remove them
yourself. `/etc/glidex/stord.json` stays with your other files there,
unless `--purge-user-data`.

On a cluster node, take the node out of the cluster first
([Remove an empty node](clusters.md#remove-an-empty-node) or
[Detach](clusters.md#detach-a-node-with-its-vms)), then uninstall. The
uninstaller doesn't check: on a host that is still a member it stops
the node's VMs and services like on any host, and the cluster sees the
node as unreachable. Its cluster identity stays in
`/var/lib/glidex-control-plane/.glidex/cluster/` unless
`--purge-user-data`.

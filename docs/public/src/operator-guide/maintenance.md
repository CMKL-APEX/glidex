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
| `glidex-netd` restarts | Keep running. netd restores bridges, NAT and uplinks at start; vhost-user sockets survive in `/run/glidex/vhost`. |
| Host shutdown | Each VM gets its power button and `host_shutdown_grace_secs` (default 60, at most 300) to shut down, then is stopped. VMs stop before netd and Open vSwitch. |
| Host boot | The control plane relaunches the VMs that were meant to be running (`reconcile.on_host_boot`: `resume`, the default) or leaves them stopped (`stop`). |

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
| `/var/lib/glidex-control-plane/.glidex/images/`, `disks/` | Cloud images, firmware and VM disks. |
| `/var/lib/glidex-control-plane/.glidex/tls/` | The API's self-signed certificate and key, if you didn't configure one. |
| `/var/lib/glidex-control-plane/.glidex/cluster/` | In a cluster: this node's identity, certificates and Raft log. |
| `/etc/glidex/` | Site configuration: `control-plane.json`, `netd.json`, `policies/`, `install.conf`, and certificates you put there. |
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
- `images/` and `disks/` if you can't re-create them.

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
the VM is stopped, or back up from inside the guest.

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
[Clusters](clusters.md#backups-and-lost-quorum). VM disks stay on their
nodes: back them up per node as above.

## Logs

Each service logs to the journal:

```bash
journalctl -u glidex-control-plane -f
journalctl -u glidex-netd
journalctl -u glidex-ui
journalctl -u glidex-authd
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

`glidex-netd` and `glidex-ui` read `RUST_LOG` the same way. Remove it
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
vSwitch and `cloud-hypervisor` stay unless you ask:

| Option | Also removes |
| --- | --- |
| `--remove-ovs` | Open vSwitch (packages or the source build in `/opt/glidex`). |
| `--reset-dpdk` | DPDK settings Glidex put in OVS. |
| `--remove-cloud-hypervisor` | The `cloud-hypervisor` binary. |
| `--purge-user-data` | `/var/lib/glidex-control-plane` (VMs, credentials, images, disks), `~/.glidex`, and `control-plane.json`, `netd.json` and `policies/` in `/etc/glidex`. |

`-y` / `--yes` skips the confirmation. Back up first if you might want
the data back.

On a cluster node, take the node out of the cluster first
([Remove an empty node](clusters.md#remove-an-empty-node) or
[Detach](clusters.md#detach-a-node-with-its-vms)), then uninstall.

<!-- TODO(verify): what uninstall does on a host that is still a cluster member (it doesn't check for membership in the code reviewed). -->

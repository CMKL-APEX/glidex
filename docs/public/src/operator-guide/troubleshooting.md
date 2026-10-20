# Troubleshooting

Start with the services and their logs, then look up the error code the
API returned. Most problems are a stopped service, a missing group
membership, a path the `glidex` user can't read, or the host network.

## Check the services

```bash
systemctl status glidex-control-plane glidex-netd glidex-ui glidex-authd.socket
journalctl -u glidex-control-plane -b
```

At start, the control plane logs one line per check. Each should end in
`OK`:

```text
  Checking disk tools... OK
  Loading configuration... OK
  Opening database... OK
  Checking KVM access... OK (VMs open it in their glidex-vm units)
  Loading VMs... OK (2 VMs, 1 running, VM runner: systemd)
  Checking UEFI firmware (Cloud-Hypervisor)... OK (image cloudhv-edk2)
  Checking UEFI firmware (QEMU)... OK (image ovmf)
  Checking networking... OK
  Loading authorization policies... OK
```

| Line | Means | Do |
| --- | --- | --- |
| `Checking disk tools... MISSING (...)` | `qemu-img`, `sgdisk` or another disk tool is missing; image and disk operations fail. | Re-run the installer. |
| `Loading configuration... FAILED` | `/etc/glidex/control-plane.json` has an error or an unknown key. The control plane doesn't fall back to defaults. | Fix the file; the message names the field. |
| `Checking KVM access... MISSING` | No `/dev/kvm`. | See [KVM](#kvm). |
| `Checking KVM access... NO ACCESS` | The `glidex` user isn't in the `kvm` group. | Re-run the installer, which adds it. |
| `Checking networking... LIMITED` | The control plane can only read netd's status: it isn't running as the `glidex` group. | Use the installed unit; don't run the control plane by hand as yourself. |
| `Checking networking... UNAVAILABLE` | `glidex-netd` isn't running or not installed. | `systemctl status glidex-netd`; re-run the installer without `--no-networking`. |
| `Loading authorization policies... FAILED` | A policy file in `/etc/glidex/policies` is invalid or unreadable. | Fix it; the directory must be `root:glidex`, mode 0750. |

Logs of one VM: `journalctl -u glidex-vm@<vm-id>`, and `gxctl events
<vm>` for what Glidex did with it. For log locations and more detail,
see [Maintenance](maintenance.md#logs).

## gxctl can't connect

| Message | Cause and fix |
| --- | --- |
| Permission denied on `/run/glidex-cp/api.sock` | You aren't in `glidex-users` (or `glidex-admin`), or you were added and haven't logged out and back in since. Check with `id`. |
| Connection refused on `https://localhost:8841`, or `unauthenticated` without `--url` | With no local socket, `gxctl` falls back to TCP on this host. The control plane isn't running (`systemctl status glidex-control-plane`), or `/run/glidex-cp/api.sock` is missing. |
| `this host is a cluster agent: it has no local API` | Expected on an agent. Use a server: `gxctl auth login https://<server>:8841`. |
| `unauthenticated` over TCP | No token, or an expired one. `gxctl login --oidc` or `--token`, or `gxctl auth login`. |
| A TLS or certificate error over TCP | See [Certificate fingerprints](#certificate-fingerprints). |

## The web UI

| Symptom | Cause and fix |
| --- | --- |
| `421 Misdirected Request` | The browser used a name the UI doesn't serve. Add it to `GLIDEX_UI_HOSTS` (see [Configuration](configuration.md#web-ui)). |
| `403 origin_not_allowed` | A change came from a page on another site, or a page calls the API on port 8841 directly from an origin not in `auth.allowed_origins`. Pages served by the web UI don't need that list. |
| A page doesn't load after an update | The tab still runs the old UI. Reload it. |

## Error codes

API errors come back as JSON with a code, a message and sometimes
details:

```json
{"error": "quota_exceeded", "message": "…", "details": {"quota": "…"}}
```

`gxctl` and the web UI show the code and message. The common ones:

| Code | Means | Do |
| --- | --- | --- |
| `unauthenticated` | No valid login or token. | Log in again; check the token's expiry. |
| `reauth_required` | The action needs a recent login (cluster changes, installing OVS, uplink and bridge changes, policies). | Log in again and retry. |
| `forbidden` | Your roles or the site policies don't allow it. | Check `gxctl whoami`; ask for a role. See [Access control](access-control.md). |
| `not_found` | No such VM, network, image… in the project you're using. | Check `--project` / your default project (`gxctl project use`). |
| `conflict` | A name is taken, or the resource is in use or busy (a disk claimed by a VM or being moved, a PCI device used by another VM on the same host or sharing its IOMMU group, a DPDK NIC, a kernel image a VM boots, a datastore that still has disks). | Read the message: it names what holds it. |
| `ambiguous` | A name matches more than one thing. | Use the id. |
| `invalid_state` | The VM can't do that now (start a running VM, attach what's already attached). | Check its state with `gxctl get <vm>`. |
| `invalid_config`, `invalid`, `invalid_network`, `invalid_image`, `invalid_disk` | A bad request: wrong path, out-of-range value, unknown network. | The message says which field. |
| `invalid_datastore` | A datastore name or path that isn't allowed: the path must be a directory under `/srv/glidex`. | See [Datastores](datastores.md#preparing-the-directory). |
| `insufficient_storage` (`507`) | A datastore without overcommit lacks the space for a new or moved disk; `details` has `free_bytes` and `needed_bytes`. | Free space, pick another datastore, or allow overcommit; see [Overcommit and capacity](datastores.md#overcommit-and-capacity). |
| `quota_exceeded`, `tenant_quota_exceeded` | The project or tenant would go over a quota. | Raise it (`gxctl project quota`) or free resources. |
| `device_flow_unsupported` | `gxctl auth login --oidc` through a `google` connector that has no device client. | Add a *TVs and Limited Input devices* client to the connector (`--device-client-id`, then `connector secret set <id> device_client_secret`); see [Google](authentication.md#google). |
| `cannot_delete_self`, `last_admin` | Deleting yourself, or the last holder of `system-admin` or `directory-admin`. | Have another administrator do it; give the role to someone else first. See [Removing people](access-control.md#removing-people). |
| `last_identity` | Removing a user's last sign-in method. | Disable the user first, or add another sign-in method. |
| `node_departed` | Giving a role on one host whose node has left the cluster. | Pick a current node (`gxctl nodes`). |
| `managed_by_connector` | The user or team is managed by a SCIM connector or a directory sync: its name, whether it is enabled, or a membership the IdP gave, belongs to the IdP. Also deleting a managed user that is still enabled. | Change it at the IdP, or disable the user first. See [Users an IdP manages](access-control.md#users-an-idp-manages). |
| `not_provisioned` | Making a SCIM token for a connector that isn't provisioned by SCIM. | `gxctl connector update <id> --no-jit --provisioning scim` first. |
| `not_pulled`, `no_source` | Running a directory sync of a connector that isn't `--provisioning pull`, or isn't a Google Workspace connector. | See [Directory sync from Google Workspace](authentication.md#directory-sync-from-google-workspace). |
| `sync_running` | A run of that connector's directory sync is already going. | Wait, then `gxctl connector show <id>`. |
| `login_not_allowed` | The connector's rules refused the login. Through a SCIM connector or a directory sync: the person wasn't provisioned (pulled), or their account is disabled. | Check the user is assigned to the application at the IdP and was pushed; see [Provisioning with SCIM](authentication.md#provisioning-with-scim). |
| `precondition_failed` | Someone changed the resource since you read it. | Read it again and retry. |
| `hypervisor_unavailable` | The VM's hypervisor isn't installed on this host. | Re-run the installer (without `--no-qemu` for QEMU). |
| `hypervisor_error` | The hypervisor refused or failed. | `journalctl -u glidex-vm@<vm-id>`, `gxctl events <vm>`. |
| `tool_unavailable` | A disk tool is missing. | Re-run the installer. |
| `image_error` | Download, checksum or disk operation failed. | The message says which; check disk space under `/var/lib/glidex-control-plane`. |
| `netd_unavailable` | `glidex-netd` isn't reachable. | `systemctl status glidex-netd`. |
| `netd_permission_denied` | netd refused the control plane's request. | Check `/etc/glidex/netd.json`'s `policy`, if you set one. |
| `netd_error`, `unsupported_on_host` | netd failed, or the host can't do it (missing DPDK, AF_XDP, IOMMU). | `gxctl ovs status` lists what is missing. |
| `host_interface_in_use` | The NIC carries the host's address. | Add the uplink with `--migrate-ip`; see [Host networking](host-networking.md#moving-the-hosts-address-onto-the-bridge). |
| `confirmation_required` | The change restarts Open vSwitch or similar. | Repeat with `--force` (or confirm in the UI) when traffic may be interrupted. |
| `migration_rolled_back` | Moving the host's address to a bridge failed the gateway check and was undone. | Fix the network, then `gxctl uplink-reapply`. |
| `not_a_server`, `cluster_unavailable`, `leader_changed` | In a cluster: asked an agent, no server reachable, or no leader right now. | Use a server; check `gxctl cluster status`; retry. |
| `node_unreachable` | The VM's node can't be reached. | Check that node; see [Clusters](clusters.md#a-node-is-down-for-a-while). |
| `gateway_not_ready` | A gateway node doesn't map the edge's physical network or runs `br-int` in userspace. | See [Gateway nodes](host-networking.md#gateway-edge-nodes). |
| `internal`, `persistence_error` | A bug or a storage problem. | `journalctl -u glidex-control-plane`; check disk space. |

## SCIM provisioning

When an IdP can't provision a [SCIM connector](authentication.md#provisioning-with-scim),
its test or logs show the endpoint's answer:

| Answer | Cause and fix |
| --- | --- |
| `401` | A wrong, expired or revoked token, or another connector's. Check `gxctl connector scim-token list <id>`, make a new token, and enter it in the IdP. |
| `403` | The connector is disabled, or isn't `--provisioning scim`. |
| `400 invalidFilter` | The IdP filters on an attribute Glidex doesn't: users by `userName`, `externalId`, `emails`, `displayName`; groups by `displayName`, `externalId`. |
| `409 uniqueness` | A user with that `userName`, or a group with that name, already exists for the connector. |
| `413` | A request body over 1 MiB. |
| `429` | Over 20 requests a second for the connector. IdPs retry after `Retry-After`. |
| The connection fails or times out | The endpoint isn't reachable from the IdP, or the path isn't published; see [Making the endpoint reachable](authentication.md#making-the-endpoint-reachable). |

Every SCIM write is in the audit log as `system:scim:<connector-id>`:
`gxctl audit --limit 50`.

## Certificate fingerprints

Both the API (port 8841) and the web UI (port 5173) use a self-signed
certificate unless you configured one. Before trusting it from another
machine, compare its SHA-256 fingerprint with the one on the host:

```bash
sudo openssl x509 -in /run/glidex-cp/tls.crt -noout -fingerprint -sha256              # API
sudo openssl x509 -in /var/lib/glidex-ui/tls/ui.crt -noout -fingerprint -sha256       # web UI
```

The installer also prints both at the end.

- **The browser's fingerprint differs.** Something between you and the
  host presents another certificate (a proxy, or the wrong host). Don't
  accept it until you know why.
- **`gxctl` says the certificate is not among the profile's pins.** The
  host's certificate changed. Glidex regenerates it only in narrow
  cases (a configured certificate replaced, which is served within a
  minute without a restart; `cluster rotate-ca`; a re-init). Confirm
  the new fingerprint on the host through another
  channel, then:

  ```bash
  gxctl auth login --profile <name> --pin sha256/<new fingerprint> --rebind
  ```

  `gxctl auth trust list` shows a profile's pins; `gxctl auth trust
  fetch` shows what a server presents.
- **The `ts.net` name still gets a certificate warning** with
  `--tailscale-cert` on. The UI serves its usual certificate until
  `tailscaled` hands it one: `journalctl -u glidex-ui` says why (HTTPS
  certificates not enabled for the tailnet, `TS_PERMIT_CERT_UID` not
  `glidex-ui`, the first certificate still being issued). See
  [Tailscale certificates](tls.md#tailscale-certificates).
- **A join says the server's CA is not the one the token names.** The
  cluster CA rotated after the token was issued (for example a server
  left). Issue a new token.

See [TLS certificates](tls.md) to use your own certificates.

## Paths the `glidex` user can't read

The control plane and every VM run as the `glidex` system user, in a
sandbox. Paths you give the API (disk images, kernels, cloud-init files)
are opened by that user, so:

- They must be readable by `glidex`. Your home directory usually isn't,
  and `/home`, `/root` and `/run/user` are hidden from the services
  entirely.
- They must not be under `/tmp` or `/var/tmp`: VMs have a private
  `/tmp`. The API refuses such paths (`… is under /tmp, which VMs cannot
  see`).
- Passing host paths at all needs the `host.paths` permission
  (`system-admin`).

Put such files in a directory of their own, such as `/srv/vm-files`:
`sudo chgrp -R glidex /srv/vm-files && sudo chmod -R g+rX /srv/vm-files`.
Not under `/srv/glidex`: that is where [datastores](datastores.md) are
mounted and registered, and the installer offers what is mounted there
as datastores. Better still, import them as images (a kernel as a
[kernel image](../user-guide/kernel-boot.md#from-host-files)), which
Glidex stores itself.

## KVM

```bash
ls -l /dev/kvm
lsmod | grep kvm
```

- **No `/dev/kvm`.** Turn on virtualization (VT-x / AMD-V) in the
  firmware settings, then `sudo modprobe kvm_intel` or
  `sudo modprobe kvm_amd`. In a VM, nested virtualization must be on in
  the outer hypervisor.
- **`/dev/kvm` exists but VMs fail with permission denied.** The
  `glidex` user must be in the `kvm` group:
  `id glidex`. The installer adds it when the group exists; re-run it,
  then restart the control plane.

## PCI devices

- **`PCI device … is used by VM …` or `… shares IOMMU group … with …`.**
  Another VM on that host holds the device or a device of its IOMMU
  group. Claims are per host: in a cluster, an unpinned VM goes to a node
  where the device is free.
- **A VM with PCI devices waits for a node.** Its page lists each
  node's reason: `no PCI device …` (the node doesn't report it), or the
  VM holding it on every node that has it.

- **A VM's `DeviceHealthy` condition is `False`.** One of its devices
  fell off the bus or had new uncorrectable PCIe errors (event
  `DeviceError`). Glidex leaves the VM running; check
  `gxctl node topology <node> --devices` and the host's `dmesg`, and
  [cordon](host-resources.md#device-health-and-cordoning) the device
  if you don't trust it.
- **A VM's `NumaAligned` condition is `False`.** A `NUMA_POLICY_PREFERRED`
  VM didn't fit on one NUMA node and spans the host.

See [PCI passthrough](pci-passthrough.md#troubleshooting) for host setup
problems (IOMMU, `vfio-pci`, `/dev/vfio` permissions), and [Host
resources](host-resources.md#troubleshooting) for hugepages, NUMA,
dedicated CPUs, device pools and cordoned devices.

## Datastores and disks

| Symptom | Cause and fix |
| --- | --- |
| A node doesn't reach a datastore (`NODES OK` below the node count) | `gxctl datastore show <ds>` gives each node's reason (`OutsideRoot`, `Missing`, `NotADirectory`, `ReadOnly`, `NoMarker`, `ForeignMarker`, `NotEmpty`); see [Each node's status](datastores.md#each-nodes-status). |
| A VM waits for a node: `datastore … is not reachable here` | Its disks are on a datastore that node doesn't reach. Fix the datastore on the nodes it should be on. |
| A VM doesn't start: `disk … is in use elsewhere (… is locked)` (condition reason `DiskBusy`) | Another node holds the disk's [volume lock](datastores.md#volume-locks), for example a node that seemed gone still runs the VM. Find and stop it there; the VM starts once the lock is free. |
| A disk shows `Ready` `False`, reason `MovePending`, or *· moving* | A move is going on. The disk's events say `Moved` when it is done, or `MoveFailed` with the reason; it then stays where it was. While it moves it can't be attached or resized (`409 conflict`), and its VM doesn't start (*disk … is moving to datastore …*). |
| A linked disk waits: `image … has not been copied into datastore … yet` | The disk's worker copies the image into the shared datastore first; it carries on by itself. See [Images in shared datastores](datastores.md#images-in-shared-datastores). |
| `place` answers *Not placed yet* | No node can take the VM yet; the reason follows. The VM's events record `Replacing`. See [Placing a stopped or lost VM again](datastores.md#placing-a-stopped-or-lost-vm-again). |
| A datastore can't be changed or deleted (`409 conflict`) | The message gives the reason: it is `local`, disks are still on it, a running VM with a disk open on a node whose path changes, or a node a disk is bound to. See [Changing and deleting](datastores.md#changing-and-deleting). |

## Kernel boot

| Symptom | Cause and fix |
| --- | --- |
| A kernel image stays `failed` | The extraction failed: its message says why. `gxctl image retry <image>` (or **Retry**) runs it again. The image's events (`GET /images/{id}/events`) record `Retrying` and, once done, `Extracted`. |
| `kernel_from` refused with `400 invalid_image`, or catalog pulls bring no kernel | Extraction is off on this host (`GLIDEX_KERNEL_EXTRACTION=off`), the host isn't x86_64, or it has no Cloud Hypervisor or QEMU. See [Configuration](configuration.md#environment-variables). |
| The first extraction fails to download | It fetches pinned Debian packages over HTTPS from `snapshot.debian.org` once; allow that, or add kernel images from a URL or host files. |
| A VM's `KernelExtracted` condition is `False` (reason `ExtractFailed`, event `KernelExtractFailed`) | Following the guest's kernel failed; the VM boots the kernel it has, and the next start tries again. See [Following the guest's kernel](../user-guide/kernel-boot.md#following-the-guests-kernel). |
| A VM doesn't start: `Ready` `False`, reason `KernelNotExtracted` | A VM that follows its guest's kernel has no kernel to boot yet. Fix the cause in the message, or set `kernel` to a ready kernel image. |
| A VM waits: `kernel … has not been copied to this node yet` (`DisksReady` `False`, `DiskNotReady`) | The node fetches its copy of the kernel image; the VM carries on by itself. |
| `409 conflict` deleting a kernel image | VMs boot it or keep it to go back to; the message names them. A VM-owned kernel image goes by itself once unused (event `Unused`). |

## Networking

Start with what the host can do:

```bash
gxctl ovs status            # netd access, OVS version and state, what is missing
gxctl networks              # networks and their status
gxctl bridges
sudo ovs-vsctl show
```

| Symptom | Check |
| --- | --- |
| The Networking page says `glidex-netd is not reachable` | `systemctl status glidex-netd`, `journalctl -u glidex-netd`. |
| A network stays not ready | `gxctl networks` and the network's status in the UI give the reason. |
| A VM on a NAT network gets no address | dnsmasq is installed (`gxctl ovs status`); a host firewall (ufw, firewalld) may drop DHCP or forwarded traffic: allow the NAT subnet. |
| A VM has an address but no internet | IP forwarding (`sysctl net.ipv4.ip_forward` is 1), the host's own route out, and the firewall's FORWARD policy. |
| A bridged VM gets no address | The bridge has an uplink (`gxctl uplinks <bridge>`), the link is up, and the LAN has a DHCP server (Glidex doesn't run one there). |
| An uplink is held off the bridge after a reboot | The gateway didn't answer at boot. Fix it, then `gxctl uplink-reapply <bridge> <uplink>`. |
| vhost-user NICs fail | The host needs the `dpdk` profile, DPDK initialized and hugepages: `gxctl ovs status`. |
| The installer says `OVS still runs DPDK from an earlier dpdk profile` | The kernel profile turns DPDK off by restarting `ovs-vswitchd`. Re-run with `--allow-ovs-restart`, or with `--ovs-profile dpdk` to keep DPDK. See [Back to the kernel profile](host-networking.md#back-to-the-kernel-profile). |
| The installer says `OVS-DPDK stays on: DPDK ports need DPDK: …` | A DPDK uplink or vhost-user VM NIC is still on OVS. Remove the uplink and stop or move those VMs, then re-run. |
| `br-int` stays userspace (netdev) after going back to the kernel profile | It switches only while no VM port is on `br-int`: stop or move the node's VMs on cluster networks. |
| Cluster VMs on different nodes can't reach each other | UDP 6081 (Geneve) between the nodes' tunnel addresses; the **OVN** card on the Cluster page shows the tunnels. |
| `gxctl cluster status` lists ports that don't answer | Open them; see [Ports between nodes](host-networking.md#ports-between-nodes). |

If the host lost its own connectivity after an uplink change, wait a
minute: an uncommitted address move is rolled back by itself. If you
need to act at the console, `gxctl uplink-rm <bridge> <uplink>` puts the
address back on the NIC.

## Getting help

When you report a problem, include the Glidex version (`gxctl cluster
status` in a cluster, or the control plane's startup banner), what you
ran, the error code and message, and the relevant journal lines. Remove
host names, addresses, user names and anything secret from them first,
and never include tokens, passwords or private keys.

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
| `this host is a cluster agent: it has no local API` | Expected on an agent. Use a server: `gxctl auth login --url https://<server>:8841` (this works on the agent itself). |
| `unauthenticated` over TCP | No token, or an expired one. `gxctl login --oidc` or `--token`, or `gxctl auth login`. |
| A TLS or certificate error over TCP | See [Certificate fingerprints](#certificate-fingerprints). |

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
| `conflict` | A name is taken, or the resource is in use or busy (a disk claimed by a VM, a PCI device used by another VM, a DPDK NIC). | Read the message: it names what holds it. |
| `ambiguous` | A name matches more than one thing. | Use the id. |
| `invalid_state` | The VM can't do that now (start a running VM, attach what's already attached). | Check its state with `gxctl get <vm>`. |
| `invalid_config`, `invalid`, `invalid_network`, `invalid_image`, `invalid_disk` | A bad request: wrong path, out-of-range value, unknown network. | The message says which field. |
| `quota_exceeded`, `tenant_quota_exceeded` | The project or tenant would go over a quota. | Raise it (`gxctl project quota`) or free resources. |
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
  cases (a configured certificate replaced, `cluster rotate-ca`, a
  re-init). Confirm the new fingerprint on the host through another
  channel, then:

  ```bash
  gxctl auth login --profile <name> --pin sha256/<new fingerprint> --rebind
  ```

  `gxctl auth trust list` shows a profile's pins; `gxctl auth trust
  fetch` shows what a server presents.
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

Put such files somewhere like `/srv/glidex/` with
`sudo chgrp -R glidex /srv/glidex && sudo chmod -R g+rX /srv/glidex`. Better
still, import them as images, which Glidex stores itself.

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

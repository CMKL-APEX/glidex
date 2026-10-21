# Host networking

VM networking on a Glidex host runs on Open vSwitch (OVS). The control
plane never touches the host's network itself: it asks `glidex-netd`, a
small root helper, to create bridges, NAT, uplinks and VM ports. This
page covers the host side: installing OVS, choosing a datapath, bridges,
uplinks, NAT networks, and the host's part in cluster networks on OVN.
Creating networks for VMs is in [Networks](../user-guide/networks.md).

Host networking needs the `net-admin` role (or `system-admin`). On the
local socket, members of `glidex-admin` have it.

With `gxctl`, everything here acts on the host whose socket it uses. In
the web UI, each host's page (**Hosts**, then the host) has a
**Networking** section with that host's Open vSwitch, bridges and
uplinks; a server runs them on the node they are for, so an agent's are
managed from any server's UI. A cluster agent has no local API for
`gxctl`. Over the API, a request to a server acts on another node when
it names that node's id in the `X-Glidex-Target-Node` header (see
[REST API](../api/rest.md)); what you may do there follows your roles on
that host. The web UI's **Networking** page keeps the network records
and links to the Host pages.

## Pieces

| Piece | What it does |
| --- | --- |
| `glidex-netd.service` | Root helper. Owns glidex's bridges, NAT (address, nftables, dnsmasq), uplinks and VM ports. State in `/var/lib/glidex/netd.db`, sockets in `/run/glidex/`. |
| Open vSwitch | The switch. Distribution packages, or a pinned source build for DPDK. |
| dnsmasq | DHCP (and DNS) for NAT networks, one instance per network. |
| OVN (clusters) | Cluster networks that span nodes. Installed by default, off until the host is in a cluster. |

`glidex-netd` starts before the control plane. At start it re-creates
glidex's bridges, re-binds DPDK NICs to `vfio-pci`, re-applies uplinks
that carry the host's address, and restores NAT, then reports ready.

The control plane talks to netd's full socket as the `glidex` group. If
it can't, its startup line says `Checking networking... LIMITED` or
`UNAVAILABLE`, and the Networking section of the host's page shows a
warning instead of the OVS status.

## Install Open vSwitch

The installer sets up OVS, `glidex-netd` and the host settings NAT needs
(IP forwarding) unless you pass `--no-networking`. Pick the profile at
install time; it is remembered for later runs:

```bash
cargo run -p glidex-install                        # kernel datapath (default)
cargo run -p glidex-install -- --ovs-profile dpdk  # DPDK: reserves hugepages and PMD cores
```

With `--ovs-profile dpdk`, `--pmd-cpu-mask MASK` sets the OVS-DPDK PMD
CPU mask (hex; `auto` clears it). If OVS already has bridges, the
installer won't restart `ovs-vswitchd` unless you pass
`--allow-ovs-restart`. See [Installation](installation.md) for the other
options.

You can also install or switch the profile on a running host, from the
web UI (the host's page → **Networking** → **Install Open vSwitch** or
**Install / switch profile**) or `gxctl`:

```bash
gxctl ovs status                           # netd access, OVS version, what this host can do
gxctl ovs install --profile dpdk           # --force if it must restart a running OVS
gxctl ovs install --profile dpdk --source  # the pinned source build instead of packages
gxctl ovs dpdk-init --socket-mem 1024 --pmd-cpu-mask 0x6   # enable DPDK in ovs-vswitchd (restarts it)
```

OVS 3.0 or newer is required. Restarting OVS interrupts traffic on its
bridges, so both the UI and `gxctl` ask for confirmation first (`--force`
in `gxctl`).

### Back to the kernel profile

A host that ran the `dpdk` profile goes back with the installer:

```bash
cargo run -p glidex-install -- --ovs-profile kernel --allow-ovs-restart
cargo run -p glidex-install -- --ovs-profile kernel --allow-ovs-restart --release-hugepages
```

The kernel profile runs no DPDK. When the installer finds DPDK on in
`ovs-vswitchd`, it removes the settings the `dpdk` profile set
(`dpdk-init`, `dpdk-socket-mem`, `dpdk-lcore-mask`, `pmd-cpu-mask`,
`pmd-auto-lb`, `userspace-tso-enable`) and restarts `ovs-vswitchd`,
since DPDK can't be stopped in a running one. That gives back DPDK's
PMD cores and socket memory.

- With bridges, the restart interrupts their traffic, so it needs
  `--allow-ovs-restart`. Without it, the installer says what the restart
  would interrupt, leaves DPDK on and finishes the rest of the install.
- It is refused while a DPDK port exists (a DPDK uplink or a vhost-user
  VM NIC), naming it: those would stop working. Remove DPDK uplinks and
  stop or move VMs with vhost-user NICs first.
- The OVS packages stay installed.
- Hugepages stay reserved unless you pass `--release-hugepages`, which
  puts `vm.nr_hugepages` back to its value before Glidex (else 0) and
  removes it from `/etc/sysctl.d/90-glidex.conf`. It goes only with the
  kernel profile, only once DPDK is off, and isn't saved. Hugepages a
  running guest uses go back once freed, or after a reboot.

With DPDK off, a cluster node's `br-int` on **Automatic** goes back to
the kernel datapath within about a minute, but only once no VM port is
on `br-int` (see [The `br-int` datapath](#the-br-int-datapath)).

Switching the profile with `gxctl ovs install` or the web UI doesn't
turn DPDK off; use the installer.

## Datapaths and NIC types

OVS forwards packets either in the kernel (`system` datapath) or in
userspace (`netdev`, with DPDK or AF_XDP). A VM's NIC is either a **tap**
device or a **vhost-user** socket. Not every mix works; `gxctl ovs
status` and the Networking section of the host's page list the combinations and what each is
missing on this host:

| Id | Combination | Needs |
| --- | --- | --- |
| A | kernel datapath, tap VM ports (NAT or kernel uplink) | OVS running; `cap_net_admin` on `cloud-hypervisor` (the installer sets it) |
| B | userspace datapath, DPDK uplink, vhost-user VM ports | the `dpdk` profile, DPDK initialized, IOMMU, hugepages |
| C | userspace datapath, AF_XDP uplink, vhost-user VM ports | as B, plus OVS with AF_XDP support |
| D | userspace datapath, AF_XDP uplink, tap VM ports | OVS with AF_XDP support |

In short:

- **tap** is the default and works everywhere. Use it unless you need
  DPDK.
- **vhost-user** needs the `dpdk` profile. Guest memory is shared with
  OVS automatically. Choose it per network (`--vhost-user`, or **VM
  port: vhost-user (DPDK)** in the network form).

The `dpdk` profile needs the IOMMU on (`intel_iommu=on` or
`amd_iommu=on` on the kernel command line, and VT-d / AMD-Vi in the
firmware). The installer reports a missing IOMMU but doesn't change the
kernel command line.

## Bridges

A NAT or isolated network gets its own bridge, named
`gxbr-<first 8 characters of the network id>` unless you name it. A
bridged network, and an uplink, use a bridge you create first.

Create a bridge in the web UI: the host's page → **Networking** →
**glidex bridges** →
**+ New bridge**, with a name and a datapath:

- **kernel (system)**: for kernel uplinks, and for an OVN gateway's
  physical network.
- **userspace (netdev, DPDK)**: for AF_XDP and DPDK uplinks.

Over the API:

```bash
api -X POST https://localhost:8841/ovs/bridges \
  -H 'Content-Type: application/json' \
  -d '{"name": "gxbr-up", "datapath": "DATAPATH_SYSTEM"}'
```

(`api` is the `curl` wrapper from the [REST API](../api/rest.md) page.)

`gxctl bridges` lists glidex's bridges with their datapath and port
count; one marked `MISSING on host` exists in netd's records but not in
OVS. Glidex only touches bridges it owns, never ones you made with
`ovs-vsctl` yourself.

## Uplinks

An uplink connects a glidex bridge to the physical network through one
of the host's NICs. There are three kinds:

| Kind | Bridge datapath | NIC |
| --- | --- | --- |
| Kernel | kernel (system) | stays a kernel interface (`eno1`) |
| AF_XDP | userspace (netdev) | stays a kernel interface; OVS reads it through AF_XDP |
| DPDK | userspace (netdev) | leaves the kernel: bound to `vfio-pci`, named by PCI address |

Add one from the web UI (the host's page → **Networking** → a bridge →
**Add uplink…**) or
`gxctl`:

```bash
gxctl uplink-add gxbr-up --kernel eno2
gxctl uplink-add gxbr-fast --afxdp enp65s0f0 --xdp-mode native --rxq 4
gxctl uplink-add gxbr-fast --dpdk 0000:41:00.0 --name up0 --rxq 2
gxctl uplinks gxbr-up                # list a bridge's uplinks
gxctl uplink-rm gxbr-up eno2         # remove it; the NIC is restored
```

`--xdp-mode` is `best_effort` (default), `native`,
`native_with_zerocopy` or `generic`. `--rxq` is 1–64. A NIC used by a
DPDK uplink can't also be passed through to a VM.

### Moving the host's address onto the bridge

If the host itself uses the NIC (it carries the host's address or
default route), adding it as a plain uplink would cut the host off, so
it is refused with `host_interface_in_use`. Move the NIC's addresses and
routes to the bridge instead:

```bash
gxctl uplink-add gxbr-up --kernel eno1 --migrate-ip --force
```

In the web UI, tick **The host uses this NIC: move its address and
routes to the bridge.**

The move is checked twice before it sticks:

1. `glidex-netd` snapshots the NIC's addresses and routes, moves them to
   the bridge, and waits up to 20 seconds for the default gateway to
   answer through the bridge. If it doesn't, the NIC gets its exact
   configuration back.
2. The uplink is then `pending_commit`. The client (`gxctl` or the
   page) checks it can still reach the API and commits. Without a commit
   within 60 seconds, netd rolls the change back by itself.

So a mistake costs about a minute of connectivity, not the host. Run it
from a session that can survive a short interruption all the same.

An address from DHCP is refused, because the DHCP client stays on the
NIC and the address may be lost when its lease ends. Give the NIC a
static address first, or pass `--accept-dhcp` (**If the address comes
from DHCP, move it anyway.**) knowingly. DPDK uplinks can't take the
host's address: the NIC leaves the kernel.

### At boot: re-apply or roll back

The host's own network configuration puts the address back on the NIC
at boot. `glidex-netd` then moves it to the bridge again, with the same
gateway check. If moving fails or the gateway doesn't answer, it gives
the NIC its own configuration back and holds the uplink off the bridge,
so the host stays reachable.

A held uplink shows **held off the bridge** on the host's page, with
when and why. `gxctl uplinks <bridge>` shows its phase and the netd log
says `IP migration rolled back at start`. Fix the cause (a cable, a
switch port, the gateway), then re-apply it:

```bash
gxctl uplink-reapply gxbr-up eno1
```

or **Re-apply** on the page. It goes through the same check and commit
as a new uplink.

`gxctl uplink-rm` on an uplink that carries the host's address moves the
address back to the NIC. Networks on that bridge lose their way out.

## NAT networks

A NAT network is a private subnet on its own bridge. `glidex-netd` puts
a gateway address on the bridge, runs dnsmasq for DHCP (and DNS), and
masquerades traffic out through whatever route the host already uses.
It needs no uplink.

```bash
gxctl network-add lab                          # NAT, first free /24 in 10.88.0.0/16
gxctl network-add lab2 --nat --subnet 192.168.50.0/24
gxctl network-add lab-fast --vhost-user        # vhost-user NICs (needs the dpdk profile)
```

- The gateway is the subnet's first address; DHCP hands out the rest.
  The subnet must be a /29 or larger.
- A VM keeps its address across restarts (a reservation per MAC).
- Node NAT networks come from `10.88.0.0/16` by default. To use another
  range, set `nat_supernet` in `/etc/glidex/netd.json` (see
  [Configuration](configuration.md)).
- An isolated network (`--isolated`) has no way out: its VMs reach only
  each other.
- In a cluster with OVN on, `network-add` makes a cluster network unless
  you pass `--node` (a bridge on this host only).

If `ufw` or `firewalld` is active, the host's page warns that it may
drop forwarded NAT traffic. `glidex-netd` adds its own nftables table
(`glidex`) and, where the iptables `FORWARD` or `INPUT` policy would drop
guest traffic, its own chains (`GLIDEX-FORWARD`, `GLIDEX-INPUT`). If
guests still get no address or can't get out, allow the NAT subnets in
your firewall.

## Bridged networks

A bridged network puts VMs straight on a glidex bridge that has an
uplink, so they appear on the physical LAN:

```bash
gxctl network-add lan --bridged gxbr-up            # optionally --vlan 20
```

The bridge must be a glidex bridge. Glidex doesn't hand out addresses on
a bridged network: the LAN's DHCP server, or a static address in the
guest, does.

## Clusters: OVN on the host

In a cluster with OVN on, *cluster networks* span every node: each node
is an OVN chassis, VMs on different nodes reach each other through
Geneve tunnels, and NAT cluster networks leave through a shared edge on
gateway nodes. Turning OVN on and setting the edge is in
[Clusters](clusters.md#cluster-networks-on-ovn). Each node also has
host-level settings, on the node's page (**Cluster** → a node):

### Physical networks

A physical network is a name (for example `uplink`) for a LAN that some
nodes reach. On each node, map the name to the glidex bridge that has
the uplink to that LAN. Provider networks (bridged cluster networks) and
the shared edge use these names.

On the node's page, **Physical networks** → **Change…**, add rows of
*physical network* → *bridge*. It applies within half a minute, without
a restart. You can also set them in the host's
`/etc/glidex/control-plane.json` (read when the control plane starts);
a mapping set in the UI wins over the file's:

```json
{
  "cluster": {
    "ovn": {
      "bridge_mappings": { "uplink": "gxbr-up" }
    }
  }
}
```

The scheduler places a VM on a provider network only on nodes that map
its physical network.

### The `br-int` datapath

OVN's integration bridge, `br-int`, carries every VM port on cluster
networks. On the node's page, **br-int datapath** → **Change…**:

| Setting | Use |
| --- | --- |
| Automatic | Userspace (DPDK) where OVS runs DPDK, else the kernel datapath. |
| Kernel (system) | What a gateway node needs. VMs here get tap NICs on cluster networks. |
| Userspace (netdev, DPDK) | For vhost-user NICs on cluster networks. Needs DPDK; can't carry a gateway. |

The node switches within half a minute, but only while no VM port is on
`br-int`: stop or move the node's VMs on cluster networks first. After
going [back to the kernel profile](#back-to-the-kernel-profile),
**Automatic** switches to the kernel datapath the same way. A VM
with a vhost-user NIC on a cluster network is placed only on nodes whose
`br-int` is userspace.

### Gateway (edge) nodes

The shared edge, and each project router with its own outside address,
run on gateway nodes. A gateway node must:

- map the edge's physical network to a bridge with an uplink to that
  LAN, and
- have `br-int` on the kernel datapath.

Use a **kernel (system)** bridge for that physical network. Glidex
refuses to name a node a gateway that doesn't meet these.

### Ports between nodes

The installer doesn't touch the firewall. Between cluster nodes, allow:

| Port | Between | For |
| --- | --- | --- |
| TCP 8842 | every node | the node API |
| UDP 6081 | every node (tunnel addresses) | Geneve tunnels |
| TCP 6641, 6642 | every node to the servers | OVN northbound and southbound databases |
| TCP 6643, 6644, 6648 | servers only | OVN's own Raft groups and `ovn-northd` |

`gxctl cluster status` tests the TCP ports from a server and lists any
that don't answer. It can't test UDP 6081.

## Removing networking

`cargo run -p glidex-install -- uninstall` tears down glidex's host networking with
netd's own code: uplinks put the host's address back on the NIC and
restore NIC drivers, and NAT, bridges and VM ports go. OVS itself stays
unless you pass `--remove-ovs`. See [Maintenance](maintenance.md#uninstall).

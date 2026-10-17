# Networks

A network connects VM NICs. Glidex creates no network by itself: an
administrator creates host networks and offers them to projects, and a
project's owner can create networks of the project's own. A VM gets one
NIC per network it attaches to.

This page is the user's view. Setting up the host side (Open vSwitch,
bridges, uplinks, OVN) is in
[Host networking](../operator-guide/host-networking.md).

## Network modes

| Mode | What VMs get | Addresses |
|---|---|---|
| **NAT** | A private IPv4 subnet behind the host. VMs reach the outside through the host (masquerade); nothing outside can open a connection to them. | DHCP from Glidex. |
| **Isolated** | VMs on the network reach each other and nothing else, not even the host. | None on a single host (configure them in the guest); DHCP on a cluster network. |
| **Bridged** | VMs sit on an existing LAN through one of the host's bridges, as if plugged into it. | From the LAN (its own DHCP). |

NAT networks can't reach each other, and a guest reaches the host only
for DHCP, DNS and ping on its gateway. Glidex has no port forwarding: to
reach a VM from outside, put it on a bridged (or, on a cluster, provider)
network.

A VM's NIC is a **tap** device (the default) or a **vhost-user** port,
which needs OVS-DPDK on the host. The network decides which.

## Where a network lives

On a single host, a network is an Open vSwitch bridge on that host.

On a cluster, a network is either:

- a **cluster network** (on OVN), which spans every node: VMs on any
  node can use it, and traffic between nodes goes through tunnels. Needs
  OVN turned on for the cluster. A NAT cluster network reaches the
  outside through the cluster's shared **edge**, which an administrator
  sets up first;
- a **node network**, a bridge on one node: only VMs placed on that node
  can use it, and the scheduler places them there.

On a cluster with OVN on, new networks span the cluster unless you ask
for a node network. The cluster's bridged networks are **provider**
networks: they sit on a named physical network that the nodes map,
optionally on a VLAN.

## Who can use a network

- A **host network** has no project. An administrator (`net-admin`)
  creates it and grants it to projects, to whole tenants, or to every
  project.
- A **project network** belongs to a project. Its owner creates it (NAT
  or isolated only) and can share it with other projects. It counts
  against the project's network quota.

To attach a VM to a network you need the `editor` role in the VM's
project, and the network must be usable by that project.

```
gxctl> networks                     # host networks, and project networks you can see
gxctl> network show lab             # id, mode, scope, bridge, phase, grants or shares
```

In the web UI, the **Networking** page lists the networks the selected
project can attach VMs to (with host rights, every network and the
host's bridges). Its **Status** column shows **ready**, or what the
network is waiting for.

A network is **pending**, **ready**, **degraded** (part of it is missing
on the host; Glidex restores it), or reports that the host's network
helper is unavailable.

## Create a project network

You need the `owner` role in the project.

### Web UI

On the project's page, under **Project networks**, click **Create a
project network**, or click **Add Network** on the **Networking** page
and pick **Project &lt;name&gt;** under **Network for**. Then:

- **Spans** (clusters with OVN on): **Every node (cluster network on
  OVN)** or **This node only (a bridge here)**.
- **Name**: lowercase letters, digits and `-`, at most 32 characters.
- **Mode**: **NAT (DHCP + masquerade)** or **Isolated (VM-to-VM)**.
- **VM port**: **tap** or **vhost-user (DPDK)**.
- **Subnet (optional)**: for NAT (and isolated cluster networks). On a
  single host, the default is the first free `/24` in `10.88.0.0/16`;
  on a cluster, a free range from the cluster's pool.
- **Leaves through** / **Routed through**: a router of the project, for
  a cluster network. See [Project routers](#project-routers-clusters).

### gxctl

```
gxctl> network create lab --project my-project                 # NAT
gxctl> network create backend --project my-project --isolated
gxctl> network create lab2 --project my-project --subnet 10.88.42.0/24 --mtu 1450
```

`--vhost-user` makes its NICs vhost-user ports.

### API

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X POST https://host.example.org:8841/projects/my-project/networks \
  -d '{"name": "lab", "mode": "nat"}'
```

See [REST API](../api/rest.md#networks).

## Create a host network

Host networks need the `net-admin` role. In the web UI, click **Add
Network** on the **Networking** page and pick **Host network**; in
`gxctl`:

```
gxctl> network-add lab                       # NAT, default subnet
gxctl> network-add lab --subnet 10.88.7.0/24
gxctl> network-add backend --isolated
gxctl> network-add lan --bridged br-lan      # on an existing glidex bridge with an uplink
gxctl> network-add lab --cluster             # or --node, on a cluster
```

`--vlan <n>` puts a network on a VLAN, `--vhost-user` makes its NICs
vhost-user ports, and `--bridge <name>` names the bridge of a NAT or
isolated network. A bridged network needs a glidex bridge with an
uplink first, and a cluster provider network a physical network the
nodes map: see [Host networking](../operator-guide/host-networking.md).
Provider networks are created from the web UI (**Provider (a physical
network)**, with its **Physical network** and **VLAN**) or the API.

Then grant it. The lists replace the current grants:

```
gxctl> network grant lab --project my-project --project other-project
gxctl> network grant lab --tenant acme
gxctl> network grant lab --all-projects
```

## Share a project network

A project's owner offers a share to another project, whose owner
accepts it within 7 days. Then the other project's VMs can attach to the
network.

```
gxctl> network share lab <other-project-id>        # offer
gxctl> network shares other-project                # offers and shares of a project
gxctl> network accept other-project lab            # accept, as the other project's owner
gxctl> network leave other-project lab             # stop using it
gxctl> network unshare lab <other-project-id>      # withdraw an offer or a share
```

In the web UI, the network's row on the project page has **Offer
share**; the other project's page lists it under **Networks shared with
this project**, with **Accept** and **Leave**.

## Attach VMs

Pick the networks when you create the VM: the **Networks** checkboxes
in the web UI, the `Networks` prompt in `gxctl create`, or `networks` in
the API. One NIC per network, in order, at most 8. A network named
`default`, if the project can use one, is preselected.

To change a VM's networks later, `PATCH` its `networks` (the whole list);
it applies at the VM's next start. See
[Virtual machines](vms.md#change-a-vm).

## Addresses

- **NAT** (single host): the gateway is the subnet's `.1`, and each NIC
  gets a fixed address from the rest of the subnet, by its MAC. The NIC
  keeps it across restarts until the VM is deleted. The gateway also
  answers DNS.
- **Cluster networks** (NAT and isolated): Glidex reserves an address
  for each NIC from the network's subnet and serves it by DHCP. A NIC can
  only send from its own MAC and address (on provider networks, its own
  MAC).
- **Bridged and provider**: the LAN's DHCP server, or static addresses
  in the guest.
- **Isolated** (single host): no DHCP; configure addresses in the guest.

NAT networks are IPv4 only. The address of each NIC shows in the VM's
details: **Network** on its page in the web UI, `NIC <n>` in
`gxctl get <vm>`, `nics[].ipv4` in the API.

## Delete a network

A network can't be deleted while any VM uses it, even a stopped one.

```
gxctl> network rm lab          # or network-rm lab; waits up to a minute
```

In the web UI, **Delete** on the network's row (project page) or on the
**Networking** page.

## Project routers (clusters)

On a cluster with OVN on, a project's NAT cluster networks leave
through the cluster's shared **edge**, each on its own: they can't reach
each other. A **router** of the project changes that for the networks
attached to it:

- they reach each other through the router;
- with an outside address (an **external** router), they leave with
  that address instead of the edge's. Without one (internal only), they
  reach each other and nothing outside;
- an isolated network attached to a router gets a gateway (the subnet's
  `.1`) and is routed like a NAT one. Running VMs learn it when their
  DHCP lease renews, or when they restart their network or reboot.

Only NAT and isolated networks that span the cluster can go on a router;
node networks can't. VMs keep their addresses when their network moves
to or from a router, but connections open through the old way out are
cut.

Routers are managed on the project's page, in the **Routers** card (it
shows only with OVN on), with the `owner` role:

1. Click **New router**, give it a **Name**, and tick **Reaches outside,
   with its own address** for an external router (possible once an
   administrator gave the edge a pool of addresses for routers). An
   external router counts against the project's external IP quota.
   Administrators can also choose its **Address** and **Gateway nodes**.
2. On the router's row, **Attach network…** puts existing networks on
   it, and **+ network on &lt;router&gt;** creates a new one there.
   **Detach** puts a network back on the edge.
3. **Delete** removes a router once no network is attached.

There are no `gxctl` commands for routers. With the API:

```bash
api=https://host.example.org:8841
auth="Authorization: Bearer $GLIDEX_TOKEN"
curl -H "$auth" -H 'Content-Type: application/json' -X POST $api/projects/my-project/routers \
  -d '{"name": "r1", "external": true}'
curl -H "$auth" -H 'Content-Type: application/json' -X PUT $api/projects/my-project/networks/lab/router \
  -d '{"router": "r1"}'                  # {} puts it back on the edge
curl -H "$auth" -X DELETE $api/projects/my-project/routers/r1
```

# Host page and the UI on agents (design)

Status: **design**. Every host gets a **Host page** in the web UI. It
gathers what belongs to that one host: its Open vSwitch, bridges and
uplinks, PCI devices, topology, and maintenance. The page works on a
standalone host and, in a cluster, from any server's UI for any node. An
agent no longer serves the UI itself: it sends the browser to a server.
This replaces the agent relay of [clustering-ui.md](clustering-ui.md) §3.6
for browsers.

## 1. Why

- An agent's UI can't sign people in. Its control plane relays each
  request to a server (clustering-ui.md §3.6). But an SSO login sets its
  state cookie on the agent's origin, while the IdP returns the browser
  to a server's origin (the first `auth.allowed_origins` entry). So the
  login can't finish. Making it work would mean registering every
  agent's address with every IdP. Passkeys and the origin checks on API
  calls depend on the same single origin.
- On a server's UI, the host-only operations always mean the server's
  own host. These are `HostNetworkService` (OVS, DPDK, bridges,
  uplinks), PCI devices, reconcile and leave (`relay::is_remote_host_path`).
  So without an agent's own UI, nobody can manage an agent's host
  networks.
- A compromised agent sees every password typed into its UI (§3.6
  "Trust"). With the redirect, nothing is typed there.

## 2. Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| W1 | Host page | `/hosts/:id`, one page per node (on a standalone host, the one node). It has these sections:<br>• **Overview**: capacity, topology, versions, and the node record in a cluster.<br>• **Networking**: OVS and DPDK status, install, bridges, uplinks, and the physical networks.<br>• **Devices**: PCI devices, cordoning, SR-IOV.<br>• **Maintenance**: reconcile, and leave on a cluster member.<br>`/cluster/nodes/:id` redirects to it, and the cluster-only sections (OVN, tunnels, membership actions) move with it and show only in a cluster. The menu has **Host** on a standalone host and **Hosts** in a cluster (the node list). The Networking page keeps the network records, which belong to the whole cluster, and links to each host for its bridges and uplinks. | One place per host, the same standalone or clustered; nothing about a host is reachable only from that host. |
| W2 | Choosing the host | Host-only calls carry `X-Glidex-Target-Node: <node id>`. On a server, a request naming another node has its route's action authorized on that node's `Host` (`Ent::host_of`), then runs there over the node link (`relay::run_on_host`, server certificate), as relayed requests do now. A request naming the server itself, or naming no node, runs locally. Anywhere else, the header is refused with `400 bad_target_node`: on a path that isn't host-only, or naming an unknown or departed node, or on a standalone host naming any node but itself. Per-host roles (tenancy.md, #60) already decide who may act on which host. | It reuses the relay's checked path. The browser never talks to an agent. |
| W3 | Where each server's UI is | Each server publishes its web UI's address as `status.ui_url` in its node record, which replicates to every node. It is the first of these that is set:<br>1. **`cluster.ui_url`** in the server's `control-plane.json`: a custom address, e.g. a **load balancer** in front of the servers (`https://glidex.example.com`). Servers behind one load balancer all set the same value.<br>2. The address its glidex-ui **reports** (§3): the Tailscale name when the Tailscale certificate is on, else the first name in `GLIDEX_UI_HOSTS`, else the host's FQDN, with the port it listens on.<br>3. The first `auth.allowed_origins` entry.<br>Agents publish none. | No setting is needed in the common case, and a site with a load balancer names it once per server. |
| W4 | A custom address is trusted | A server with `cluster.ui_url` also accepts it as an origin, alongside `auth.allowed_origins` (computed or set). Its OIDC callback defaults to it (`redirect_uri` unset), and its glidex-ui accepts its host as a `Host` value (§3) besides `GLIDEX_UI_HOSTS`. | Behind a load balancer, logins and API calls arrive with the load balancer's origin and Host. Every server must take them, and the IdP needs one callback for all of them. |
| W5 | The agent redirects | An agent's glidex-ui answers a browser's `GET`/`HEAD` with `307` to a server's `ui_url`, keeping the path and query. The path `/` goes to `/hosts/<agent id>`, so opening an agent's address lands on its Host page. The server is the one the agent's node link reached last, then the others in order; with `cluster.ui_url` they are usually one address. An `/api` request on the agent gets `421 use_a_server`, with `location` set to that URL, and is never relayed. When the agent knows no server's address, it serves a small page with `503`, listing the servers' advertise addresses. It never asks for a login. | One origin for everything: SSO, passkeys and cookies work, and no password is typed into an agent. |
| W6 | Older agents | Servers keep the `/cluster/v1/ui/…` relay route, so agents on an older build keep working during a rolling upgrade. `cluster.ui_relay` stays accepted, but it is ignored and documented as replaced. The route is removed in a later release. | Upgrades go server-first, then agents (cluster-runbook.md). |
| W7 | Out of scope | `gxctl --node` for host-only commands; an agent's API socket stays as it is (C D14, gxctl logs in to a server). | It follows from W2 later, without new server work. |

## 3. glidex-ui and its control plane

glidex-ui reports itself on `ui.sock`, at start and every 30 seconds:

```
PUT /ui/self {"url": "https://gpu-dev2.example.ts.net:5173"}
→ 200 {"redirect": "https://glidex.example.com",   // only on an agent
       "home": "/hosts/prism1",                     // with redirect
       "extra_hosts": ["glidex.example.com"],       // W4, on a server
       "servers": ["10.0.0.11:8842", …]}            // for the 503 page
```

- The control plane accepts it only from the `glidex-ui` peer
  (`ui_peer_ok`), as for every `ui.sock` request. It is not an API route
  (no user, no audit). It keeps the reported URL in memory. A server
  writes `status.ui_url` (W3) through its node self-report
  (`VmManager::report_self`), and only when the URL changes.
- glidex-ui keeps the last answer. If the control plane is unreachable,
  it keeps using that answer, and a host that never got one serves
  normally (a standalone host, or a node before it joins). A host that
  joins or leaves a cluster gets the new answer within 30 seconds.

## 4. Who sees what

- **Hosts list and pages:** they are shown with `listNodes`; a
  standalone host's one page is shown with `readHostTopology` or
  `readOvsStatus` on `Host`.
- **Sections:** each section asks `POST /authz/allowed` for its actions
  on that page's host (`hostOf`), and is hidden when none is allowed,
  as on the Node page today.

## 5. Uplinks on another host

An uplink that moves a host's address is kept only if it is committed
after the move (networking.md §8.5). On another node, the commit travels
over the node link, which uses the node's advertise address. If the move
breaks that address, the commit can't arrive, and netd rolls the move
back, which is the safe outcome. The page says this when the uplink's
NIC carries the node's advertise address.

## 6. Tests

- **Unit:**
  - W3's order;
  - W2's refusals (an unknown node, a path that isn't host-only, a
    standalone host naming another node);
  - glidex-ui's redirect, including the path mapping, `421` on `/api`,
    the `503` page, and keeping the last answer;
  - W4's extra origin and Host.
- **Integration:**
  - **Two nodes:** with a server and an agent in-process, a server's UI
    request naming the agent runs on the agent, authorized on the
    agent's `Host`;
  - **Role and URL:** a user holding a role on the server's host only is
    refused on the agent; `ui_url` replicates to the agent.
- **UI e2e:**
  - **Standalone:** the Host page's Networking and Devices sections;
  - **Mocked two-node directory:** the Hosts list, and requests carrying
    the target node.
- **On the host pair** (gpu-dev2 server, prism1 agent):
  - opening prism1's UI lands on gpu-dev2's `/hosts/prism1`;
  - SSO works there;
  - prism1's bridges are listed from gpu-dev2.

## 7. Tasks

1. This spec; clustering-ui.md §3.6 and §5.2 point here.
2. **Server:**
   - W2: the target header on direct requests;
   - W3 and W4: `status.ui_url`, `cluster.ui_url`, the extra origin and
     the OIDC callback default;
   - `PUT /ui/self`;
   - tests.
3. **glidex-ui:** reporting itself, the redirect, `421`, the `503` page,
   and the extra Host values (W5).
4. **UI:**
   - the Host page and the Hosts list (W1), with the target header on
     host-only calls;
   - the Networking page's host parts move there;
   - `/cluster/nodes/:id` redirects;
   - web-ui.md routes;
   - e2e.
5. **Docs:** the operator guide's clustering and web UI pages
   (`cluster.ui_url`, load balancers, opening an agent's address).

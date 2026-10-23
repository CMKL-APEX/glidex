# Workload identity

Status: **draft**; WI0, WI1a and WI1b-1 implemented (§13, §15). Extends
[tenancy.md](tenancy.md) (§7 tokens, §9 workload federation, T6),
[security.md](security.md) (§7 Cedar) and [networking.md](networking.md).
§11 lists the changes other documents need once this is accepted.

Glidex already trusts identities from **outside**: a tenant's workload
federation (tenancy.md §9, T5) trades a CI job's JWT for a short-lived
token acting as a service account, so the job stores no glidex secret.
Glidex gives **its own VMs** no identity. Software in a guest that has to
call glidex (a Kubernetes node joining a cluster, a storage driver, a
backup agent, an AI agent) or anything else (object storage, Vault, a
cloud provider) needs a long-lived secret placed in the guest: in its
cloud-init user-data, so in the seed image and on its disk, valid until
someone revokes it.

This document makes glidex an **identity provider for its VMs**: a guest
asks a local endpoint for a short-lived, audience-bound token that says
which VM it is, in which project and tenant; glidex itself and outside
systems verify it against the published keys. A VM with a **service
account** attached can also get glidex API tokens for that account, with
nothing stored in the guest. It comes before managed applications
([apps.md](apps.md)) so that none of them ships secrets into
guests.

| | Today | With this design |
|---|---|---|
| A guest calling the glidex API | An API token (`gxt_`) placed in the guest by hand or in user-data, long-lived | An access token for the VM's service account from the metadata endpoint, ≤ 1 h, rotated by asking again |
| A guest calling an outside service | That service's own long-lived secret | A JWT for that service's audience; the service trusts the directory's issuer (OIDC discovery, JWKS) |
| Which VM made a call | Unknown: the token is the caller | The VM, its project and tenant, in the token and in glidex's audit log |
| Revocation | Revoke the token everywhere it was copied | Detach the service account or delete the VM: no new tokens; existing ones expire within their TTL |

## 1. Scope

- **In:** the directory as the issuer of workload tokens (keys, OIDC
  discovery, JWKS); the guest metadata endpoint over the network and
  over vsock, served by a per-node `glidex-metad`; the token's claims;
  attaching a service account to a VM, and who may; glidex API tokens
  for that service account; exchanging a VM's token at
  `POST /auth/exchange`; audit; how it works before and after T6; the
  token model's room for delegation (§8).
- **Later, designed here:** delegated tokens for agents acting on behalf
  of a user (§8, milestone WI3).
- **Out:** identities for processes or containers inside a guest (a
  Kubernetes cluster gives its pods their own, which it can federate
  through T5); a resident guest agent (the vsock helper is a client
  run on demand); hardware attestation (TPM, SEV-SNP, TDX);
  secrets storage. User-data is not a secret store, and nothing here
  makes it one.

## 2. Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| W1 | Trust domain | Workload tokens are issued by the VM's **directory**, under its issuer URL (tenancy.md D27: `https://<host>/d/<directory>`), with its signing keys. A cluster is never an issuer. | One stable issuer per organization, the same one its people log in with (T6, D11). Outside systems trust it once, however many clusters run the tenant's VMs; an issuer URL written into customers' AWS or Vault configuration can't be moved later. |
| W2 | Who vouches for the VM | The **cluster** running the VM is the only one that knows which VM asks; it attests, the directory's key signs. Before T6 (one control plane, directory `local`) that is one process. With T6 the directory gives each cluster it trusts a **delegated signing key** (§6) that it lists in its own JWKS, so a cluster mints tokens while cut off from its directory, and a relying party still trusts the directory alone. | Clusters keep running workloads through a partition (tenancy.md T6 staleness); the directory stays the single root and can withdraw a cluster's key at once. |
| W3 | Delivery | A **link-local metadata endpoint** at `169.254.169.254:80` on every network a VM has, answered by a per-node daemon, **`glidex-metad`** (W11). It identifies the VM by the **port** a request arrives on, never by a source address alone. Requests need the header `Metadata-Flavor: Glidex`; replies leave with IP TTL 1. vsock is a second channel (W12). | Works from any cloud image with `curl`, on every network the VM has, with no agent. The port is what OVN and the node bridge already bind to the VM, so a guest can't claim to be another VM; the header and TTL defeat request forgery (SSRF) through a proxy in the guest and forwarding beyond it. |
| W4 | Every VM has an identity | Any running VM may fetch its **identity token** (§4.2) for any audience; it grants nothing in glidex by itself. Rights come only from an attached service account (W5). | Identity is safe to give: an outside service decides what a VM's identity may do, as with any OIDC issuer. |
| W5 | Service account on a VM | A VM may name **one service account** (`service_account`: a `SERVICE_ACCOUNT` token of a project of the VM's tenant). Setting it needs `useServiceAccount` on that service account's project (§7), not only `updateVm`. It can change while the VM runs and applies to the next token. | Without its own action, anyone who can create VMs could act as every service account in reach (the escalation GCP guards with `iam.serviceAccountUser`). |
| W6 | Glidex tokens for the VM | `GET /v1/glidex/token` on the metadata endpoint returns an **access token for the cluster** (`aud` = cluster id, tenancy.md §7.3) acting as the VM's service account: a federated token (D48) with origin `vm`, TTL 15 min by default, at most 1 h, never past the service account's own expiry. No service account, no token (`404`). | The guest never holds a `gxt_` secret; rights stay on the service account's role links, as for T5. |
| W7 | Outside relying parties | The directory publishes `/.well-known/openid-configuration` and its JWKS (already needed for T6 logins). An identity token's `aud` is what the guest asks for; there is no default audience. | AWS IAM OIDC providers, Vault's JWT auth, Google workload identity federation and Kubernetes accept any issuer that publishes these. |
| W8 | Lifetimes and revocation | **Identity tokens 15 min** by default; a guest may ask for 10 min to 1 h. **Glidex tokens 15 min** (T5's default, D48), at most 1 h. `glidex-metad` hands out the same token until 80% of its lifetime has passed, then a new one, so a guest that asks again always holds one with time left (§4.4). Deleting or stopping the VM, detaching or revoking its service account, or (T6) withdrawing the cluster's key, stops new tokens at once; issued tokens live until `exp`. | Short lifetimes are the revocation; relying parties need no revocation list. Shorter than GCP's and Kubernetes' 1 h because asking is local and cheap; longer than SPIRE's 5 min JWT-SVIDs so a guest that caches naively doesn't fail; well short of Azure's 24 h tokens that can't be revoked (§4.4). |
| W9 | Claims | `sub` is a SPIFFE ID, `spiffe://<trust domain>/tenant/<t>/project/<p>/vm/<vm id>` (trust domain: W13); glidex-specific claims sit under one `glidex` object (§4.2). | SPIFFE JWT-SVIDs require a SPIFFE ID as `sub`, so SPIFFE-aware software accepts the token as is (tenancy.md §20); one namespace keeps claims from colliding with relying parties' own. |
| W10 | Delegation (agents) | Tokens and audit records carry an **actor chain** from the start: a token obtained by an agent acting for a user names the user as `sub` and the agent in `act` (RFC 8693 §4.1). Issuing such tokens is milestone WI3 (§8). | Retrofitting "who acted for whom" into a token format and the audit later is a breaking change; carrying the field costs nothing now. |
| W11 | `glidex-metad` | One **per-node daemon** serves the endpoint for every VM on its node. **netd** steers `169.254.169.254:80` from each VM port to it and tells it which port is which VM (§7.1). It never signs: it asks the control plane, authenticated as its node, to mint for a VM, and the control plane refuses a VM that isn't running **on that node**. | netd already owns every VM port and the node's datapaths, so steering stays where networking is (security.md §8) and metad needs no network privileges. One daemon per node, not per VM. A compromised node can mint only for the VMs it runs, which it controls anyway. |
| W12 | vsock | A **second channel**: the same HTTP API over virtio-vsock, for VMs with no network, or whose networks the guest firewalls off. Every VM gets a vsock device; the guest connects to the host (CID 2) on a fixed port. The VM is known from the device the connection came through, with no address at all (§7.2). Needs a vsock-capable client in the guest (`socat`, or a small `glidex-meta` helper), so the network endpoint stays the default. | Isolated and network-less VMs still get identity, and an identity that depends on no network setting is the strongest binding glidex can offer. |
| W13 | Trust domain | Each directory has a **trust domain**, a SPIFFE name with no path: by default `<directory slug>.<issuer host>` (`local.glidex.example.org`); a directory owner may set one under a domain the organization owns (`acme.com`, `glidex.acme.internal`) when the directory is created. It is **fixed once the first token is issued**. It need not resolve in DNS. A directory whose issuer has no public URL still issues tokens: glidex verifies them itself, and relying parties that can take keys directly (Vault's static JWKS, Kubernetes) are given the JWKS; those that fetch discovery themselves (AWS) need a public issuer. | SPIFFE trust domains carry no path, so hosted directories sharing a host (D27) need distinct names; every `sub` embeds it, so changing it would break every relying party's rules. |

## 3. Relation to tenancy (T5, T6, T7)

This design takes T6's direction instead of competing with it.

- **The directory stays the identity provider** (D11, D27). People get ID
  and access tokens from it (T6 §7); VMs get workload tokens from it.
  Both use the directory's signing keys and the same JWKS. Building the
  issuer here (milestone WI0) is work T6 needs anyway.
- **T5 federation is reused, not duplicated.** A VM's glidex token (W6)
  is a federated token (D48) whose origin is the VM instead of an outside
  issuer: the same record, principal (the service account's `Token`),
  TTL rules and audit. A directory trusts its own workload tokens
  without a federation record; another directory's can be trusted
  through an ordinary T5 federation (its issuer is a public `https` URL).
- **Audience binding (§7.3) holds:** glidex tokens are addressed to one
  cluster; identity tokens to the audience the guest names, never to a
  directory or a cluster unless asked, and glidex refuses an identity
  token presented as a bearer token.
- **Service accounts stay directory records** (§7.4). Attaching one to a
  VM is a reference from the cluster's VM record to the directory's
  token; with T6 the service account must list the VM's cluster in its
  `clusters`, or attaching it is refused.
- **Admission (D9):** a cluster mints for a VM only while the VM's
  tenant is admitted there; a suspended tenant's VMs get no tokens.
- **Fleets (T7)** pin a directory's long-term key (D36). The token
  signing keys and the clusters' delegated keys are short-lived keys
  that long-term key endorses, so rotating them is routine and never a
  D36 key change (§6).
- **Hosted directories (D27):** the host can mint any hosted
  directory's workload tokens, as it can already read its records. An
  organization that won't accept that runs its own directory.
- **Cluster operators** can always act as the VMs they run (they run
  them). Tokens carry `glidex.cluster`, so a relying party that trusts
  only some clusters can say so, and with T6 a directory withdraws a
  cluster's delegated key.

## 4. Tokens

### 4.1 Issuer and keys

- `iss`: the directory's issuer URL. Discovery at
  `<issuer>/.well-known/openid-configuration` (issuer, `jwks_uri`,
  `id_token_signing_alg_values_supported: ["ES256"]`); JWKS at
  `<issuer>/jwks.json`.
- Keys: ES256, rotated every 30 days, the previous key published until
  every token it signed has expired; private keys sealed with the
  directory's data key (tenancy.md §13), like connector secrets.
- Before T6 there is one directory, `local`; its issuer URL is the
  control plane's public URL plus `/d/local`, so it doesn't change when
  T6 arrives. An install that has no public URL configured serves
  identity only to glidex itself (W6) until it has one.

### 4.2 Identity token

```json
{
  "iss": "https://glidex.example.org/d/local",
  "sub": "spiffe://local.glidex.example.org/tenant/acme/project/web/vm/6f1c…",
  "aud": "sts.amazonaws.com",
  "iat": 1792800000, "nbf": 1792800000, "exp": 1792800900,
  "jti": "…",
  "glidex": {
    "tenant": "acme", "project": "web",
    "vm": { "id": "6f1c…", "name": "web-1" },
    "cluster": "<cluster id>",
    "service_account": "<token id>"
  }
}
```

- Trust domain: the directory's (W13), `local.glidex.example.org` above.
- `glidex.service_account` only when one is attached. Owner references
  ([apps.md](apps.md)) add `glidex.owner` later. No node name, IP or MAC: they
  change with placement and say nothing a relying party should match on.
- Names in claims are slugs; a relying party should match on `sub` or
  ids, which never change.

### 4.3 Glidex access token

What W6 returns: a `gxt_` federated token (D48) with origin `vm`,
recording the VM id, the cluster and the token's `jti`, acting as the
service account. The metadata endpoint mints it in one step; it is the
exchange of §5 done for the guest.

### 4.4 Lifetimes

| | Default | Range | Served |
|---|---|---|---|
| Identity token | 15 min | 10 min to 1 h (`ttl`) | Cached per VM and audience; a new one after 80% of its lifetime |
| Glidex token | 15 min | up to 1 h, never past the service account's expiry | Cached per VM; a new one after 80% |
| Directory signing key | rotated every 30 days | | Published until every token it signed has expired |
| Cluster key certificate (T6, §6) | 48 h | | Renewed daily |

What others do, for comparison (checked October 2026):

| System | Token | Default lifetime | Refresh |
|---|---|---|---|
| Google Compute Engine metadata server | Service account access token, ID token | 1 h | Clients refetch near expiry |
| Kubernetes projected service account token | Audience-bound JWT | 1 h (`expirationSeconds`, at least 10 min) | kubelet rotates at 80% of the lifetime or after 24 h |
| AWS EC2 instance metadata | Role credentials | Hours | New credentials at least 5 min before the old expire |
| AWS STS `AssumeRoleWithWebIdentity` | Session | 1 h (15 min to the role's maximum, up to 12 h) | Assume again |
| SPIRE | JWT-SVID / X.509-SVID | 5 min / 1 h | Agent rotates at half the lifetime |
| Azure managed identity | Access token | 24 h, cached by the platform | Can't be forced; changes take up to 24 h |

## 5. Exchange

`POST /auth/exchange` (tenancy.md §9) accepts a VM's identity token as
`subject_token` with `federation=<tenant>/vm` (a built-in name, no
record): the token must be the directory's own, unexpired, with
`aud` = this cluster's id, for a VM that still exists, runs, and has a
service account attached; the answer is the token of §4.3. The metadata
endpoint makes this unnecessary inside glidex; it is there for a guest
that forwards its identity to a component off the VM (a CI runner VM
handing a token to a build container, say), and is what T6 clusters use
for VMs of another directory's tenants.

## 6. Delegated cluster keys (T6)

When directory and cluster are apart:

1. During the handshake (tenancy.md §12) and then daily, the cluster
   sends the directory a public key it generated; the directory signs a
   **cluster key certificate**: the key, the cluster id, the tenants the
   cluster may mint for (those admitted there and opted in), valid
   48 h.
2. The directory adds the key to its JWKS (with `kid` =
   `<cluster id>:<n>`) and removes it when it expires or the cluster
   loses trust.
3. The cluster signs workload tokens with it, `iss` still the
   directory's. A relying party fetching the JWKS sees one issuer.
4. A cluster cut off from its directory keeps minting until its
   certificate expires (48 h, longer than T6's default staleness), then
   stops.

## 7. API, policy and audit

- **VM field** `service_account` (create and update, `UpdateVm` mask
  `serviceAccount`); shown on the VM page and by `gxctl get`.
- **Cedar:** `action useServiceAccount in ["project.members"] appliesTo
  { principal: [User, Token], resource: [Project], context: ProjectCtx }`:
  on the service account's project (the `Token` entity carries no
  project), `context.project` the VM's. It is in the group of
  `manageServiceTokens`, so whoever may give a VM a service account could
  already make that service account's tokens (project owners; not
  editors). `base.service-account-same-tenant` forbids one of another
  tenant; a site policy can narrow it further.
- **Audit:** each glidex token minted for a VM (`issueVmToken`, actor
  `system:vm:<vm id>`, the service account, the cluster) and every call
  made with it (the service account's `Token` as principal, with
  `federated` naming the VM). Identity tokens are not audited one by
  one; a per-VM counter is (metering.md).
- **Metadata API** (both channels; all `GET`, header `Metadata-Flavor: Glidex`,
  no redirects, `Content-Type: application/json` except tokens):
  - `/v1/identity?audience=<aud>[&ttl=<secs>]`: the identity token, as
    `application/jwt`.
  - `/v1/glidex/token`: `{access_token, expires_in, token_type,
    endpoint}`, `endpoint` being the cluster's API URL.
  - `/v1/instance`: `{vm_id, name, project, tenant, cluster}`.
  - `/v1/` lists them. Rate limit: 10 requests a second per VM.
  - A request carrying `X-Forwarded-For` or `Forwarded` is refused
    (`403`), as is one from a VM that isn't running.

### 7.1 The network channel

- **Node NAT networks** (WI1b-1): the port's identity travels as a
  packet mark, then as an address only netd hands out.
  1. netd gives each VM port on a NAT bridge a **metadata index**
     (1–16383 per node, kept across re-attaches) and an OpenFlow rule on
     its bridge: `in_port=<port>,tcp,nw_dst=169.254.169.254,tp_dst=80 →
     set pkt_mark 0x6d000000|index, NORMAL`. Only that rule sets such a
     mark, and only for its port.
  2. `inet glidex` (netd's table): a `prerouting` NAT rule DNATs marked
     requests from NAT bridges to `169.254.169.254:8775`, where metad
     listens (the address is on `lo`); an `input` NAT rule SNATs them to
     the port's **peer address** `169.254.192.0/18 + index` (a map
     generated from the port records). The `input` filter accepts only
     marked connections to metad's port and drops everything else for it,
     host processes included. Behind ufw, `GLIDEX-INPUT` gets the same
     marked accept.
  3. metad, unprivileged, reads the peer address `accept` gives it and
     sends only that to the control plane; the control plane finds the
     running VM on its own node whose NIC status has that index
     (`NicStatus.metadata_index`, recorded from netd's `AttachResult`).
  - The guest's source address is never trusted: a guest on another port
    can spoof it but has no mark, and can't complete a handshake for an
    address that isn't its own.
  - Not used: `net.ipv4.tcp_fwmark_accept` (host-wide, it would change
    other services' sockets), a network namespace for metad (a mark
    doesn't cross namespaces), `100.64.0.0/10` (Tailscale's).
  - OVS keeps no OpenFlow rules across a restart: netd puts them back at
    start; a port that had none (attached before this) gets one when it
    is attached again.
- **Isolated and bridged node networks, DPDK bridges** (WI1b-3): no host
  address on the bridge, or a userspace datapath where the mark doesn't
  reach the host stack; to design.
- **OVN networks:** on each node, a local port per network with VMs
  there answers `169.254.169.254` (the OpenStack OVN metadata agent's
  design) and hands requests to metad. OVN port security binds each
  VM port's MAC and addresses, so on such a network the (network,
  source address) pair names one port; glidex's OVN ports get address
  port security for this (checked in WI1b-2).

### 7.2 The vsock channel

- Each VM gets a virtio-vsock device with a CID unique on its node.
  Cloud Hypervisor (`--vsock cid=…,socket=…`): a guest connection to the
  host on the metadata port reaches a Unix socket in the VM's runtime
  directory, which the VM's shim relays to metad naming the VM. QEMU
  (`vhost-vsock-pci,guest-cid=…`): metad listens on the host's vsock
  port and maps the peer CID to the VM.
- The same API, as HTTP/1.1 over the stream. The port number, the
  devices each hypervisor needs in its unit (`/dev/vhost-vsock` for
  QEMU) and the guest helper are settled in WI1c.

## 8. Agents (WI3)

An agent is a workload, usually in a VM: it has the VM's identity and
the rights of the VM's service account. Acting **for a person** is
different, and uses delegation:

- The person approves, once, an agent (named by its VM or service
  account) to act for them with chosen roles and a lifetime.
- The agent exchanges (`POST /auth/exchange`, `subject_token` = the
  person's grant, `actor_token` = its identity token) for a token whose
  `sub` is the person and `act` the agent. Its rights are the
  intersection of the person's and the grant's; at most 1 h, renewable
  only while the grant lasts.
- Cedar's context gains `auth.actor` (the agent's principal) so site
  policies can treat delegated calls differently: refuse destructive
  actions, or require the person's step-up.
- The audit names both, person and agent, for every call.

W10 keeps the token and audit formats ready for this; the grants
(storage, UI, revocation) are milestone WI3's design.

## 9. Security considerations

- **Port identity.** On node networks metad learns the port a request
  came in on; on OVN networks address port security makes (network,
  source address) name one port (§7.1); over vsock the device names the
  VM. A spoofed source address reaches nothing it could use; a request
  from a port or device that belongs to no running VM gets `403`.
- **metad** holds no signing key and can only ask for the VMs running
  on its node (W11); netd remains the only process that changes the
  datapath.
- **Inside the guest** every process can fetch the VM's tokens, as on
  any cloud. A Kubernetes cluster on glidex must keep its pods from the
  endpoint (a network policy, or the cluster's own per-pod identity) or
  every pod acts as the node (the GKE metadata concealment problem).
  The Kubernetes app does this ([apps.md](apps.md) §9).
- **Request forgery.** Required header, no redirects, `Forwarded`
  refused, TTL 1 on replies: a guest web application that can be made
  to fetch a URL can't be made to fetch a token without also setting a
  header (AWS IMDSv1's weakness).
- **Exfiltrated tokens** are bound to an audience and expire within
  minutes; glidex tokens to one cluster.
- **Operators and hosted-directory hosts** can mint tokens for what they
  run or host (§3); the tokens say which cluster minted them.
- **Key compromise** of a cluster's delegated key reaches only its
  admitted tenants for at most 48 h; the directory removes it from the
  JWKS at once.

## 10. Testing

- Unit: claims, lifetimes, key rotation and JWKS contents; the endpoint's
  header, `Forwarded` and rate-limit rules; `useServiceAccount`.
- API: attach and detach a service account; tokens refused without one;
  revoking the service account ends minting; audit records.
- Functional: a VM on a node network and one on an OVN network fetch an
  identity token with `curl`, verify it against the JWKS, get a glidex
  token and list their own project; a request on another VM's port
  can't get the first VM's token.
- Interop checks (spec/idp-checks.md style): AWS `AssumeRoleWithWebIdentity`
  and Vault's JWT auth accept an identity token.

## 11. Changes to other specs (on acceptance)

- **tenancy.md:** §9 gains the built-in `vm` origin and §5; §7 notes the
  shared signing keys; §20's SPIFFE item points here; T6's task list
  gains §6.
- **security.md:** `useServiceAccount`; the metadata endpoint in the
  threat model.
- **networking.md:** serving `169.254.169.254` on node networks and OVN
  networks.
- **data-model.md:** `Vm.service_account`; directory signing keys;
  federated token origin `vm`.

## 12. Settled questions

| Question | Answer |
|---|---|
| Where the endpoint is served | A per-node `glidex-metad` fed by netd (W11, §7.1). |
| vsock | A second channel beside the network one (W12, §7.2), milestone WI1c. |
| Trust domain naming | Per directory, `<slug>.<issuer host>` by default or the organization's own name, fixed at the first token; no public URL needed except for relying parties that fetch discovery (W13). |
| Default lifetimes | 15 min for identity and glidex tokens, refreshed at 80% (W8, §4.4). |

## 13. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **WI0** issuer *(done)* | Directory signing keys, rotation, discovery and JWKS (built as tenancy.md T6 task 2, D52–D53); minting identity tokens in the control plane | Unit tests; an outside verifier (a JWT library) accepts a token against the published JWKS. |
| **WI1a** VMs' service accounts *(done)* | `service_account` on VMs; `useServiceAccount`; glidex tokens (W6); exchange (§5); audit; UI and gxctl | §10 API tests. |
| **WI1b-1** the endpoint on NAT networks *(done)* | `glidex-metad`, the control plane's `metad.sock`, netd's steering on node NAT networks (§7.1); installer; user guide | The e2e test from inside a guest (§15). |
| **WI1b-2** OVN networks and clusters | The per-network local port on OVN networks, address port security on OVN ports; on a node that isn't the leader, metad's asks forwarded to the leader as that node | A guest on an OVN network, on an agent node. |
| **WI1b-3** isolated, bridged and DPDK node networks | The endpoint where the bridge has no host address, or the datapath is userspace | Their own designs. |
| **WI1c** vsock | vsock devices on both hypervisors, the shim relay (Cloud Hypervisor) and metad's vsock listener (QEMU), the guest helper | The functional test over vsock on a VM with no network. |
| **WI2** outside | Docs and checks for AWS, Vault, Google and Kubernetes as relying parties; per-VM metering of tokens | The interop checks of §10. |
| **WI3** delegation | Agent grants, the actor exchange, `auth.actor` in Cedar, audit | Design review, then its own tests. |
| **WI4** T6 | Delegated cluster keys (§6) | Lands with T6: tokens minted on a cluster apart from its directory verify against the directory's JWKS; a withdrawn cluster key stops verifying. |

## 14. References

- Google Compute Engine metadata server: instance identity tokens
  (`…/service-accounts/default/identity?audience=`), the
  `Metadata-Flavor` header.
- AWS EC2 IMDSv2: session tokens and hop limits against request forgery.
- Kubernetes service account token projection: audience- and
  time-bound tokens; the service account issuer's OIDC discovery.
- SPIFFE: SPIFFE IDs and JWT-SVIDs; SPIRE.
- RFC 8693 (OAuth 2.0 Token Exchange): `actor_token` and the `act`
  claim; RFC 7519 (JWT); OpenID Connect Discovery 1.0.
- IETF WIMSE working group (Workload Identity in Multi-System
  Environments).
- OpenStack Neutron/OVN metadata agent: serving `169.254.169.254` on
  OVN networks.
- Lifetimes (§4.4): [EC2 instance metadata credentials](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instance-metadata-security-credentials.html),
  [STS AssumeRoleWithWebIdentity](https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRoleWithWebIdentity.html),
  [SPIRE server configuration](https://spiffe.io/docs/latest/deploying/spire_server/),
  Kubernetes `ServiceAccountTokenProjection` and kubelet token rotation,
  Google Compute Engine metadata server tokens, Azure managed identity
  token caching.

## 15. Implementation status

**WI0** is implemented. The directory's keys, discovery and JWKS came
with tenancy.md T6 task 2 (D52, D53); on top of them:

| Spec | Code |
|---|---|
| Identity token claims, lifetimes, SPIFFE ID, trust domain names, audiences (§4.2, §4.4, W9, W13) | `directory/workload.rs` (`identity_claims`, `ttl`, `spiffe_id`, `valid_trust_domain`, `default_trust_domain`, `valid_audience`) |
| Minting for a VM (W4) | `api::issuer::vm_identity` (a running VM's tenant, project and directory; `409 not_running`), `api::issuer::sign_identity` (typed `JWT`, signed with the directory's keys through `api::issuer::sign`) |
| Trust domain per directory (W13) | `Directory.trust_domain`, fixed by `TenantStore::set_trust_domain` at the first identity token or at creation (`CreateDirectoryRequest.trust_domain`, gxctl `directory create --trust-domain`, the Hosting page); `HostedDirectory.trust_domain` shows it, or the default it would get |
| Never a bearer token (§3) | `api::op::access_principal` takes only `typ: at+jwt` |
| Tests | `directory::workload` unit tests; `connector_tests.rs`: `vm_identity_tokens_verify_against_the_directory_jwks` (verified with `jsonwebtoken` against the served discovery and JWKS), `a_directory_keeps_the_trust_domain_it_was_created_with` |

Nothing calls `sign_identity` for guests yet: the metadata endpoint is
WI1.

**WI1a** (the API half of WI1) is implemented; the metadata endpoint is
WI1b.

| Spec | Code |
|---|---|
| `service_account` on VMs (W5) | `VmConfig.service_account`; `service_account` in `CreateVmRequest`, `Vm` and `VmChange` (mask `service_account`, `null` removes it); not launch-relevant, so no `RestartRequired` |
| `useServiceAccount` (§7) | `policies/glidex.cedarschema`, `base.service-account-same-tenant` in `policies/base.cedar`; `service::vm::check_service_account` (a live `SERVICE_ACCOUNT` token, else `400`) on create and on a changed `service_account` |
| Glidex tokens for a VM (W6) | `api::issuer::vm_glidex_token`: a `FederatedToken` with `federation` `vm:<tenant id>` and `rule` `vm:<vm id>`, 15 min, never past the service account's expiry; `AuthService::federated_principal` checks only the service account for those (W8); audited `issueVmToken` as `system:vm:<id>` |
| Exchange (§5) | `directory::exchange::exchange_vm` for `federation=<tenant>/vm`: `api::issuer::verify_identity` (typed `JWT`, the directory's keys and issuer, `aud` this cluster), a running VM of that tenant, each token once (`replay_key` `jti:<jti>`); `vm` is refused as a federation id |
| UI and gxctl | the create form's *Service Account* picker and the VM page's `VmServiceAccount`; gxctl `service-account <vm>` |
| Tests | `connector_tests.rs`: `giving_a_vm_a_service_account_needs_use_service_account`, `a_vm_gets_glidex_tokens_for_its_service_account`, `the_vm_exchange_takes_only_a_running_vms_identity_token`; UI e2e `vm-identity.spec.ts`. An exchange that succeeds needs a running VM: WI1b's functional test |

**WI1b-1** (the endpoint on node NAT networks) is implemented; §7.1
describes the datapath.

| Spec | Code |
|---|---|
| Marks, peer addresses, the OpenFlow rule, the `inet glidex` rules (§7.1) | `glidex-ovs`: `metadata.rs` (`mark`, `peer_address`, `index_of`, `allocate`, `flow`, `add_flow`, `del_flow`, `ensure_address`, `nft_chains`, `input_rules`), `nat::nft_script`'s `metadata_ports`, `iptables_input_rules` |
| Indexes per VM port | `glidex-netd`: `VmPortRecord.metadata_index` and `AttachResult.metadata_index`; `attach` (kept across re-attaches), `remove_port` and `move_port` drop the rule; `reconcile_startup` puts the rules back |
| The control plane's side (W11) | `api::metad` on `metad.sock` (`metad_socket`, only `metad_user`): `/v1/instance`, `/v1/identity`, `/v1/glidex/token`, for the running VM on this node with that index (`NicStatus.metadata_index`); answers kept until 80% of their life, by VM (`reuse`, `remember`); `503 not_leader` where writes don't go (WI1b-2 forwards) |
| `glidex-metad` | the crate: `Metadata-Flavor: Glidex`, `Forwarded`/`X-Forwarded-For` refused, peer addresses only, 10 requests a second per port, IP TTL 1, `application/jwt` identity tokens; `packaging/glidex-metad.service` (its own user, no capabilities) |
| Installer | the `glidex-metad` user and group, binary and unit, with networking and services; uninstall removes them |
| Tests | `glidex-ovs` (`metadata`, the ruleset goldens, checked with `nft -c`), `netd_tests.rs::nat_ports_get_a_metadata_index_and_rule`, `glidex-metad`'s own, `connector_tests.rs::metad_socket_answers_metad_alone_and_only_for_vm_ports`; `glidex-e2e`'s `metadata.rs` from inside a guest, the successful exchange included |

Known limits of WI1b-1:
- **A NAT guest reaches the host's API only where allowed**: netd's
  `input` chain drops everything from NAT bridges to the host but DHCP,
  DNS, ping and the metadata endpoint. A network's `host_api`
  (`SetNetworkHostApi`, `setNetworkHostApi` in `host.network`: host
  network administrators, never a project's own) or a host's
  `nat_host_api` opens the API's `listen` ports from that bridge (on
  every host address, `inet glidex` and `GLIDEX-INPUT`); the network
  controller keeps netd's `NatState.host_api_ports` as they should be.
- In a cluster, nodes that can't write (followers, agents) forward to the
  leader since WI1b-2's first part, below.
- The control plane keeps each answer until 80% of its lifetime (§4.4),
  keyed by the VM, its service account and the question, never by the
  peer address: netd hands a freed index to the next VM, and an answer
  kept by address would reach it (the e2e test checks this). A changed
  service account gets a fresh token at once (W5).

**WI1b-2, first part: answers from the leader.** A node that can't write
(a follower server or an agent) finds the VM behind the peer address in
its own cache (an agent's holds only its VMs, so only the id) and asks
the leader as itself: `POST /cluster/v1/metadata` on :8842 with its node
certificate, `{vm_id, kind, audience, ttl}`. The leader answers only
about a running VM **placed on the asking node** (spec/clustering.md
§12.3), signs and records as it would for itself (D53), and keeps its
answers by VM as before.

| Spec | Code |
|---|---|
| Asking | `api::metad::answer` (local when `can_write`, else `ask_leader` through `NodeLink::post_leader` or `Cluster::post_leader`), `Question`, `Kind` |
| Answering | `cluster::server::metadata` (`caller`, leader only), `api::metad::leader_answer` (`not_placed_here` unless the VM's placement is the node's); `Cluster::app`, set by `VmManager::set_api_app` |
| Agents | `main.rs`: an agent serves `metad.sock` too |
| Tests | `cluster_tests.rs::an_agents_metadata_tokens_come_from_the_leader` (an agent's identity token signed by the leader; a VM of another node refused) |

**WI1b-2, second part: OVN and isolated networks (design, prototyped).**
Their guests' requests never reach the host stack (OVN switches them in
its pipeline; isolated bridges have no host address), so netd's rules
can't catch them. Two designs:

- **tc/eBPF at the VM's port** (prototyped,
  `scripts/dev/spikes/metadata-ebpf/`). A tc ingress program on every
  VM's tap sees its packets before OVS or OVN. For `169.254.169.254:80`
  it rewrites the source to the port's peer address and the port to
  metad's, remembers the VM's address and MAC in a map, and redirects
  the packet into the host stack through a dummy device; a tc egress
  program on that device rewrites replies back and redirects them out of
  the right tap. The spike, on a bridge with no host address or route,
  identified two VMs by their ports, kept a VM spoofing the other's
  address as itself, and dropped a port without an index. One mechanism
  for node NAT, OVN, isolated and bridged networks (any kernel-datapath
  tap), unaffected by OVS restarts, identity from the tap. Costs: BPF
  code in netd (aya or libbpf), a recent kernel, harder debugging; no
  help for vhost-user/DPDK ports (WI1c's vsock). Guests still need a
  route to `169.254.169.254` (a gateway, or an on-link route and an ARP
  answer for it, which the program would give).
- **OVN localports** (OpenStack's design): a namespace with a proxy per
  OVN network on each node, identity from (network, source address)
  under port security. More processes and namespaces, and isolated node
  networks would still need something else.

Recommendation: tc/eBPF, replacing the OpenFlow mark on node NAT
networks too, once it handles ARP for `169.254.169.254` and the
DPDK-port exclusion.


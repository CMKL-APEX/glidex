# Tenants, directory and operators (design)

Status: **design**, nothing implemented. Builds on
[security.md](security.md) §5–§7 (authentication, projects and teams,
Cedar) and [clustering.md](clustering.md) §12 (cluster authorization).
It lifts the "federation and SCIM provisioning" item of
[security.md §12](security.md#12-out-of-scope).

This document adds the **tenant**, an organization, above projects, and
separates two things that today are one: the people and organizations
that use glidex, and the clusters that run their workloads.

- A **tenant** has users, teams and projects, and brings its users from
  **directory connectors**: PAM, LDAP, OIDC, Google, Microsoft Entra ID
  (Azure), GitHub, JumpCloud and SCIM.
- A **project** belongs to one tenant, never more, and can use
  resources in several clusters.
- A **cluster** is operated by one **operator tenant**: the
  organization that owns its hardware. Several clusters may share an
  operator, or each may have its own. The operator runs the cluster's
  hosts (nodes, OVN, host networks, PCI devices, images, membership),
  writes its site policies, and decides which tenants it admits.
- The **directory** holds the tenants, users, teams, projects and
  connectors, and is where users log in. One login works on every
  cluster that admits the user's tenant. The directory is run by the
  **directory owner**, itself a tenant.
- Clusters that share a directory form a **fleet**. A standalone
  cluster is a fleet of one: it is its own directory, and its operator
  owns the directory. That is what an upgraded host looks like (§16).

```
                 directory (owner: tenant "admin")
        tenants · users · teams · projects · connectors · logins
         │ feed (filtered per cluster)              │
 ┌───────┴──────────────┐              ┌────────────┴─────────┐
 │ cluster A            │              │ cluster B            │
 │ operator: "admin"    │              │ operator: "dc-north" │
 │ admits: lab, acme    │              │ admits: acme         │
 │ site policies, hosts │              │ site policies, hosts │
 └──────────────────────┘              └──────────────────────┘
   project acme/web: VMs in A and B · project lab/ci: VMs in A
```

## 1. Starting point (17946fc)

| Area | Today | Problem for multi-tenancy |
|---|---|---|
| Hierarchy | `Project in Cluster`; teams and users are global (`security.md` §6.1). | No boundary between groups of projects: whoever holds `system.identity` or `system.projects` manages everyone, and nobody else can manage users at all. |
| Operators | One set of host roles; the cluster's operator and the people managing users are the same. | Hardware owned by different organizations can't be offered to the same users. |
| Login sources | One PAM config and **one** OIDC issuer in `control-plane.json` (`config.rs::OidcConfig`). | A second organization can't bring its IdP; changing the IdP needs a root-edited file and a restart. |
| Login scope | Users, sessions and tokens are per cluster. | A user of two clusters is two users with two logins. |
| Provisioning | JIT at login only; team sync from the `groups` claim at login. | A user removed from the IdP keeps their glidex session and **personal tokens (up to 365 days)** until they expire. |
| Names | Project, disk and network names are unique cluster-wide. | A name collision tells one tenant that another tenant's object exists. |
| Machine identity | Service-account tokens are long-lived bearer secrets. | CI pipelines store `gxt_` secrets instead of using the OIDC/STS identity they already have. |
| Secrets | The only IdP secret is the systemd credential `oidc-client-secret`. | Per-tenant connectors created through the API need secrets stored by the control plane. |

## 2. Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| D1 | Tenant boundary | Every project, team, user, service account, guest credential and connector belongs to **exactly one** tenant. Projects never span tenants. | The tenant is the organizational boundary; Cedar expresses it as `resource in principal.tenant`. |
| D2 | Users | A user belongs to one tenant (its *home tenant*). The same person in two organizations is two users. | Keeps identity keys, provisioning and deprovisioning per tenant; no cross-tenant identity merging to get wrong. |
| D3 | Directory and clusters | Tenants, users, identities, teams, projects, guest credentials, project and tenant role links, connectors, federations and API tokens are **directory** records. VMs, disks, networks, routers, nodes, images, site policies and host role links are **cluster** records. | The people and organizations are the same everywhere; hardware and what runs on it are owned and operated per cluster. |
| D4 | Projects span clusters | A project uses resources in every cluster that admits its tenant (D7). Each VM, disk, network and router is in one cluster and one project. Project role links apply in every cluster. | A service can run in several datacenters under one set of roles. L2 networks, disks and placement still stop at a cluster boundary. |
| D5 | Operator tenants | Each cluster has one **operator tenant** (`Cluster.operator`), set when it joins the fleet. Only the operator's principals can hold host roles on it and pass the **host-scoped** actions there (every `host.*`, `image.manage`, cluster `policy.*`, `quota.exceed`, `node.drain`, `host.cluster*`), enforced by a `forbid` in `base.cedar`. An operator may run many clusters. | Hardware owned by different organizations stays under its owner's control; a role link given to anyone else by mistake hands out nothing. |
| D6 | Directory owner | One tenant owns the directory (`owns_directory`): it creates tenants, flags operator tenants, assigns domains, admits clusters into the fleet. Its rights are **directory-scoped**; on a cluster it has only what that cluster's operator gives it, unless it is the operator. | Running the directory and running hardware are different jobs, possibly of different organizations. |
| D7 | Admission, both sides | A tenant's projects can create resources on a cluster only when the cluster's operator **admits** the tenant (with a cap, D17) and the tenant **opts in** to the cluster. The cluster's site policies can refuse more (`useCluster`). The directory sends a cluster only the records of tenants admitted on both sides. | The operator decides whom it serves; the tenant decides which operators see its users and projects. |
| D8 | Per-cluster site policies | Each cluster keeps its own site policies (security.md §7.6), written by its operator; the directory has its own for directory actions. A cluster's policies decide requests evaluated on that cluster only. | Each operator sets its own rules: accepted tenants, console network ranges, device passthrough. |
| D9 | One login | Users log in at the directory, which is the fleet's OIDC provider. A cluster accepts directory-issued tokens addressed to it and checks them against its replica (§7). Cluster-local credentials remain for the operator's host accounts and break-glass. | One login for the user; no cluster ever holds a tenant's IdP secrets or a credential valid on another cluster. |
| D10 | Directory replication | The directory is one glidex Raft cluster. Each fleet cluster keeps a read-only replica of the records it needs, fed by a filtered change stream. There is no Raft across clusters and no write forwarding from clusters to the directory: directory writes are made against the directory, with the caller's own credential. | Datacenters fail and partition independently; directory writes are rare, resource writes stay local, and a compromised cluster can't write the directory on anyone's behalf. |
| D11 | Partitions | A cluster cut off from the directory keeps running workloads and serving credentials it can verify; new logins need the directory. Past `fleet.max_staleness` (set by the operator, default 1 h) the cluster stops accepting directory credentials, because a revocation may not have reached it; cluster-local credentials keep working. | Running workloads must not depend on a WAN link; "removed from the IdP" must not stay ineffective indefinitely. The operator chooses the balance for its cluster. |
| D12 | Tenant administration | Role `tenant-admin` on a `Tenant`: projects, project quotas within each cluster's cap, teams, users, links, connectors, opt-ins. Role `tenant-auditor`: read everything in the tenant, its audit and usage. | Organizations run themselves without asking an operator or the directory owner. |
| D13 | Delegated administration | Directory-owner principals may be linked to tenant or project roles of any tenant; these links are visible to the tenant and their actions are in its audit. Operator principals reach a tenant's resources **in their own cluster** through their host roles, also audited to the tenant. Nobody else crosses the tenant boundary. | The directory owner bootstraps and helps tenants; an operator supports what runs on its hardware; tenants can see both. |
| D14 | Connectors | A **connector** is one configured external source, owned by a tenant. It may do any of: **login**, **provisioning** (users and groups created, updated and disabled ahead of login), **attributes** (SSH public keys, display name, email). Presets (Google, Entra, GitHub, JumpCloud) are fixed configurations of four protocols: PAM, LDAP, OIDC/OAuth 2.0, SCIM 2.0. | Every "provider" is a preset, not a new code path; the security-critical parsing exists once. |
| D15 | Identity key | `(connector_id, subject)`, with the provider's **immutable** subject (`sub`, Entra `oid`, GitHub numeric id, LDAP `entryUUID`/`objectGUID`). Emails and login names are attributes, never keys. `unix:`/`pam:` keys keep the node suffix of clustering.md D13. | Emails get reassigned; GitHub logins get renamed. Keying on them lets a new owner of the name inherit someone's account. |
| D16 | Group → team | Directory groups map to teams of the same tenant (`group_teams`), or auto-create one team per group (`group_mode: auto`, marked `managed_by`). Teams carry rights only through role links. Groups are never mapped to roles directly. | One rights mechanism (links), visible in one place. |
| D17 | Quotas | Each admission carries the tenant's **cap on that cluster**, set by the operator. Tenant admins set each project's quota per cluster. Both are checked by the cluster at create, start and resize. No fleet-wide sum. Exceeding a cap needs `exceedQuota` from the cluster's operator. | Capacity belongs to whoever owns the hardware; the tenant splits its share among its projects. |
| D18 | Deprovisioning | A user disabled or removed by provisioning is **disabled at once**: directory sessions and tokens revoked, managed team memberships dropped; clusters drop the user's sessions as the change arrives. The record stays until a tenant admin purges it. Connectors without provisioning clamp their users' tokens to `token_max_days` (default 30). | Closes the "removed from the IdP, still has a 365-day token" gap. |
| D19 | Connector secrets | Stored **only in the directory**, encrypted with the directory's secrets key (KEK, held like the CA key). Write-only in the API. Never in a cluster's feed. SCIM bearer tokens are stored as SHA-256 only. | Clusters don't need them (logins happen at the directory, D9), so a cluster run by another organization never sees a tenant's IdP secrets. |
| D20 | Owner and operator connectors | Connectors of the directory owner and of operator tenants must restrict who gets in (`jit: false`, non-empty `required_groups`, or provisioning) and require MFA by default (`require_mfa`, from `amr`/`acr`). | A connector that admits "anyone in the IdP" would make every employee of that organization an operator or directory administrator. |
| D21 | PAM and peer identity | Host accounts belong to a cluster. A `pam` connector is created by that cluster's operator, is **bound to that cluster**, and may be placed in the operator's tenant or in a tenant admitted there (a lab giving a class's Unix group its own tenant). PAM and peer logins happen at that cluster and give sessions valid there only. `glidex-admin`/root resolve to the operator tenant. `glidex-authd`'s `allowed_groups` stays the outer fence. | Unix accounts exist on one cluster's hosts; they can't be a fleet-wide credential. |
| D22 | Names | Directory objects of a tenant (projects, teams, service accounts, guest credentials) have names unique **per tenant**. Cluster objects (VMs per project; disks and project networks per tenant) have names unique per tenant **within the cluster**. Tenant slugs are unique in the fleet. Ids stay global UUIDs; flat REST paths stay (security.md decision 8). | Removes cross-tenant existence oracles without changing any id-based path. |
| D23 | Grants | Operators grant host networks and PCI devices on their cluster to a **tenant** (`tenant_grants`) or to projects. Project networks are shared only between projects of the same tenant, within one cluster. | One grant per customer; cross-tenant L2 sharing stays an operator decision. |
| D24 | Machine identity | **Workload federation** at the directory: a tenant trusts an external JWT issuer (GitHub Actions, GitLab CI, any OIDC workload issuer) or AWS STS identity and maps matching claims to one of its service accounts; `POST /auth/exchange` returns a short-lived (≤ 1 h) token for the clusters named. | CI uses the identity it already has; no long-lived glidex secret in a CI vault. |
| D25 | No tenant-authored Cedar | Site policies belong to operators (per cluster) and the directory owner (directory). Tenants get roles and teams. | Tenant-authored Cedar would need per-tenant validation scopes and lock-out rules; nothing asks for it. |
| D26 | Two transports | Every operation is served over REST/JSON and gRPC from one service function (§14.1, [grpc.md](grpc.md)); only protocol-fixed endpoints (OIDC/OAuth browser redirects, SCIM) are HTTP-only. | Built on the service layer from the start instead of ported later. |
| D27 | No SAML | Connectors speak OIDC/OAuth 2.0, LDAP, PAM and SCIM only. A SAML-only IdP connects through an OIDC front (its own OIDC app type, or Dex/Keycloak) as a generic `oidc` connector. | Every IdP listed here offers OIDC. XML signature verification is a long-running source of authentication bypasses. |
| D28 | Tenant creation | Only the directory owner creates tenants. No self-service signup. | Signup would need abuse controls, email verification and default quotas nobody asked for. |
| D29 | Upgrade | An existing host becomes a fleet of one: it is the directory, tenant `admin` owns the directory and operates the cluster, existing projects move to a `default` tenant admitted there (§16). | The tenant boundary holds from the first start, and nothing a user does today changes. |

## 3. Threat model additions

| Actor | Trusted for |
|---|---|
| The directory and its owner | Identity for every tenant in the fleet: who a user is, which tenant, which roles. A compromised directory compromises every tenant's access to every cluster that trusts it, but not an operator's hosts (D5). |
| An operator | Its clusters' hosts, and the resources of admitted tenants **located there** (D13). Not the directory, not other clusters, not tenants it doesn't admit. |
| A cluster's control plane | Its own resources, and the replica of what the directory sends it: the records of tenants admitted on both sides (D7). It never receives connector secrets (D19) or a credential valid elsewhere (§7.3), and can't write the directory (D10). |
| Tenant administrators | Their tenant's projects, users, teams, connectors and opt-ins. Nothing on hosts; nothing in other tenants. |
| A tenant's IdP (OIDC, LDAP, SCIM client) | Identities and group memberships **for that tenant only**. A compromised customer IdP yields at most that tenant's rights. |
| The owner's and operators' IdPs | Their tenants' membership; combined with role links, the directory or a cluster: hence D20. |
| CI issuers trusted by a federation | Claims matching that federation's rules, mapped to one service account. |

New assets: connector secrets, SCIM tokens, the directory's signing keys
and KEK, provisioning state, admissions. New boundaries: IdP →
directory (SCIM pushes and pull-sync responses are untrusted input),
directory → cluster (the feed), and tenant ↔ tenant and operator ↔
operator inside one fleet.

**Invariants:**
- nothing a tenant configures (connector, mapping, team, link,
  federation rule, opt-in) can produce a principal outside that tenant,
  or an action in the host-scoped or directory-scoped groups;
- nothing an operator configures on its cluster affects another
  cluster or the directory;
- a credential a cluster receives is valid on that cluster only.

## 4. Data model

Every table is marked **directory** or **cluster** (`TableId::scope()`),
so the feed knows what to carry. New tables: `tenants`, `connectors`,
`connector_secrets`, `connector_sync`, `scim_tokens`, `federations`,
`fleet_clusters` (directory), `admissions`, `feed_state` (cluster).

| Record | Scope | Change |
|---|---|---|
| `Tenant` (new) | directory | `id` (UUID; `admin` for the upgraded host's tenant), `slug` (`[a-z0-9-]{1,32}`, unique in the fleet), `display_name`, `owns_directory: bool`, `operator: bool`, `suspended: bool`, `tags: [String]` (set by the directory owner; usable in site policies), `domains: [String]`, `clusters: [cluster_id]` (opt-ins, D7), `limits {projects, users, teams, connectors}`, `created_at`. |
| `FleetCluster` (new) | directory | `cluster_id`, `name`, `operator` (tenant id), `endpoints` (API and UI URLs), `ca_fingerprint`, `admitted: [tenant_id]` as the cluster last reported it, `feed_revision`, `joined_at`. |
| `Admission` (new) | cluster | `tenant_id`, `cap` (quotas on this cluster, D17), `status` (`admitted` \| `suspended`), `admitted_by`, `at`. Written by the cluster's operator; the cluster reports the list to the directory, which filters the feed by it and by the tenant's opt-ins. |
| `Project` | directory | + `tenant_id` (immutable), `quotas: {cluster_id → quotas}` (D17). Name unique per tenant. No cluster. |
| `Vm`, `Disk`, `Network`, `Router` | cluster | + nothing new: `project_id` already; the cluster is the one holding the record. |
| `Credential` (guest) | directory | moves from cluster to directory: a project's guest credentials serve its VMs in every cluster. Key `(project_id, username)` as today. |
| `User` | directory | + `tenant_id` (immutable), `managed_by`, `external_id`, `email`, `disabled_reason`. |
| `Identity` | directory | `provider` becomes `conn:<connector_id>`, or `pam`/`unix` with the cluster in the subject (`alice@<node>`, D21); old `oidc:<issuer>` rows are re-keyed at migration. |
| `Team`, `TeamMember` | directory | + `tenant_id`, `managed_by`, `external_id`; `source: manual \| connector:<id>`. |
| `LinkRecord` | both | links on a `Tenant`, a `Project` or the `Fleet` are directory records; links on a `Cluster` or `Host` are records of that cluster. |
| `Token` | directory | personal and service-account API tokens (`gxt_`) + `tenant_id`, `clusters: [cluster_id]` (where it is valid; §7.4). |
| `Network`, `PciDevice` | cluster | + `tenant_grants: [tenant_id]`. |
| `AuditEntry` | both | + `tenant` (actor's) and `target_tenant` (resource's). |
| `Connector` | directory | below. |

```json
{
  "id": "6c1e…", "tenant_id": "…", "kind": "azure",
  "display_name": "CMKL Entra ID",
  "enabled": true,
  "login": true,
  "provisioning": "scim",          // none | scim | pull
  "jit": false,
  "required_groups": ["glidex-users"],
  "require_mfa": true,
  "group_mode": "mapped",           // mapped | auto
  "group_teams": { "<group id or name>": "<team id>" },
  "token_max_days": 30,
  "settings": { "tenant_id": "…", "client_id": "…" },
  "secrets": { "client_secret": { "set": true, "updated_at": 1760000000 } },
  "cluster": null,                  // pam only: the cluster it is bound to (D21)
  "source": "api",                  // api | file
  "created_at": 1760000000
}
```

`settings` is validated per kind (§5). `secrets` are never returned,
only whether each is set and when.

## 5. Connector kinds

| Kind | Login | Provisioning | Attributes | Subject | Notes |
|---|---|---|---|---|---|
| `pam` | `glidex-authd` on the bound cluster | JIT at login | `~/.ssh/*.pub` via authd | `<name>@<node>` | D21. Created by the cluster's operator. |
| `ldap` | simple bind over LDAPS / StartTLS (never plain) | pull: users and groups by filter | `sshPublicKey`, `mail`, `displayName` | `entryUUID` / `objectGUID` | AD, FreeIPA, OpenLDAP, JumpCloud LDAP-as-a-Service. Bind with the user's DN after a search with the service account; password never stored. |
| `oidc` | Authorization Code + PKCE; device grant for gxctl | JIT; optional SCIM | claims | `sub` | Generic: Keycloak, Okta, Authentik, Auth0, Dex, Zitadel. Today's `auth.oidc` behavior (security.md §5.4) unchanged. |
| `google` | OIDC preset (`https://accounts.google.com`); `hd` must equal one of `settings.domains` | pull: Admin SDK Directory API (service account with domain-wide delegation, read-only scopes) | custom schema field for SSH keys (optional) | `sub` | Google ID tokens carry no groups; group mapping needs pull sync. Without it, `group_teams` is refused. |
| `azure` | OIDC preset (`https://login.microsoftonline.com/<tid>/v2.0`); `tid` checked | SCIM (Entra enterprise app provisioning) or pull via Microsoft Graph (delta queries) | — | `oid` (not `sub`, which is per-app pairwise) | Group overage (`_claim_names` past 200 groups) is resolved through Graph when pull is configured; otherwise the login is refused rather than mapping no groups. |
| `github` | OAuth 2.0 web flow and device flow (GitHub is not OIDC) | pull: org members and teams via a GitHub App installation | public SSH keys | numeric user id | Login requires membership of `settings.org` (checked at every login and sync). Team mapping by team id. |
| `jumpcloud` | OIDC preset (`https://oauth.id.jumpcloud.com`) | SCIM (JumpCloud's generic SCIM app) or LDAP | via LDAP `sshPublicKey` | `sub` | |
| `scim` | — (provisioning only; pair with a login connector via `login_connector`) | SCIM 2.0 push | — | SCIM `externalId` | For IdPs that provision by SCIM but log in through another connector of the same tenant. |

**Presets.** `google`, `azure`, `github` and `jumpcloud` are stored with
their kind so the UI can show the right fields and checks, but their
code is `oidc` or OAuth 2.0 plus a short validation and claim-mapping
module. Adding a preset (Okta, OneLogin, Keycloak) is a settings schema
and a claim map, not a protocol.

**AWS.** IAM Identity Center offers SAML, not OIDC, to applications like
glidex (D27). AWS users log in through the IdP that Identity Center
itself federates from (Entra, Okta, Google, JumpCloud), connected
directly. AWS *workload* identity (STS) is covered by federation (§9).

**URLs** are per connector, on the directory:
`https://<directory>/api/auth/c/<connector_id>/callback` (OIDC/OAuth) and
`/scim/v2/c/<id>/` (SCIM). The connector page shows them so the tenant
admin can paste them into the IdP.

**Test.** `POST /connectors/{id}/test` checks what can be checked
without a user (discovery and JWKS, LDAP bind with the service account,
Directory/Graph/GitHub API access) and reports per-step results. Never
returns secrets or directory contents beyond counts.

## 6. Logging in at the directory

1. **Discover.** `POST /auth/discover {hint}`: a tenant slug or an email
   address. Returns `{tenant: {slug, display_name}, connectors: [{id,
   kind, display_name}]}` for an exact slug or an assigned domain,
   otherwise `{connectors: []}` after the same delay. Slugs are not
   secrets (they are in URLs), but unknown domains don't reveal which
   tenants exist.
2. **Login** through the chosen connector. Each connector produces an
   `ExternalIdentity {connector_id, subject, email, name, groups,
   mfa}`. The common path then:
   - refuses if the connector or its tenant is disabled or suspended;
   - looks up `(connector_id, subject)`;
   - if absent: JIT-creates the user in the connector's tenant when
     `jit` and `required_groups`/domain rules pass; refused otherwise
     (same error and timing as a wrong password);
   - refuses `require_mfa` connectors without `mfa`;
   - syncs managed team memberships from `groups` (D16);
   - creates a **directory session** (browser) or a refresh token
     (gxctl), stamped with `tenant_id`, `auth_time` and `amr`.
3. **Domains** are assigned to tenants by the directory owner, not
   self-claimed: no DNS verification flow, and no tenant can capture
   another organization's domain.

`Principal` gains `tenant_id`; Cedar's `Auth` context gains `mfa: Bool`,
so site policies can require MFA (e.g. `forbid … host.cluster.critical
unless context.auth.mfa`).

## 7. Using a cluster

### 7.1 Browser

The directory is an OIDC provider; every fleet cluster is a client of
it, registered when the cluster joins (§12).
1. A browser at a cluster's UI without a session is sent to the
   directory (Authorization Code + PKCE, `client_id` = the cluster).
   A user who already has a directory session isn't asked again.
2. The cluster receives an ID token with `aud` = its cluster id, `sub`
   = the user id, `tenant`, `sid`, `auth_time`, `amr`; it checks the
   signature against the directory's keys (in its replica, §12), the
   audience, and that the user is in its replica, enabled, of a tenant
   it admits.
3. It creates a **cluster session** (today's session, security.md §5.5:
   idle 30 min, absolute 12 h, bound to `sid`). Logging out at the
   directory or revoking `sid` reaches the cluster on the feed and ends
   its sessions.

### 7.2 gxctl

`gxctl auth login --directory <url> --tenant <slug>` runs the device
grant (OIDC presets, GitHub) or the password flow (LDAP) at the
directory and keeps a **refresh token** for it in the profile. Before
calling a cluster, gxctl gets an **access token** for that cluster from
the directory: a JWT with `aud` = the cluster id, the same claims as
§7.1, valid `access_token_ttl` (default 1 h). The cluster verifies it
offline. One login serves every cluster; profiles gain `directory` and
list the clusters the tenant uses ([gxctl-auth.md](gxctl-auth.md)).

### 7.3 Why audience-bound

A cluster receives only credentials addressed to it (`aud`), and the
directory accepts only credentials addressed to the directory. A
cluster, even a compromised one, can't replay what it receives to
another cluster or to the directory; refresh tokens never leave the
client and the directory.

### 7.4 API tokens

Personal and service-account tokens (`gxt_`, security.md §5.5) are
directory records valid on the clusters they list (`clusters`, default
the one they were created for; `all` must be asked for). Their hashes
are in the feed of those clusters only, so a cluster verifies them
offline. A token listing several clusters can be replayed between them
by a compromised one of them: the reason the default is one cluster.
On a fleet of one this is exactly today's token.

### 7.5 Cluster-local credentials

Peer identity on `api.sock` and PAM logins (D21) stay on the cluster
that has those accounts and give sessions valid there only. Root and
`glidex-admin` resolve to the operator tenant (break-glass,
security.md §5.2, clustering.md D14 unchanged).

### 7.6 Partitions (D11)

| Lost | Still works | Stops |
|---|---|---|
| Cluster ↔ directory | Running VMs, all project operations; cluster sessions until they end; access tokens until `exp`; API tokens; cluster-local credentials | New cluster sessions (they need an ID token, which needs the directory), feed updates |
| Past `fleet.max_staleness` | Cluster-local credentials, running VMs | Every directory credential on that cluster, because a revocation may be missing |
| User ↔ directory | Credentials the user already holds | New logins and new access tokens |

Every response carries `X-Glidex-Directory-Lag` (seconds) when the
replica is behind, and `GET /fleet` reports it.

## 8. Provisioning (at the directory)

### 8.1 SCIM 2.0 (push)

- Base `https://<directory>/scim/v2/c/<connector_id>/`, TLS only, on the
  directory's API listeners. Authenticated by a bearer token
  `gxs_<base62>` minted by `POST /connectors/{id}/scim-tokens` (shown
  once, SHA-256 stored, expiry ≤ 1 year, several at once for rotation).
  The token authorizes exactly this connector; it is not a Cedar
  principal.
- Supported: `/Users` and `/Groups` with `GET` (filters `userName eq`,
  `externalId eq`, `displayName eq` only), `POST`, `PUT`, `PATCH` (the
  `add`/`replace`/`remove` operations Entra, Okta and JumpCloud send,
  including Entra's non-standard `"value": "False"` for `active`),
  `DELETE`; `/ServiceProviderConfig`, `/Schemas`, `/ResourceTypes`. No
  bulk, sort, or `/Me`. Pagination by `startIndex`/`count` ≤ 200.
- Mapping: `externalId` → `User.external_id`, `userName` → login
  attribute, `active: false` or `DELETE` → D18 disable, group membership
  → managed team membership. Unknown attributes are accepted and
  ignored. Groups create managed teams only with `group_mode: auto`.
- Limits: 20 requests/s per connector, 1 MiB bodies, tenant `limits`
  enforced with SCIM `409`/`400` errors. Every write is audited as
  `system:scim:<connector_id>` with the tenant.

### 8.2 Pull sync

- Kinds `ldap`, `google`, `azure` (Graph), `github`. A controller on the
  directory's leader (`directory_sync`), every `sync.interval` (default
  15 min, minimum 5) and on `POST /connectors/{id}/sync`.
- Each run diffs the connector's managed users and teams and commits
  the diff as write sets of ≤ 500 changes. Delta tokens (Graph delta,
  LDAP `modifyTimestamp`) are kept in `connector_sync`; a full resync
  runs daily.
- **Safety stop:** a run that would disable more than `sync.max_disable`
  (default 10 % of managed users, at least 5) is not applied; the
  connector is marked `sync_blocked` until the tenant admin confirms or
  fixes it. An API error is never read as "no users".

### 8.3 Ownership of fields

A managed user's display name, email, `external_id` and disabled state
belong to its connector: manual edits return `409 managed_by_connector`.
Role links, default project and manual team memberships stay the
tenant's. Deleting a connector disables its users (default) or detaches
them.

## 9. Workload federation (at the directory)

```json
{
  "id": "…", "tenant_id": "…", "kind": "jwt",
  "issuer": "https://token.actions.githubusercontent.com",
  "audience": "glidex:<directory id>",
  "rules": [
    { "match": { "repository": "cmkl/infra", "ref": "refs/heads/main" },
      "service_account": "<token id>", "clusters": ["<cluster id>"], "ttl_secs": 900 }
  ]
}
```

- Kinds: `jwt` (any OIDC-style workload issuer: GitHub Actions, GitLab,
  Kubernetes service-account issuers, Azure managed identity, Google
  service-account ID tokens) and `aws` (a presigned
  `sts:GetCallerIdentity` request, verified by calling STS; match on
  account and role ARN).
- `POST /auth/exchange` (RFC 8693 shape) → access tokens (§7.2) acting
  as the mapped service account, for the rule's clusters, at most
  `ttl_secs` (≤ 3600), not renewable, audited with the rule and the
  external subject.
- Rules match exact claim values only; every rule must name at least
  one of `repository`/`project_path`/`sub`/role ARN. The service
  account must belong to the federation's tenant.

## 10. Authorization (security.md §7)

### 10.1 Schema

```cedarschema
namespace Glidex {
  type Auth = {
    method: String, transport: String, age_secs: Long,
    source_ip?: ipaddr, mfa: Bool,
  };
  // Every request is decided by one cluster (or the directory, which
  // is a cluster too); `cluster` is that one.
  type Ctx = { auth: Auth, cluster: Cluster };
  type ProjectCtx = { auth: Auth, cluster: Cluster, project: Project };
  type NetworkCtx = { auth: Auth, cluster: Cluster, network: Network };

  entity Fleet;
  entity Tenant in [Fleet] {
    owns_directory: Bool, operator: Bool, suspended: Bool,
    tags: Set<String>, clusters: Set<Cluster>,
  };
  entity Cluster in [Fleet] { operator: Tenant, admitted: Set<Tenant> };
  entity Host in [Cluster];
  entity Team in [Tenant] { tenant: Tenant };
  entity User in [Team, Tenant] { disabled: Bool, tenant: Tenant };
  entity Token in [Tenant] { owner?: User, expired: Bool, tenant: Tenant };
  entity Connector in [Tenant] { tenant: Tenant };
  // Projects and guest credentials are tenant records, in no cluster (D3, D4).
  entity Project in [Tenant] { tenant: Tenant };
  entity Credential in [Project] { project: Project };
  // What runs somewhere is in its project and in its cluster.
  entity Vm in [Project, Cluster] { project: Project, cluster: Cluster };
  entity Disk in [Project, Cluster] { project: Project, cluster: Cluster };
  entity Network in [Cluster, Project] {
    project?: Project, all_projects: Bool, grants: Set<Project>,
    tenant_grants: Set<Tenant>, shares: Set<Project>,
  };
  entity PciDevice in [Host] { grants: Set<Project>, tenant_grants: Set<Tenant> };

  // Marker groups: every concrete action is in exactly one of these
  // (route-coverage test).
  action "tenant-scoped";
  action "host-scoped";
  action "directory-scoped";
  action "shared";

  // Directory roles, linked on the Fleet.
  action "role.directory-admin";
  action "role.directory-auditor" in ["role.directory-admin"];
  action "directory.read" in ["role.directory-auditor", "directory-scoped"];
  action "directory.tenants", "directory.clusters", "directory.policy"
    in ["role.directory-admin", "directory-scoped", "step-up"];

  // Tenant roles, linked on a Tenant.
  action "role.tenant-admin";
  action "role.tenant-auditor" in ["role.tenant-admin"];
  action "role.owner" in ["role.tenant-admin", "role.system-admin"];
  action "role.viewer" in ["role.operator", "role.auditor", "role.tenant-auditor"];
  action "tenant.read", "tenant.audit", "tenant.usage" in ["role.tenant-auditor", "tenant-scoped"];
  action "tenant.members", "tenant.projects", "tenant.clusters" in ["role.tenant-admin", "tenant-scoped"];
  action "tenant.directories" in ["role.tenant-admin", "tenant-scoped", "step-up"];

  // Cluster roles (auditor, image-admin, net-admin, system-admin), linked
  // on a Cluster or Host, keep their groups and gain the host-scoped
  // marker; system-admin also gets admissions:
  action "cluster.admissions" in ["role.system-admin", "host-scoped"];

  // Every resource created on a cluster (compound request, §10.4).
  action useCluster in ["shared"]
    appliesTo { principal: [User, Token], resource: [Cluster], context: ProjectCtx };

  // Existing permission groups gain a marker, e.g.:
  //   "vm.read", "disk.read", "credential.*", … in [..., "tenant-scoped"]
  //   "host.read", "host.network", "host.cluster", "image.manage",
  //   "policy.*", "quota.exceed", "node.drain" in [..., "host-scoped"]
  //   readImage, readNetwork, useNetwork, usePciDevice in ["shared"]
  // system.identity, system.projects and system.tenants are replaced
  // by the directory and tenant groups above.
}
```

How links reach:
- a project role on a `Project` covers its credentials and its VMs and
  disks **in every cluster** (`Vm in [Project, …]`);
- a tenant role on a `Tenant` covers all its projects;
- a cluster role on a `Cluster` covers that cluster's hosts and the
  resources **located in it** (`Vm in [… , Cluster]`), of every tenant,
  but no `Project` or `Credential` record (those are in no cluster);
- a directory role on the `Fleet` covers directory-scoped actions.

### 10.2 Base policies (added)

```cedar
// D1, D13: a tenant's things are its own; the directory owner may be
// linked in (delegation); an operator reaches what runs on its cluster.
@id("base.tenant-boundary")
forbid (principal, action in Glidex::Action::"tenant-scoped", resource)
unless {
  resource in principal.tenant ||
  principal.tenant.owns_directory ||
  (principal.tenant == context.cluster.operator && resource in context.cluster)
};

// D5: a cluster's hosts belong to its operator.
@id("base.host-operator")
forbid (principal, action in Glidex::Action::"host-scoped", resource)
unless { principal.tenant == context.cluster.operator };

// D6: the directory belongs to its owner.
@id("base.directory-owner")
forbid (principal, action in Glidex::Action::"directory-scoped", resource)
unless { principal.tenant.owns_directory };

// D7: admitted by the operator and opted in by the tenant.
@id("base.admission")
forbid (principal, action == Glidex::Action::"useCluster", resource)
unless {
  resource.admitted.contains(context.project.tenant) &&
  context.project.tenant.clusters.contains(resource)
};
// Who may create is decided by createVm & co.; this only checks the place.
@id("base.cluster-use")
permit (principal, action == Glidex::Action::"useCluster", resource);

// A suspended tenant's principals can do nothing.
@id("base.tenant-suspended")
forbid (principal, action, resource)
when { principal.tenant.suspended };

// D23: project networks are shared within one tenant only.
@id("base.share-same-tenant")
forbid (principal, action == Glidex::Action::"offerNetworkShare", resource)
when { resource has project && resource.project.tenant != context.project.tenant };
```

`base.network-grant` gains `|| resource.tenant_grants.contains(context.project.tenant)`,
and `base.pci-grant` the same. An operator's site policy narrows
admission further, e.g.

```cedar
@id("site.edu-only")
forbid (principal, action == Glidex::Action::"useCluster", resource)
unless { context.project.tenant.tags.contains("edu") };
```

### 10.3 Rules kept in code

- **Link boundary.** A link on a `Tenant` or `Project` takes a
  principal of that tenant, or of the directory owner (D13). A link on
  a `Cluster` or `Host` takes a principal of the cluster's operator. A
  link on the `Fleet` takes a principal of the directory owner. Checked
  where the link is written (directory or cluster).
- **Caps and project quotas** (D17), next to `tenancy.rs::overruns`, on
  the cluster.
- **Names** (D22), in the same transaction as the create.
- **Entities per request** include the principal's `Tenant`, the
  deciding `Cluster` (with `operator` and `admitted`), and the
  resource's project and tenant. A missing entity fails closed.

### 10.4 Compound requests (security.md §7.4)

Every create of a VM, disk, network or router on a cluster adds
`useCluster` on that `Cluster` with the project in the context; so does
a start (a suspended admission refuses starts but not stops or
deletes).

### 10.5 Roles

| Role | Link resource | Includes |
|---|---|---|
| project roles (`viewer` … `owner`) | project | unchanged; cover the project in every cluster |
| `tenant-auditor` | tenant | `viewer` in every project, `tenant.read`, `tenant.audit`, `tenant.usage` |
| `tenant-admin` | tenant | `owner` in every project, `tenant-auditor`, `tenant.members`, `tenant.projects`, `tenant.clusters`, `tenant.directories` |
| cluster roles (`auditor`, `image-admin`, `net-admin`, `system-admin`) | cluster / host | unchanged, for that cluster; `system-admin` adds `cluster.admissions` and its site policies; operator principals only (D5) |
| `directory-auditor` | fleet | `directory.read` |
| `directory-admin` | fleet | `directory-auditor`, `directory.tenants` (tenants, operator flags, tags, domains, delegation links), `directory.clusters` (fleet join tokens, removal), `directory.policy` (the directory's site policies); owner principals only (D6) |

## 11. Operators and the directory owner

**An operator** owns, through its cluster roles, on each of its clusters:

| Area | Actions (unchanged names) |
|---|---|
| Cluster membership | init, join and rejoin tokens, remove, forget, purge, drain, detach, imports, CA rotation, snapshots |
| Host networking | OVS install, DPDK, bridges, uplinks, node-scope networks, OVN and the edge router |
| Devices | PCI/VFIO grants. GPUs, DPUs and other accelerators get their own design later (§20). |
| Images | the cluster's image library |
| Tenants on its cluster | admissions with their caps (`cluster.admissions`), site policies (which tenants, which rules), audit and usage of what runs on it, support access to it (D13) |

**The directory owner** owns, through directory roles:
- tenants: *create* `{slug, display_name, domains, tags, operator}`,
  optionally with a first `tenant-admin` invitation bound to the first
  login with that verified email through the tenant's connector
  (single-use, 7 days); *suspend*/*resume* (takes effect on the next
  request on every cluster, as the feed delivers it; running VMs keep
  running unless the operators stop them); *delete* (refused while any
  cluster reports resources of the tenant; then connectors, users,
  teams, projects, links and tokens go in one write);
- the fleet: which clusters belong (§12), with which operator;
- domains, tenant tags, the directory's own site policies.

**A tenant** decides, through `tenant.clusters`, which clusters it
opts into, and sees every operator's admission and every delegated
link affecting it.

## 12. The fleet

**Joining.** The directory owner creates a cluster entry naming its
operator tenant and gets a fleet join token (`gxctl fleet join-token
--operator <tenant>`); the cluster's operator runs `gxctl fleet join`
on it (`host.cluster.critical` and step-up there, `directory.clusters`
at the directory). The token carries the directory CA's hash; the
directory pins the cluster's CA. The cluster becomes an OIDC client of
the directory (§7.1) and starts its feed. A cluster with existing
tenants maps them onto directory tenants (`--tenant <local>=<fleet>`)
or adds them; the directory owner approves the merge in one write, as
an import is approved (clustering.md §5.9).

**The feed.** The directory's leader serves `/fleet/v1/feed` to each
cluster over mTLS between the two clusters' servers: the write sets
that touch directory tables, in revision order (clustering.md D2),
**filtered for that cluster**: the records of tenants it admits that
opted into it, its operator tenant's records, the directory's public
keys, and revocations. Each cluster's leader applies a batch in one
write and records the revision reached in `feed_state`; a cluster too
far behind gets a filtered snapshot. Clusters report their admissions
on the same channel. Connector secrets are never in a feed (D19).

**Directory writes** are made at the directory with the caller's own
directory credential: the UI on a cluster links to the directory's
pages for them, and gxctl calls the directory. The cluster that served
the page is never in that path (D10).

**Losing the directory.** The directory is a Raft cluster, so losing a
node changes nothing. Losing all of it leaves clusters serving as in
§7.6. `gxctl fleet promote` on a cluster that holds a full replica
(only the directory's standby can: clusters hold filtered ones) makes
it the directory under a new **fleet epoch**; feeds carry the epoch, so
an old directory that comes back can't serve clusters. A **standby**
is a cluster that, with the directory owner's consent, receives the
whole directory except connector secrets, which are re-entered after a
promote.

**Leaving.** A cluster that leaves keeps the records of the tenants
with resources on it and becomes a fleet of one with its own directory;
the tenants' users then log in there separately.

**Not across clusters:** Raft (except within the directory's own
cluster), L2 networks, disks, VM placement, quotas, site policies,
image libraries.

## 13. Connector secrets (D19)

- `connector_secrets` rows: `(connector_id, field)` →
  `{kek_id, nonce, ciphertext}` with XChaCha20-Poly1305, AAD =
  `connector_id ‖ field ‖ kek_id`; directory only.
- The KEK is 32 random bytes generated when the directory is created,
  stored as the systemd credential `secrets-kek` (`systemd-creds`, TPM2
  when present) on each of the directory's servers, transferred to
  joining servers with the CA key (clustering.md §5.2), never written to
  Raft, snapshots, feeds or departure bundles.
- Decryption only on the directory's leader, in the connector code
  path, into zeroized buffers.
- **KEK rotation** with CA rotation (clustering.md §12.5) and on demand.
  *Honest limit:* a departed directory server held the KEK and the
  ciphertexts, so the leave flow lists the connectors and asks their
  tenants to rotate IdP-side secrets.
- File-defined connectors (`source: file`, migrated from
  `control-plane.json`) keep reading `oidc-client-secret` from
  `LoadCredential`.

## 14. API

Every operation is served over **both REST/JSON and gRPC** from one
implementation (§14.1, [grpc.md](grpc.md)). Directory operations answer
on the directory; cluster operations on the cluster. The operation
table is the single list both routers are checked against (grpc.md §5).

| Operation (Cedar action) | Where | REST | gRPC | Permission |
|---|---|---|---|---|
| `discover` | directory | `POST /auth/discover` | `LoginService.Discover` | none |
| `startLogin`, `finishLogin` | directory | `GET /auth/c/{id}/start`, `/callback` | — (browser redirects) | none |
| OIDC provider (`authorize`, `token`, JWKS, discovery) | directory | `/oidc/…` | — (OIDC endpoints) | none / client authentication |
| `startDevice`, `pollDevice` | directory | `POST /auth/c/{id}/device`, `…/poll` | `LoginService.StartDevice`, `PollDevice` | none |
| `passwordToken` | directory, or the bound cluster for PAM | `POST /auth/token` (+ `connector`) | `LoginService.PasswordToken` | none (TLS or api.sock) |
| `accessToken` | directory | `POST /auth/access-token` (`{cluster}`) | `LoginService.AccessToken` | refresh token |
| `exchangeToken` | directory | `POST /auth/exchange` | `LoginService.ExchangeToken` | none (workload token) |
| `listTenants`, `readTenant` | directory | `GET /tenants[/{id}]` | `TenantService.List`, `Get` | `directory.read` (all) · own tenant |
| `createTenant`, `updateTenant`, `suspendTenant`, `resumeTenant`, `deleteTenant` | directory | `POST /tenants`, `PATCH /tenants/{id}`, `POST …/suspend`, `…/resume`, `DELETE` | `TenantService.Create`, `Update`, `Suspend`, `Resume`, `Delete` | `directory.tenants` |
| `optInCluster`, `optOutCluster` | directory | `PUT/DELETE /tenants/{id}/clusters/{cluster}` | `TenantService.OptIn`, `OptOut` | `tenant.clusters` |
| `listTenantBindings`, `setTenantBinding`, `deleteTenantBinding` | directory | `GET/PUT/DELETE /tenants/{id}/bindings` | `TenantService.ListBindings`, `SetBinding`, `DeleteBinding` | `tenant.members` |
| `listProjects`, `createProject`, `updateProject`, `deleteProject` | directory | `GET/POST /projects`, `PATCH/DELETE /projects/{id}` | `ProjectService.…` | `tenant.projects` / project roles |
| `listConnectors` … `revokeScimToken` | directory | `/tenants/{id}/connectors…`, `/connectors/{id}/…` | `ConnectorService.…` | `tenant.read` / `tenant.directories` |
| SCIM 2.0 | directory | `/scim/v2/c/{id}/…` | — (RFC 7644) | SCIM token |
| `listFederations` … `deleteFederation` | directory | `/tenants/{id}/federations` | `FederationService.…` | `tenant.directories` |
| `listUsers`, `listTeams` | directory | `GET /users`, `/teams` (+ `?tenant=`) | `TenantService.ListUsers`, `ListTeams` | own tenant; `directory.read` for all |
| `listFleetClusters`, `createFleetCluster`, `removeFleetCluster` | directory | `GET/POST /fleet/clusters`, `DELETE …/{id}` | `FleetService.…` | `directory.read` / `directory.clusters` |
| `listAdmissions`, `admitTenant`, `updateAdmission`, `removeAdmission` | cluster | `GET /admissions`, `PUT/PATCH/DELETE /admissions/{tenant}` | `AdmissionService.…` | `cluster.admissions` (tenants see their own) |
| `readFleet` | cluster | `GET /fleet` | `FleetService.Status` | authenticated |
| resources (VMs, disks, networks, …) | cluster | unchanged paths | per grpc.md §9 | unchanged + `useCluster` (§10.4) |
| `readAudit`, `readUsage` | both | `GET /audit`, `GET /usage` (+ `?tenant=`) | with their families | + `tenant.audit` / `tenant.usage` for the tenant's entries there |

**HTTP-only, on purpose:** browser redirects, the OIDC provider
endpoints and SCIM are fixed by their protocols. They still go through
the service layer (§14.1).

**Lists** take `page_size` (≤ 500, default 100) and `page_token` on both
transports.

Errors: `409 managed_by_connector`, `409 tenant_in_use`, `403
tenant_suspended`, `403 not_admitted` (useCluster), `403
tenant_quota_exceeded`, `422 invalid_connector`, `409 sync_blocked`,
`503 directory_unavailable`, `401 directory_stale` (past
`max_staleness`).

### 14.1 Service layer (both transports)

1. **Operations are transport-neutral functions** in
   `src/service/{tenancy,directory,login,fleet}.rs`, taking a `Call`
   (principal, Cedar action, request id, audit slot; grpc.md §6) and a
   proto request, returning a proto response or a `ServiceError`.
2. **All decisions live in the service function**: Cedar requests,
   not-found versus forbidden, caps, names, the link boundary,
   managed-field checks, secrets, the store write. Adapters only
   decode, call one function, and encode.
3. **Types come from proto** (`crates/glidex-api/proto/`), REST using
   their JSON form.
4. **Errors are one enum with stable codes** mapped to both transports
   (grpc.md §7).
5. **One operation, one Cedar action, one audit entry**, whichever
   transport.
6. **Secrets never appear in response messages** (grpc.md §4 check).
7. **Reads and writes are declared per operation** (grpc.md G-D6).
8. **Directory writes go through `service::directory` only**, which on a
   cluster that isn't the directory answers `directory_is_elsewhere`
   with the directory's URL. One module, so a fleet of one and a fleet
   of many run the same code.

`control-plane.json`: `auth.pam` and `auth.oidc` keep working as
file-defined connectors (`source: file`, read-only through the API).
New: `fleet.max_staleness`, `fleet.access_token_ttl`,
`tenancy.max_tenants`, `directory.sync_interval_min`.

## 15. UI and CLI

**UI** ([web-ui.md](web-ui.md)):
- Login: at the directory, one field ("work email or organization"),
  then the tenant's connector buttons; a cluster's UI sends the browser
  there and back (§7.1).
- Directory owner: **Tenants** (create with domains, tags, operator
  flag; suspend), **Fleet** (clusters, their operators, feed lag).
- Operator, per cluster: **Admissions** (tenants, caps), site policies,
  plus today's cluster pages.
- Tenant admin: **Directories** (connector wizard per preset with the
  URLs to paste, write-only secrets, *Test*, *Sync now*), **Teams**,
  **Projects** (with per-cluster quotas and where each project has
  resources), **Clusters** (opt-ins and each operator's admission),
  **Members**, **Federations**.
- Lists of VMs, disks and networks show their cluster.

**CLI** ([cli.md](cli.md)):
```
gxctl auth login --directory <url> --tenant <slug> [--connector <id>]
gxctl tenant list|show|create|update|suspend|resume|delete|opt-in|opt-out
gxctl directory list|show|add <kind>|update|secret set <field>|test|sync|remove
gxctl directory scim-token create|list|revoke
gxctl federation list|add|remove
gxctl fleet status|clusters|join-token|join|leave|promote
gxctl admission list|admit|update|remove        # on a cluster
```

## 16. Migration

On the first start with the tenancy schema (one write), a host becomes a
fleet of one that is its own directory:

1. Create `Fleet::"<new id>"`, this cluster as its directory, and tenant
   `admin` (`owns_directory`, `operator`), the cluster's operator.
2. Create tenant `default`, opted in and admitted here with an
   unlimited cap; every project moves to it (project quotas become this
   cluster's entry), with its guest credentials.
3. Users who hold a system-role or grant link, or are `glidex-admin`
   peers, go to `admin`; every other user to `default`. Teams go to
   `default` unless all members are in `admin`.
4. Links on the `Cluster` stay as this cluster's links. Holders of
   `role.system-admin` also get `directory-admin` on the `Fleet` and a
   delegated `tenant-admin` on `default` (D13), so they keep managing
   users and projects as before.
5. `auth.pam` becomes a file-defined PAM connector bound to this
   cluster, in `admin`; operators who want `glidex-users` in `default`
   move it (`gxctl directory add pam --tenant default --group
   glidex-users`). `auth.oidc` becomes a file-defined OIDC connector in
   `auth.oidc.tenant` (default `default`); `oidc:<issuer>` identities are
   re-keyed to `conn:<id>`.
6. API tokens become directory tokens valid on this cluster.
7. Name indexes are rebuilt per tenant (no conflicts are possible).

Nothing a user does today changes: one login page, the same tokens, the
same rights.

## 17. Testing

- **Policy decisions** (table-driven, security.md §14 style), with two
  clusters A (operator `admin`) and B (operator `dc-north`) in the
  entity set:
  - a `tenant-admin` of acme reads acme's VMs in A and B and nothing of
    lab's (`base.tenant-boundary`);
  - `system-admin` on A, held by an `admin` principal, reaches acme's
    VM in A but not in B, and no acme project or credential record;
  - a `dc-north` principal with `role.net-admin` linked on A (inserted
    into the store directly) is denied (`base.host-operator`);
  - `useCluster` on B for lab is denied until B admits lab **and** lab
    opts in (`base.admission`); a site `forbid` by tag beats both;
  - a directory-owner principal with a delegated `tenant-admin` on acme
    passes; without the link, it doesn't;
  - directory-scoped actions are denied to operators that don't own the
    directory; a suspended tenant gets `base.tenant-suspended`.
- **Route coverage:** every action is in exactly one marker group.
- **Link boundary:** each of the four link kinds refuses a principal
  from the wrong tenant.
- **Names, caps, project quotas per cluster.**
- **Connectors, per protocol** against local mocks (OIDC with
  `tid`/`hd`/`oid`, Entra group overage; GitHub org membership; LDAP
  StartTLS and `objectGUID`; MFA required without `amr`).
- **SCIM:** Entra and Okta request sequences as fixtures; `active:
  false` revokes within the request; a token of connector X can't touch
  Y; limits.
- **Pull sync:** a failing API changes nothing; the safety stop blocks a
  mass disable; delta then full resync agree.
- **Secrets:** no connector read or feed ever contains secret bytes;
  store, snapshot and `node.db` contain no plaintext.
- **Federation:** wrong repository, wrong `aud`, expired token and a
  rule without a pinning claim are refused; tokens expire at `ttl_secs`
  and work only on the rule's clusters.
- **Fleet (T6),** a directory and two clusters in process:
  - a cluster's feed holds only tenants admitted on both sides, never a
    connector secret;
  - an access token for A is refused by B and by the directory;
  - a refresh token is never sent to a cluster;
  - logout at the directory ends the cluster sessions as the feed
    delivers it;
  - a partitioned cluster keeps serving per §7.6, then refuses directory
    credentials past `max_staleness` while PAM and peer still work;
  - promote with a new epoch fences the old directory.
- **Both transports:** every API test is table-driven over `{rest,
  grpc}` (grpc.md §12).
- **Migration:** a pre-tenancy database migrates per §16 and every
  existing API test passes unchanged.

## 18. Milestones and work plan

Each milestone ships its API, CLI, UI and tests together and leaves
`main` releasable.

| # | Scope | Acceptance |
|---|---|---|
| **T0** tenants, operators, admission | needs grpc.md **G0**. The model for a fleet of one (§10 entities incl. `Fleet`, `Cluster.operator`, `Vm in [Project, Cluster]`, `context.cluster`), table scopes, `tenants` and `admissions`, projects and guest credentials as directory records, `useCluster` and `base.admission`, the operator and directory-owner boundaries, tenant and directory roles, link boundary, caps and per-cluster project quotas, names, `tenant_grants`, migration (§16 steps 1–4, 6–7), tenant/project/admission APIs on both transports through `service::directory`, `gxctl tenant`/`admission`, UI Tenants, Admissions and cluster columns | §17 policy, route-coverage, link, names, caps and migration tests on both transports; existing suites green. |
| **T1** connector framework | `connectors`, `connector_secrets` + KEK, the `ExternalIdentity` login path, discovery, per-connector routes, file-defined PAM/OIDC connectors (§16 step 5), PAM bound to its cluster (D21), D20 rules, D18 token clamp, `gxctl directory`, UI Directories (PAM, OIDC) | Existing OIDC and PAM tests pass through connectors; secrets and peer tests. |
| **T2** OIDC presets and GitHub | `google`, `azure`, `jumpcloud`, `oidc` presets; MFA; GitHub OAuth + device flow; connector test | Mock tests per preset; manual checks against real Google, Entra, GitHub, JumpCloud. |
| **T3** SCIM | §8.1, managed users and teams, D18, field ownership | SCIM fixtures; deprovision-revokes test. |
| **T4** pull sync and attributes | `directory_sync`, LDAP, Google Directory, Graph, GitHub org/teams, safety stop; SSH keys from connectors | Pull sync tests; LDAP against OpenLDAP in CI. |
| **T5** workload federation | `federations`, `POST /auth/exchange`, `jwt` and `aws` kinds | Federation tests; a GitHub Actions workflow deploys to a test cluster without a stored token. |
| **T6** fleet | the directory as OIDC provider (§7), access and refresh tokens, cluster sessions from ID tokens, fleet join with operators, the filtered feed and admission reports, partitions and staleness, standby and promote, `gxctl fleet`, UI Fleet | §17 fleet tests; a check across two datacenters with a real WAN link. Can start once T1 has landed. |

**T0 tasks** (files):
1. `tenancy.rs`: `Tenant`, `Admission`, stores; `Project.tenant_id` and
   per-cluster quotas; caps in `overruns` and `state.rs::usage_locked`.
2. `store/db.rs`: `Tenants` and `Admissions` table ids,
   `TableId::scope()`; the fleet id in `meta`; guest credentials marked
   directory-scoped; migration beside `adopt_into_default_project`.
3. `auth/store.rs`: `tenant_id` on `User`, `Team`, `Token`; `Token.clusters`;
   `MemberSource` = `Manual | Connector(String)` with serde aliases.
4. `policies/`: §10.1–§10.2; `authz.rs` adds `context.cluster` to every
   request and the `Fleet`, `Tenant` and `Cluster` entities.
5. Protos `glidex/tenancy/v1/{tenants,projects,admissions}.proto`;
   `src/service/{tenancy,directory}.rs`; thin adapters in `api/` and
   `api/rpc/`; route-coverage marker check.
6. `api/vms.rs`, `api/storage.rs`, `api/net.rs`: `useCluster` in
   compound requests; `tenant_grants`; `base.share-same-tenant`.
7. gxctl `tenant` and `admission`; UI Tenants, Admissions, cluster
   columns (bun).
8. `tests/tenancy_tests.rs`; spec updates in security.md §6–§7,
   rest-api.md, cli.md, web-ui.md, data-model.md.

**T1 tasks** (files):
1. `src/directory/` (`mod.rs` connector model and validation,
   `secrets.rs`, `pam.rs`, `oidc.rs` moved from `auth/oidc.rs`).
2. `auth/mod.rs`: `ExternalIdentity` path; `Principal.tenant_id`; peer
   and PAM resolution; token clamp.
3. `config.rs`: file-defined connectors.
4. `cluster/`: KEK transfer to joining directory servers, exclusion
   from snapshots and node caches, rotation with the CA.
5. `glidex-install`: the `secrets-kek` credential.
6. Protos `glidex/directory/v1/*`, `glidex/auth/v1/login.proto`;
   `src/service/{directory,login}.rs` and adapters; `gxctl directory`;
   UI Directories.

T2–T6 get their own task lists when T1 lands; per-protocol code under
`src/directory/<protocol>.rs`, presets under `src/directory/presets/`.

## 19. Out of scope

- One person in several tenants with one login (D2).
- Tenant-authored Cedar policies (D25).
- Tenant-private images; per-tenant metering prices.
- L2 networks or disks spanning clusters; moving a VM between clusters.
- Acting as an IdP for guest VMs (SSH certificates from directory
  identities), a follow-up of T4's SSH keys.
- SCIM outbound.
- SAML (D27).
- Tenant self-service signup (D28).

## 20. Later designs

- **SPIFFE/SPIRE** for workload and agent identity: SPIFFE JWT-SVIDs fit
  federation's `jwt` kind (§9) as one more trusted issuer; SPIRE could
  issue node identities in place of the cluster CA's node certificates
  (clustering.md §12.1).
- **GPUs, DPUs and other server-attached accelerators**: modelling,
  scheduling and granting them to tenants; today's PCI grants (D23) stay
  as they are.
- **Fleet-wide views**: a tenant's resources, usage and audit across
  clusters in one place (each cluster serves its own today).

## 21. Resolved questions

1. **SAML:** not supported (D27).
2. **Default tenant on upgrade:** existing projects go to `default`
   (D29, §16).
3. **Tenant creation:** the directory owner only (D28).
4. **DPUs and accelerators:** later (§20).
5. **Tenants across clusters:** one directory for the fleet, clusters
   as independently operated providers (D3, D10).
6. **Projects across clusters:** yes, a project's resources can be in
   any cluster that admits its tenant (D4); projects never span tenants
   (D1).
7. **One login:** at the directory, accepted by every admitting cluster
   (D9, §7).
8. **Different operators:** each cluster has its own operator tenant
   and site policies, and admits tenants itself (D5, D7, D8).

## 22. Open questions

1. **Directory availability across datacenters.** New logins and new
   access tokens need the directory (§7.6). Its own Raft cluster could
   place voters in three datacenters (its write rate is low, but
   clustering.md assumes a LAN), or keep one datacenter plus a standby
   (§12). Which do you expect to run?
2. **Who may be the directory owner** when operators are different
   organizations: one of them, or a separate party none of them
   operate hardware for?

# Tenants and directory connectors (design)

Status: **design**, nothing implemented. Builds on
[security.md](security.md) §5–§7 (authentication, projects and teams,
Cedar) and [clustering.md](clustering.md) §12 (cluster authorization).
It lifts the "federation and SCIM provisioning" item of
[security.md §12](security.md#12-out-of-scope).

This document adds one level above projects, the **tenant**, and lets
every tenant bring its own users from **directory connectors**: PAM,
LDAP, OIDC, Google, Microsoft Entra ID (Azure), GitHub, JumpCloud and
SCIM. One tenant is special: the **admin tenant** owns the physical
cluster (nodes, OVN, host networks, PCI devices, images, cluster
membership). Every other tenant only ever sees
its own projects and whatever the admin tenant grants it.

A tenant is not tied to one cluster. Several clusters, for example one
per datacenter, form a **fleet** that shares one tenant directory: the
same tenants, users, teams and connectors in every cluster, with each
project and its VMs in one cluster (§13.2). A standalone cluster is a
fleet of one.

## 1. Starting point (17946fc)

| Area | Today | Problem for multi-tenancy |
|---|---|---|
| Hierarchy | `Project in Cluster`; teams and users are global (`security.md` §6.1). | No boundary between groups of projects: whoever holds `system.identity` or `system.projects` manages everyone, and nobody else can manage users at all. |
| Login sources | One PAM config and **one** OIDC issuer in `control-plane.json` (`config.rs::OidcConfig`). | A second organization can't bring its IdP; changing the IdP needs a root-edited file and a restart. |
| Provisioning | JIT at login only; team sync from the `groups` claim at login. | A user removed from the IdP keeps their glidex session and **personal tokens (up to 365 days)** until they expire. |
| Names | Project, disk and network names are unique cluster-wide. | A name collision tells one tenant that another tenant's object exists. |
| Grants | Host networks and PCI devices are granted to projects. | The admin has to re-grant for every new project of a customer. |
| Machine identity | Service-account tokens are long-lived bearer secrets. | CI pipelines store `gxt_` secrets instead of using the OIDC/STS identity they already have. |
| Secrets | The only IdP secret is the systemd credential `oidc-client-secret`. | Per-tenant connectors created through the API need secrets stored by the control plane. |

## 2. Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| D1 | Hierarchy | `Fleet ⊃ Cluster` and `Fleet ⊃ Tenant ⊃ Project`, each project also in exactly **one** cluster. Every project, team, user, service account and connector belongs to **exactly one** tenant. | One boundary that Cedar can express as `resource in principal.tenant`, instead of per-resource rules; tenants above clusters so one tenant can use several (D23). |
| D2 | Users | A user belongs to one tenant (its *home tenant*). The same person in two organizations is two users. | Keeps identity keys, provisioning and deprovisioning per tenant; no cross-tenant identity merging to get wrong. Admin-tenant users are the one exception that may hold links anywhere (D4). |
| D3 | Admin tenant | Fixed id `admin`, created at migration, can't be deleted or suspended. Only its principals pass the **host-scoped** actions (every `host.*`, `image.manage`, `policy.*`, `system.*`, `quota.exceed`, `node.drain`, `host.cluster*`). Enforced by a `forbid` in `base.cedar`. | A role link mistakenly given to a customer's team can't hand out the cluster. Roles still decide *which* admin may do what; the tenant only bounds *who can ever*. |
| D4 | Operator access | Admin-tenant principals may be linked to roles in any tenant's projects (support, operations). Other principals may only be linked inside their own tenant. Every action by an admin-tenant principal on another tenant's resource is visible in **that tenant's** audit view. | Operators need to help customers; customers need to see when they did. |
| D5 | Tenant administration | New role `tenant-admin` on a `Tenant`: projects, quotas inside the tenant cap, teams, users, links and connectors of that tenant. New role `tenant-auditor`: read everything in the tenant, its audit log and usage. | Customers run their own organization without asking the cluster operator for each project or user. |
| D6 | Connectors | A **connector** is one configured external source, owned by a tenant. It may do any of: **login** (the user proves who they are), **provisioning** (users and groups are created, updated and disabled ahead of login), **attributes** (SSH public keys, display name, email). Presets (Google, Entra, GitHub, JumpCloud) are fixed configurations of four underlying protocols: PAM, LDAP, OIDC/OAuth 2.0, SCIM 2.0. SAML is not one of them (D20). | Every "provider" is a preset, not a new code path; the security-critical parsing (ID tokens, SCIM) exists once. |
| D7 | Identity key | `(connector_id, subject)`, with the provider's **immutable** subject (`sub`, Entra `oid`, GitHub numeric id, LDAP `entryUUID`/`objectGUID`). Emails and login names are attributes, never keys. `unix:`/`pam:` keys keep the node suffix of clustering.md D13. | Emails get reassigned; GitHub logins get renamed. Keying on them lets a new owner of the name inherit someone's account. |
| D8 | Group → team | Directory groups map to teams of the same tenant (`group_teams`), or auto-create one team per group (`group_mode: auto`, marked `managed_by: <connector>`). Teams carry rights only through role links, as today. Groups are never mapped to roles directly. | One rights mechanism (links), visible in one place; a directory admin can create groups but cannot grant rights the tenant admin didn't link. |
| D9 | Deprovisioning | A user disabled or removed by provisioning is **disabled at once**: sessions deleted, personal tokens revoked, membership in managed teams dropped. The record stays (audit, ownership) until a tenant admin purges it. Connectors with login but no provisioning clamp their users' personal tokens to `token_max_days` (default 30) and re-check the connector at each token-minting login. | Closes the "removed from the IdP, still has a 365-day token" gap. |
| D10 | Connector secrets | Stored in the replicated store **encrypted** with a cluster secrets key (KEK) held like the CA key (systemd credential on each server, never in Raft or snapshots). Write-only in the API. Agents never receive them. SCIM bearer tokens are generated by glidex and stored as SHA-256 only. | The API must accept secrets per tenant; the store, its snapshots and node caches must not leak them in clear. |
| D11 | Admin-tenant connectors | Must restrict who gets in: `jit: false`, or a non-empty `required_groups`, or provisioning. They may require MFA (`require_mfa`, checked from the ID token's `amr`/`acr`), which is on by default for new admin-tenant SSO connectors. Writes to them need `system.identity` + step-up. | An admin-tenant connector that admits "anyone in the IdP" would make every employee of the IdP's organization an admin-tenant user. |
| D12 | PAM and peer identity | Host accounts belong to the cluster operator. A `pam` connector names Unix groups; it is created only by the admin tenant, may be placed in any tenant, and a Unix group belongs to at most one PAM connector. A peer on `api.sock` resolves to the tenant whose PAM connector lists one of its groups; `glidex-admin`/root resolve to the admin tenant; anyone else is refused (as today). `glidex-authd`'s own `allowed_groups` stays the outer fence. | Lets a lab give a class's Unix group its own tenant without letting a tenant admin claim host accounts. The root-owned authd config can't be widened through the API. |
| D13 | Login routing | Users reach their tenant by slug (`/t/<slug>`, `gxctl auth login --tenant`) or by email domain. Domains are **assigned to tenants by a system admin**, not self-claimed. | No DNS verification flow to build, and no tenant can capture another organization's domain. |
| D14 | Names | Names of tenant-owned objects (projects, disks, project networks, credentials, teams, service accounts) are unique **per tenant**: within the cluster for objects that live in one (projects, disks, networks, credentials, project service accounts), across the fleet for directory objects (teams, tenant-level service accounts). Ids stay global UUIDs and flat paths stay (security.md decision 8). Name lookups resolve in the caller's tenant; admin principals pass `?tenant=`. Host networks, images and nodes keep their per-cluster names; tenant slugs are unique in the fleet. | Removes cross-tenant existence oracles without changing any id-based path. |
| D15 | Grants | Host networks and PCI devices can be granted to a **tenant** (`tenant_grants`) as well as to projects. Project networks may only be shared between projects of the same tenant. | One grant per customer, not per project. Cross-tenant L2 sharing stays an admin decision (a host network granted to both). |
| D16 | Quotas | A tenant has a **cap** (same quota names as projects) per cluster, checked in code next to the project quota, against the sum over its projects in that cluster. No fleet-wide sum. Tenant admins set project quotas freely; the cap still binds. Exceeding the cap needs `exceedQuota` on the `Tenant` (system-admin only). | Two levels of the existing mechanism, no new allocation model. |
| D17 | Machine identity | **Workload federation**: a tenant trusts an external JWT issuer (GitHub Actions, GitLab CI, any OIDC workload issuer) or AWS STS identity, and maps matching claims to one of its service accounts; `POST /auth/exchange` returns a short-lived (≤ 1 h) service-account token. | CI uses the identity it already has; no long-lived glidex secret in a CI vault. |
| D18 | Policies stay cluster-wide | Site policies (security.md §7.6) remain an admin-tenant feature. Tenants get roles and teams, not their own Cedar text. | Tenant-authored Cedar would need per-tenant validation scopes and lock-out rules; nothing asked for needs it yet. |
| D19 | Two transports | Every tenancy operation is served over REST/JSON and gRPC from one service function (§12.1, [grpc.md](grpc.md)); only protocol-fixed endpoints (OIDC/OAuth browser redirects, SCIM) are HTTP-only. | The tenancy API is new, so it can be built on the service layer from the start instead of being ported later. |
| D20 | No SAML | Connectors speak OIDC/OAuth 2.0, LDAP, PAM and SCIM only. An organization whose IdP offers only SAML connects through it with an OIDC front (its IdP's own OIDC app type, or a broker such as Dex or Keycloak) as a generic `oidc` connector. | Every IdP listed here also offers OIDC. XML signature verification is a long-running source of authentication bypasses, and the Rust SAML crates either wrap `xmlsec` or are young; not carrying it removes that risk and a milestone. |
| D21 | Tenant creation | Only the admin tenant creates tenants (`system.tenants`). No self-service signup. | glidex runs for one operator and the organizations it hosts; signup would need abuse controls, email verification and quota defaults nobody asked for. |
| D22 | Upgrade | Existing projects move into a `default` tenant; users with host roles move to the admin tenant (§15). | The tenant boundary (D3) holds from the first start after the upgrade, instead of everything sitting in the admin tenant until the operator splits it. |
| D23 | Fleets | A tenant can span clusters. Clusters run by one operator form a **fleet** sharing one **tenant directory** (tenants, users, teams, connectors, tenant-level role links); resources stay in the cluster that runs them (§13.2). | Datacenters fail and partition independently, so each keeps its own control plane; the people and organizations using them are the same everywhere. |
| D24 | One authority, async replicas | One cluster of the fleet holds the authoritative directory in its Raft store; the others keep read-only replicas fed by a change stream and forward directory writes to it. No Raft across clusters. | Raft over WAN links would make every write, in every datacenter, depend on the slowest link (clustering.md D1 assumes a LAN). Directory writes are rare; resource writes, which are frequent, stay local. |
| D25 | Projects in one cluster | A project, its VMs, disks, credentials, networks, routers, project role links and service accounts belong to one cluster, fixed at creation (`cluster_id`). A tenant that wants a presence in two datacenters has a project in each. | A project's resources share L2 networks, local disks and quotas that only exist inside one cluster. |
| D26 | Partitioned members keep serving | A member that can't reach the authority keeps serving existing users and all project operations; it refuses only directory writes and new users. Past `fleet.max_staleness` (default 1 h) without a sync it also refuses logins and personal tokens of users from **provisioned** connectors, since a deprovisioning (D9) may not have reached it. | A datacenter cut off from the authority shouldn't stop running workloads or lock everyone out; but "removed from the IdP" must not stay ineffective indefinitely. |
| D27 | Members are trusted for their tenants only | A tenant lists the clusters it may use (`clusters`). The authority accepts a member's forwarded writes only for tenants placed on that member, never for the admin tenant's fleet-wide links; connector secrets go only to clusters the connector serves. | A compromised member cluster must not be able to rewrite the whole fleet's directory, or read every tenant's IdP secrets. |
| D28 | Sessions and personal tokens per cluster | Logins, sessions and personal tokens stay per cluster (gxctl already keeps one profile per cluster, gxctl-auth.md). The user and its rights are the same everywhere. | Fleet-wide tokens would make every token check depend on the authority or on replication lag for revocation. One sign-on across clusters is a later step (§20). |

## 3. Threat model additions

| Actor | Trusted for |
|---|---|
| Admin-tenant system administrators | Unchanged (security.md §3), plus every tenant: of their cluster when linked on a `Cluster`, of the fleet when linked on the `Fleet`. |
| Tenant administrators | Their tenant's projects, users, teams and connectors, inside its cap. Nothing on hosts; nothing in other tenants. |
| A tenant's directory (IdP, LDAP, SCIM client) | Asserting identities and group memberships **for that tenant only**. A compromised customer IdP yields at most that tenant's rights. |
| The admin tenant's directory | Admin-tenant membership. Combined with role links, the cluster: hence D11. |
| CI issuers trusted by a federation | Claims matching that federation's rules, mapped to one service account. |
| The fleet's authority cluster | The directory of every tenant in the fleet. |
| A member cluster | Its own resources, and directory writes for the tenants placed on it (D27). It holds a replica of the directory and the secrets of the connectors that serve it. |

New assets: connector secrets, SCIM tokens, the secrets KEK, provisioning
state. New boundaries: IdP → control plane (SCIM pushes and pull-sync
responses are untrusted input), and tenant ↔ tenant inside one cluster.

**Invariant:** nothing a tenant configures (connector, mapping, team,
link, federation rule) can produce a principal outside that tenant or
an action in the host-scoped group.

## 4. Data model

New replicated tables (added to `store/db.rs::tables!`): `tenants`,
`connectors`, `connector_secrets`, `connector_sync` (cursor and last
result per connector), `scim_tokens`, `federations`. Every table is
marked **fleet** (part of the directory, §13.2) or **cluster**
(`TableId::scope()`), so the change stream and the forwarding of
directory writes know what to carry. Changed records:

| Record | Change |
|---|---|
| `Tenant` (new) | `id` (UUID; `admin` for the admin tenant), `slug` (`[a-z0-9-]{1,32}`, unique in the fleet), `display_name`, `admin: bool`, `suspended: bool`, `domains: [String]`, `clusters: [cluster_id] \| "all"` (D27), `quotas` (cap per cluster: a default plus overrides by cluster id, D16), `project_defaults` (quotas for new projects), `limits {projects, users, teams, connectors}`, `created_at`. |
| `Project` | + `tenant_id` and `cluster_id` (both immutable, D25). Name unique per tenant within its cluster. |
| `User` | + `tenant_id` (immutable), `managed_by: Option<connector_id>`, `external_id` (SCIM), `email`, `disabled_reason` (`manual` \| `deprovisioned` \| `tenant_suspended`). |
| `Identity` | `provider` becomes `unix` \| `pam` \| `conn:<connector_id>`; old `oidc:<issuer>` rows are re-keyed at migration. |
| `Team` | + `tenant_id`, `managed_by: Option<connector_id>`, `external_id`. Name unique per tenant. |
| `TeamMember.source` | `manual` \| `connector:<id>` (old `pam`/`oidc` map to the migrated connectors). |
| `Token` (service account) | + `tenant_id` (from its project, or a tenant-level service account). Project service accounts are cluster-scoped; tenant-level ones are in the directory. |
| `Network`, `PciDevice` | + `tenant_grants: [tenant_id]`. |
| `AuditEntry` | + `tenant` (the actor's) and `target_tenant` (the resource's). |

`Connector`:

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
  "source": "api",                  // api | file
  "created_at": 1760000000
}
```

`settings` is validated per kind (§5). `secrets` are never returned,
only whether each is set and when.

## 5. Connector kinds

| Kind | Login | Provisioning | Attributes | Subject | Notes |
|---|---|---|---|---|---|
| `pam` | `glidex-authd` | JIT at login | `~/.ssh/*.pub` via authd | `<name>@<node>` | D12. Admin-created only. |
| `ldap` | simple bind over LDAPS / StartTLS (never plain) | pull: users and groups by filter | `sshPublicKey`, `mail`, `displayName` | `entryUUID` / `objectGUID` | AD, FreeIPA, OpenLDAP, JumpCloud LDAP-as-a-Service. Bind with the user's DN after a search with the service account; password never stored. |
| `oidc` | Authorization Code + PKCE; device grant for gxctl | JIT; optional SCIM | claims | `sub` | Generic: Keycloak, Okta, Authentik, Auth0, Dex, Zitadel. Today's `auth.oidc` behavior (security.md §5.4) unchanged. |
| `google` | OIDC preset (`https://accounts.google.com`); `hd` must equal one of `settings.domains` | pull: Admin SDK Directory API (service account with domain-wide delegation, read-only scopes `admin.directory.user.readonly`, `admin.directory.group.readonly`) | custom schema field for SSH keys (optional) | `sub` | Google ID tokens carry no groups; group mapping needs pull sync. Without it, `group_teams` is refused. |
| `azure` | OIDC preset (`https://login.microsoftonline.com/<tid>/v2.0`); `tid` checked | SCIM (Entra enterprise app provisioning) or pull via Microsoft Graph (`User.Read.All`, `GroupMember.Read.All`, delta queries) | — | `oid` (not `sub`, which is per-app pairwise) | Group overage (`_claim_names` instead of `groups` past 200 groups) is resolved through Graph when pull is configured, otherwise the login is refused with a clear error rather than mapping no groups. |
| `github` | OAuth 2.0 web flow and device flow (GitHub is not OIDC) | pull: org members and teams via a GitHub App installation | `GET /user/{id}/keys`-style public SSH keys | numeric user id | Login requires membership of `settings.org` (checked at every login and sync). Team mapping by team id. |
| `jumpcloud` | OIDC preset (`https://oauth.id.jumpcloud.com`) | SCIM (JumpCloud's generic SCIM app) or LDAP | via LDAP `sshPublicKey` | `sub` | |
| `scim` | — (provisioning only; pair with a login connector via `login_connector`) | SCIM 2.0 push | — | SCIM `externalId` | For IdPs that provision by SCIM but log in through another connector of the same tenant. |

**Presets.** `google`, `azure`, `github` and `jumpcloud` are stored
with their kind so the UI can show the right fields and checks, but
their code is `oidc` or OAuth 2.0 plus a short validation and
claim-mapping module. Adding a preset (Okta, OneLogin, Keycloak) is a
settings schema and a claim map, not a protocol.

**AWS.** IAM Identity Center offers SAML, not OIDC, to applications like
glidex (D20). AWS users log in through the IdP that Identity Center
itself federates from (Entra, Okta, Google, JumpCloud), connected
directly. AWS *workload* identity (STS) is covered by federation (§8).

**Callback and metadata URLs** are per connector:
`https://<ui>/api/auth/c/<connector_id>/callback` (OIDC/OAuth) and
`/scim/v2/c/<id>/` (SCIM base). The connector page shows
them so the tenant admin can paste them into the IdP.

**Test.** `POST /connectors/{id}/test` checks what can be checked
without a user: discovery and JWKS fetch, LDAP bind with the service
account, Directory/Graph/GitHub API access with
the stored credentials, and reports per-step results. Never returns
secrets or directory contents beyond counts.

## 6. Authentication flow

1. **Discover.** `POST /auth/discover {hint}`: `hint` is a tenant slug or
   an email address. Returns `{tenant: {slug, display_name},
   connectors: [{id, kind, display_name}]}` for an exact slug or an
   assigned domain, otherwise `{connectors: []}` after the same delay.
   Slugs are not secrets (they are in URLs), but unknown domains don't
   reveal which tenants exist.
2. **Login** through the chosen connector. Each connector produces an
   `ExternalIdentity {connector_id, subject, email, name, groups,
   mfa: bool}`. The common path then:
   - refuses if the connector or its tenant is disabled/suspended;
   - looks up `(connector_id, subject)`;
   - if absent: JIT-creates the user in the connector's tenant when
     `jit` and `required_groups`/domain rules pass; refused otherwise
     (same error and timing as a wrong password);
   - refuses `require_mfa` connectors without `mfa`;
   - syncs managed team memberships from `groups` (D8);
   - creates the session or token, stamped with `tenant_id`.
3. **gxctl.** `gxctl auth login --tenant <slug> [--connector <id>]`
   runs the device grant (OIDC presets, GitHub), or the PAM/LDAP
   username-password token flow (`POST /auth/token` with `connector`).
   Every login connector has one of the two, so the CLI never needs a
   browser hand-off of its own. Profiles gain `tenant`
   ([gxctl-auth.md](gxctl-auth.md)).
4. `GET /auth/methods` and `GET /auth/server-info` take `?tenant=`; with
   none they list only the admin tenant's PAM method (the current
   behavior on a standalone host).

`Principal` gains `tenant_id`; Cedar's `Auth` context gains `tenant`
and `mfa: Bool`, so site policies can require MFA for sensitive actions
(e.g. `forbid … host.cluster.critical unless context.auth.mfa`).

## 7. Provisioning

### 7.1 SCIM 2.0 (push)

- Base `https://<cp>/scim/v2/c/<connector_id>/`, TLS only, on the
  control plane's listeners (not the UI). Authenticated by a bearer
  token `gxs_<base62>` minted by `POST /connectors/{id}/scim-tokens`
  (shown once, SHA-256 stored, required expiry ≤ 1 year, several at
  once for rotation). The token authorizes exactly this connector; it
  is not a Cedar principal and can do nothing else.
- Supported: `/Users` and `/Groups` with `GET` (filters `userName eq`,
  `externalId eq`, `displayName eq` only), `POST`, `PUT`, `PATCH` (the
  `add`/`replace`/`remove` operations Entra, Okta and JumpCloud send,
  including Entra's non-standard `"value": "False"` for `active`),
  `DELETE`; `/ServiceProviderConfig`, `/Schemas`, `/ResourceTypes`. No
  bulk, sort, or `/Me`. Pagination by `startIndex`/`count` ≤ 200.
- Mapping: `externalId` → `User.external_id`, `userName` → login
  attribute, `active: false` or `DELETE` → D9 disable, group membership
  → managed team membership. Unknown attributes are accepted and
  ignored. Groups create managed teams only with `group_mode: auto`;
  with `mapped`, unknown groups are stored as connector state and only
  mapped groups touch teams.
- Limits: 20 requests/s per connector, 1 MiB bodies, tenant `limits.users`
  and `limits.teams` enforced with SCIM `409`/`400` errors. Every write
  is audited as `system:scim:<connector_id>` with the tenant.

### 7.2 Pull sync

- Kinds `ldap`, `google`, `azure` (Graph), `github`. Runs on the cluster
  leader as a controller (`directory_sync`), every `sync.interval`
  (default 15 min, minimum 5) and on `POST /connectors/{id}/sync`.
- Each run computes a diff against the connector's managed users and
  teams and commits it as one write set (clustering.md D2), in chunks
  of ≤ 500 changes. Delta tokens (Graph delta, LDAP `modifyTimestamp`)
  are kept in `connector_sync`; a full resync runs daily.
- **Safety stop:** a run that would disable more than
  `sync.max_disable` (default 10 % of managed users, at least 5) is not
  applied; the connector is marked `sync_blocked` and the tenant admin
  confirms or fixes it. Why: a wrong filter or an expired credential
  returning an empty list must not lock out a whole tenant.
- An API error is never read as "no users"; the run fails and keeps
  the previous state.

### 7.3 Ownership of fields

A managed user's display name, email, `external_id` and disabled state
belong to its connector: manual edits return `409 managed_by_connector`.
Role links, default project and manual team memberships stay the
tenant's. Disconnecting a connector (`DELETE`) offers two outcomes:
disable its users (default) or detach them (they become manual users
without a login until linked to another connector).

## 8. Workload federation

```json
{
  "id": "…", "tenant_id": "…", "kind": "jwt",
  "issuer": "https://token.actions.githubusercontent.com",
  "audience": "glidex:<cluster id>",
  "rules": [
    { "match": { "repository": "cmkl/infra", "ref": "refs/heads/main" },
      "service_account": "<token id>", "ttl_secs": 900 }
  ]
}
```

- Kinds: `jwt` (any OIDC-style workload issuer: GitHub Actions, GitLab,
  Kubernetes service-account issuers, Azure managed identity, Google
  service-account ID tokens) and `aws` (a presigned
  `sts:GetCallerIdentity` request, verified by the control plane calling
  STS; match on account and role ARN).
- `POST /auth/exchange` (RFC 8693 shape: `subject_token`,
  `subject_token_type`) → `{access_token, expires_in}`: a token acting
  as the mapped service account, at most `ttl_secs` (≤ 3600), not
  renewable, audited with the matched rule and the external subject.
- Rules match on exact claim values only (no globs on `sub`); every
  rule must name at least one of `repository`/`project_path`/`sub`/
  role ARN, so "any workflow on GitHub" can't be written. The service
  account must belong to the federation's tenant.
- Admin-tenant federations need `system.identity` + step-up.

## 9. Authorization changes (security.md §7)

### 9.1 Schema

```cedarschema
namespace Glidex {
  type Auth = {
    method: String, transport: String, age_secs: Long,
    source_ip?: ipaddr, tenant: Tenant, mfa: Bool,
  };

  // A standalone cluster is a fleet of one (§13.2).
  entity Fleet;
  entity Cluster in [Fleet];
  entity Host in [Cluster];
  entity Tenant in [Fleet] { admin: Bool, suspended: Bool };
  entity Team in [Tenant] { tenant: Tenant };
  entity User in [Team, Tenant] { disabled: Bool, tenant: Tenant };
  entity Token in [Tenant] { owner?: User, expired: Bool, tenant: Tenant };
  // Each project is in its tenant and in the one cluster that runs it (D25).
  entity Project in [Tenant, Cluster] { tenant: Tenant, cluster: Cluster };
  entity Connector in [Tenant] { tenant: Tenant };
  // Vm, Disk, Credential: unchanged (in [Project]).
  entity Network in [Cluster, Project] {
    project?: Project, all_projects: Bool, grants: Set<Project>,
    tenant_grants: Set<Tenant>, shares: Set<Project>,
  };
  entity PciDevice in [Host] { grants: Set<Project>, tenant_grants: Set<Tenant> };

  // Marker groups (like "step-up"): every concrete action is in exactly
  // one of tenant-scoped, host-scoped or shared (route-coverage test).
  action "tenant-scoped";
  action "host-scoped";
  action "shared";

  action "role.tenant-admin" in ["role.system-admin"];
  action "role.tenant-auditor" in ["role.tenant-admin", "role.auditor"];
  action "role.owner" in ["role.tenant-admin"];          // and system-admin, as today
  action "role.viewer" in ["role.operator", "role.auditor", "role.tenant-auditor"];

  action "tenant.read", "tenant.audit", "tenant.usage" in ["role.tenant-auditor", "tenant-scoped"];
  action "tenant.members", "tenant.projects" in ["role.tenant-admin", "tenant-scoped"];
  action "tenant.directories" in ["role.tenant-admin", "tenant-scoped", "step-up"];
  action "system.tenants" in ["role.system-admin", "host-scoped"];

  // Existing permission groups gain a marker, e.g.:
  //   "vm.read", "disk.read", … in [..., "tenant-scoped"]
  //   "host.read", "host.network", "host.cluster", "image.manage",
  //   "policy.*", "system.*", "quota.exceed", "node.drain" in [..., "host-scoped"]
  //   readImage, readNetwork, useNetwork, usePciDevice in ["shared"]
}
```

`Team in [Tenant]` and `User in [Tenant]` make a link on a `Tenant`
cover its teams' members; `Project in [Tenant, Cluster]` makes it cover
its projects in every cluster. A system role linked on a `Cluster`
covers that cluster's hosts and every tenant's projects there, and
nothing in other clusters; linked on the `Fleet` it covers everything.
Existing links on `Cluster` keep their meaning after the upgrade.

### 9.2 Base policies (added)

```cedar
// D1: outside the admin tenant, everything tenant-scoped stays home.
@id("base.tenant-boundary")
forbid (principal, action in Glidex::Action::"tenant-scoped", resource)
unless { principal.tenant.admin || resource in principal.tenant };

// D3: the physical cluster belongs to the admin tenant.
@id("base.host-admin-tenant")
forbid (principal, action in Glidex::Action::"host-scoped", resource)
unless { principal.tenant.admin };

// A suspended tenant's principals can do nothing; its resources can
// only be read and managed by the admin tenant.
@id("base.tenant-suspended")
forbid (principal, action, resource)
when { principal.tenant.suspended };

// D15: project networks are shared within one tenant only.
@id("base.share-same-tenant")
forbid (principal, action == Glidex::Action::"offerNetworkShare", resource)
when { resource has project && resource.project.tenant != context.project.tenant };
```

`base.network-grant` gains `|| resource.tenant_grants.contains(context.project.tenant)`,
and `base.pci-grant` the same. Break-glass (`unix:glidex-admin`)
principals are admin-tenant users, so these policies don't change it.

### 9.3 Rules kept in code

- **Link boundary.** Creating a link checks that the link's principal
  is in the resource's tenant, or in the admin tenant (D4). Cedar
  can't see the *target* principal of a link request without a
  dedicated action shape; a code check next to `project.members` is
  simpler and is covered by tests.
- **Tenant cap** (D16), next to `tenancy.rs::overruns`.
- **Per-tenant name uniqueness** (D14), in the same ReDB transaction as
  the create.
- **Entities per request** now include the principal's `Tenant` (its
  `admin` and `suspended` attributes) and the resource's tenant. A
  missing tenant entity fails closed (Cedar evaluation error → Deny).

### 9.4 Roles

| Role | Link resource | Includes |
|---|---|---|
| `tenant-auditor` | tenant | `viewer` in every project of the tenant, `tenant.read`, `tenant.audit`, `tenant.usage` |
| `tenant-admin` | tenant | `owner` in every project, `tenant-auditor`, `tenant.members`, `tenant.projects`, `tenant.directories` |
| project roles | project | unchanged |
| host roles | cluster / host | unchanged; links refused for non-admin-tenant principals (D3) |

## 10. The admin tenant

Owns, through the existing host roles:

| Area | Actions (unchanged names) |
|---|---|
| Cluster membership | init, join and rejoin tokens, remove, forget, purge, drain, detach, imports, CA rotation, snapshots (`host.cluster*`, `node.drain`) |
| Host networking | OVS install, DPDK, bridges, uplinks, node-scope networks, OVN configuration and edge router (`host.network*`, `configureOvn`) |
| Devices | PCI/VFIO grants (`host.devices`). GPUs, DPUs and other server-attached accelerators get their own design later (§18). |
| Shared library | images (`image.manage`) |
| Platform | site policies, tenants (`system.tenants`), domains, quota caps, audit and usage of every tenant |

It also has projects of its own (platform services running as VMs),
connectors (the operator's IdP, host PAM) and federations, like any
tenant. `system-admin` linked on a `Cluster` covers every tenant's
projects in that cluster; linked on the `Fleet`, in every cluster (D4,
§9.1). Tenant lifecycle and domains are directory writes, made at the
fleet's authority (§13.2).

**Tenant lifecycle** (`system.tenants`):
- *create* `{slug, display_name, quotas, domains}` — optionally with a
  first `tenant-admin` invitation by email that is bound to the first
  login through the tenant's connector with that verified email
  (single-use, 7 days), so the operator never handles a customer password;
- *suspend* / *resume* — `base.tenant-suspended` takes effect on the next
  request; running VMs keep running unless `stop_vms: true`;
- *delete* — refused while the tenant has VMs, disks, networks or
  routers (`409 in_use`); then removes connectors (secrets wiped),
  users, teams, links and tokens in one write. Audit and metering stay
  (clustering.md D19).

## 11. Connector secrets (D10)

- `connector_secrets` rows: `(connector_id, field)` →
  `{kek_id, nonce, ciphertext}` with XChaCha20-Poly1305, AAD =
  `connector_id ‖ field ‖ kek_id`.
- The KEK is 32 random bytes generated at `cluster init` (or first
  start, standalone), stored as the systemd credential `secrets-kek`
  (`systemd-creds`, TPM2 when present) on every server, transferred to
  joining servers over the same mTLS step as the CA key
  (clustering.md §5.2), never written to Raft, snapshots, `node.db` or
  departure bundles.
- Decryption happens only on the leader, in the connector code path,
  into zeroized buffers.
- **KEK rotation** runs with CA rotation (clustering.md §12.5) and on
  demand: re-encrypt all rows under the new KEK in one write.
  *Honest limit:* a departed server held both the KEK and the
  ciphertexts, so the secrets themselves must be considered exposed;
  the leave flow lists the affected connectors and asks their tenants
  to rotate IdP-side secrets.
- File-defined connectors (`source: file`, migrated from
  `control-plane.json`) keep reading `oidc-client-secret` from
  `LoadCredential` as today.

## 12. API

Every tenancy operation is served over **both REST/JSON and gRPC** from
one implementation (§12.1), following [grpc.md](grpc.md). The RPC
services are `glidex.tenancy.v1.TenantService`, `glidex.directory.v1.ConnectorService`,
`glidex.directory.v1.FederationService` and `glidex.auth.v1.LoginService`. The
operation table below is the single list both routers are checked
against (grpc.md §5).

| Operation (Cedar action) | REST | gRPC | Permission |
|---|---|---|---|
| `discover` | `POST /auth/discover` | `LoginService.Discover` | none |
| `startLogin`, `finishLogin` | `GET /auth/c/{id}/start`, `GET /auth/c/{id}/callback` | — (browser redirects) | none |
| `startDevice`, `pollDevice` | `POST /auth/c/{id}/device`, `…/device/poll` | `LoginService.StartDevice`, `LoginService.PollDevice` | none |
| `passwordToken` | `POST /auth/token` (+ `connector`) | `LoginService.PasswordToken` | none (PAM, LDAP; TLS or api.sock only) |
| `exchangeToken` | `POST /auth/exchange` | `LoginService.ExchangeToken` | none (workload token) |
| `listTenants`, `readTenant` | `GET /tenants`, `GET /tenants/{id}` | `TenantService.List`, `TenantService.Get` | `system.tenants` (all) · own tenant for anyone |
| `createTenant`, `updateTenant` | `POST /tenants`, `PATCH /tenants/{id}` | `TenantService.Create`, `TenantService.Update` | `system.tenants` |
| `suspendTenant`, `resumeTenant` | `POST /tenants/{id}/suspend`, `/resume` | `TenantService.Suspend`, `TenantService.Resume` | `system.tenants` |
| `deleteTenant` | `DELETE /tenants/{id}` | `TenantService.Delete` | `system.tenants` (step-up) |
| `listTenantBindings`, `setTenantBinding`, `deleteTenantBinding` | `GET/PUT/DELETE /tenants/{id}/bindings` | `TenantService.ListBindings`, `SetBinding`, `DeleteBinding` | `tenant.members` (tenant roles only) |
| `listConnectors`, `readConnector` | `GET /tenants/{id}/connectors[/{cid}]` | `ConnectorService.List`, `ConnectorService.Get` | `tenant.read` |
| `createConnector`, `updateConnector`, `deleteConnector` | `POST/PATCH/DELETE /tenants/{id}/connectors[/{cid}]` | `ConnectorService.Create`, `Update`, `Delete` | `tenant.directories` (step-up); admin tenant: `system.identity` too |
| `setConnectorSecret` | `PUT /connectors/{id}/secrets/{field}` | `ConnectorService.SetSecret` | `tenant.directories` |
| `testConnector`, `syncConnector` | `POST /connectors/{id}/test`, `/sync` | `ConnectorService.Test`, `ConnectorService.Sync` | `tenant.directories` |
| `listScimTokens`, `createScimToken`, `revokeScimToken` | `GET/POST/DELETE /connectors/{id}/scim-tokens` | `ConnectorService.ListScimTokens`, `CreateScimToken`, `RevokeScimToken` | `tenant.directories` |
| SCIM 2.0 | `/scim/v2/c/{id}/…` | — (protocol fixed by RFC 7644) | SCIM token of that connector |
| `listFederations`, `createFederation`, `updateFederation`, `deleteFederation` | `GET/POST/PATCH/DELETE /tenants/{id}/federations` | `FederationService.List`, `Create`, `Update`, `Delete` | `tenant.directories` |
| `listUsers`, `listTeams`, `listProjects` | `GET /users`, `/teams`, `/projects` + `?tenant=` | `TenantService.ListUsers`, `ListTeams`, `ListProjects` | non-admin principals see their tenant only |
| `createProject` | `POST /projects` | `TenantService.CreateProject` | `tenant.projects` on the tenant (was `system.projects`) |
| `readAudit` | `GET /audit` | (with the audit family, grpc.md §9) | + `tenant.audit` for entries whose actor or target is in the tenant |
| `readUsage` | `GET /usage` + `?tenant=` | (with the usage family, grpc.md §9) | `tenant.usage` for own tenant |

**HTTP-only, on purpose.** Browser redirects (OIDC/OAuth callbacks)
and SCIM are fixed by their protocols and by what IdPs
send; they have no gRPC form. They still go through the service layer
(§12.1), so their checks and audit are the same.

**Lists** take `page_size` (≤ 500, default 100) and `page_token` on
both transports (`?page_size=&page_token=` in REST, response
`next_page_token`). Tenants and connectors can number in the thousands
across a cluster; unpaged lists would have to be changed later on both
transports.

### 12.1 Service layer (both transports)

Today a REST handler authorizes through `Caller` and calls the store
directly (`api/access.rs::create_project`). Tenancy code is written
differently, so gRPC isn't a second copy of it:

1. **Operations are transport-neutral functions** in
   `src/service/{tenancy,directory,login}.rs`:
   `async fn create_tenant(call: &Call, req: CreateTenantRequest) ->
   Result<Tenant, ServiceError>`. `Call` carries the principal, the
   operation's Cedar action, request id, consistency, transport and the
   audit slot; both `api::Caller` and the gRPC layer build one.
2. **All decisions live in the service function**: every Cedar request
   (including compound ones), not-found-versus-forbidden (§7.4 of
   security.md), quota and tenant cap, name rules, the link boundary
   (§9.3), managed-field checks, secret handling and the store write.
   **Adapters only decode, call one service function, and encode.** An
   adapter that calls `Authz`, a store or `VmManager` directly fails
   review; a lint test greps `src/api/` and `src/api/rpc/` for new uses in
   tenancy modules.
3. **Types come from proto.** Request and response messages are
   defined once in `crates/glidex-api/proto/` (grpc.md §4); REST uses
   their JSON mapping with the proto field names (`snake_case`, as the
   rest of the REST API). No hand-written REST twin of a message.
4. **Errors are one enum with stable codes**: `ServiceError { code,
   message, details }` where `code` is the existing REST error string
   (`not_found`, `tenant_suspended`, `managed_by_connector`, …). REST
   maps it to a status and `ApiError`; gRPC to a status code plus
   `google.rpc.ErrorInfo { reason: code }` (grpc.md §7). A new code is
   added to both mappings in the same change, checked by a test.
5. **One operation, one Cedar action, one audit entry** whichever
   transport carried it; the audit entry records `transport: rest |
   grpc`.
6. **Secrets never appear in response messages.** Secret fields exist
   only in write requests (`SetSecret`); responses carry `{set,
   updated_at}`. This is a property of the proto messages, so it holds
   for both transports.
7. **Reads and writes are declared per operation** (`kind: READ |
   WRITE` in the operation table), not inferred from the HTTP method,
   so cluster forwarding (§13) treats a gRPC `POST` correctly.

Errors: `409 managed_by_connector`, `409 tenant_in_use`, `403
tenant_suspended`, `403 tenant_quota_exceeded`, `422 invalid_connector`
(per-field messages), `409 sync_blocked`.

`control-plane.json`: `auth.pam` and `auth.oidc` keep working as
**file-defined connectors** of the admin tenant (or `"tenant": "<slug>"`)
and are listed as `source: file`, read-only through the API.
New: `tenancy.max_tenants` (default 100), `directory.sync_interval_min`.

## 13. Clusters and fleets

### 13.1 Within a cluster

- All new tables are replicated; agents' node caches never include
  `connectors`, `connector_secrets`, `scim_tokens` or `federations`.
- Login, SCIM and exchange endpoints answer on servers; an agent
  forwards them (clustering.md §8) like other API calls. gRPC calls are
  forwarded the same way, whole, using the operation's declared kind
  (§12.1 rule 7, grpc.md §8). Pull sync runs
  only on the leader; a leader change resumes from `connector_sync`.
- Clustering import/detach (`--with-access`, clustering.md §5.8–§5.9)
  carry `tenant_id` on users, teams and projects. An import into a
  cluster maps the source's tenants with `--tenant <src>=<dst>`; an
  unmapped tenant is created only with `system.tenants`.

### 13.2 Tenants across clusters (fleets)

**What is where.**

| Fleet-wide: the directory | Per cluster |
|---|---|
| Tenants (with `clusters`, caps and domains), users, OIDC/LDAP/preset identities, teams and memberships, links on a `Tenant` or the `Fleet`, connectors other than PAM and their secrets, SCIM tokens, federations, tenant-level service accounts | Projects and everything in them (VMs, disks, credentials, networks, routers, project role links, project service accounts), PAM connectors and `unix:`/`pam:` identities (host accounts, D12 and clustering.md D13), links on that `Cluster` or its `Host`s, sessions and personal tokens (D28), audit, metering, site policies, images |

**Authority and replicas (D24).**
- The fleet's **authority** is one cluster; its leader serves the
  directory's change stream, `/fleet/v1/feed`: the write sets that touch
  fleet tables, in revision order (the same write sets clustering.md D2
  replicates, filtered by table scope). Each member's leader follows it
  and applies each batch to its own store in one write, recording the
  authority revision it has reached. A member too far behind gets a
  full directory snapshot, as a lagging node does (clustering.md §8.2).
- A directory write made at a member (a tenant admin edits a team, a
  JIT login creates a user, a connector is changed) is forwarded to the
  authority with the original principal over `/fleet/v1/forward`, the
  way a follower forwards to its leader (clustering.md §6.4), and
  authorized there. The member answers once the change has come back
  on its own feed, so the caller reads its own write.
- SCIM endpoints and pull sync (§7) run at the authority only; their
  changes reach members on the feed.
- Members serve logins from their replica of connectors and identities;
  nothing on the login path waits for the authority except JIT
  creation of a new user.

**Partitions (D26).** A member cut off from the authority keeps serving
existing users and every project operation. Directory writes and JIT
logins answer `503 directory_unavailable`. `GET /fleet` shows the
replica's lag; past `fleet.max_staleness`, users of provisioned
connectors can't log in or use personal tokens there until the member
catches up, while service accounts, PAM and peer users (all
cluster-local) keep working.

**Trust between clusters (D27).**
- Clusters join a fleet with a fleet join token minted at the authority
  (`gxctl fleet join-token`, then `gxctl fleet join` on the member;
  `host.cluster.critical` and step-up on both sides). The token carries
  the authority CA's hash; the member's CA is pinned by the authority at
  join, as cluster joins pin (clustering.md §5.2). The inter-cluster
  channel is mTLS between server nodes of the two clusters, on the
  cluster port.
- The authority accepts forwarded writes from a member only for tenants
  whose `clusters` include that member, and never for links on the
  `Fleet` or admin-tenant records: those are made at the authority.
- Connector secrets go only to clusters a connector serves (its tenant's
  `clusters`, narrowed by the connector's own `clusters` setting). They
  travel on the mTLS feed only, decrypted at the authority and
  re-encrypted by the member under its own KEK (§11).

**Joining with existing tenants.** A cluster that joins a fleet already
has an admin tenant, a `default` tenant and maybe more:
- the admin tenant merges into the fleet's (same fixed id); users map by
  identity key, others are added;
- every other tenant is mapped onto a fleet tenant (`--tenant
  <local>=<fleet>`) or added as a new tenant (a slug that exists in the
  fleet is refused until renamed);
- the authority approves the merge as one write, as an import is
  approved (clustering.md §5.9, D17).

**Losing the authority.** The authority is itself a Raft cluster, so a
node failure changes nothing. Losing its whole datacenter leaves every
member serving from its replica (D26). `gxctl fleet promote` on a member
makes its replica authoritative under a new **fleet epoch**; feeds and
forwarded writes carry the epoch, so an old authority that comes back
can't serve or accept them and rejoins as a member.

**Leaving a fleet.** The leaving cluster keeps a copy of the directory
records of the tenants that still have projects on it, and becomes the
authority of its own fleet of one.

**What users see.** The same tenant, users, teams and roles in every
cluster. A user logs in to each cluster (D28); projects and lists show
their cluster; tenant admins manage the directory from any cluster.
Tenant usage and audit are per cluster; a fleet-wide view is later
(§20).

**Not across clusters:** Raft, L2 networks, disks, VM placement,
project quotas, site policies, the image library. Each cluster stays
what clustering.md describes.

## 14. UI and CLI

**UI** ([web-ui.md](web-ui.md)):
- Login page: one field ("work email or organization"), then the
  tenant's connector buttons; `/t/<slug>` skips the first step.
- Admin tenant: **Tenants** page (list, create with quotas and domains,
  suspend, usage per tenant).
- Tenant settings (tenant-admin): **Directories** with a wizard per
  preset that shows the callback/SCIM/metadata URLs to paste, secret
  fields write-only, *Test* and *Sync now* with per-step results and the
  sync safety stop; **Teams** (managed teams marked with their
  connector); **Projects**; **Members** (tenant role links);
  **Federations**.
- The project switcher lists the caller's tenant's projects; admin
  users get a tenant filter.

**CLI** ([cli.md](cli.md)):
```
gxctl tenant list|show|create|update|suspend|resume|delete
gxctl directory list|show|add <kind>|update|secret set <field>|test|sync|remove
gxctl directory scim-token create|list|revoke
gxctl federation list|add|remove
gxctl auth login --tenant <slug> [--connector <id>]
```

The new `gxctl` families use the gRPC client from `glidex-api`
(grpc.md §10); the UI uses REST until grpc.md's browser decision.

## 15. Migration

On the first start with the tenancy schema (one write):

1. Create the fleet of one (`Fleet::"<new fleet id>"`, this cluster as
   its authority), tenant `admin` (`admin: true`, `clusters: "all"`) and
   tenant `default` (`clusters: [this cluster]`).
2. Every project goes to `default`, with this cluster as its
   `cluster_id`. Users who hold a system-role or
   grant link, or are `glidex-admin` peers, go to `admin`; every other
   user goes to `default`. Teams go to `default` unless all members are
   in `admin`. Existing links stay; links of admin users in `default`
   projects are allowed by D4.
3. `auth.pam` becomes a file-defined PAM connector of `admin` with the
   same `allowed_groups`, `jit` and `group_teams`; a second PAM
   connector for `default` is **not** created automatically — on a host
   where non-admin users log in with PAM, the installer/upgrade notes
   tell the operator to move `glidex-users` to a `default` PAM
   connector (one command: `gxctl directory add pam --tenant default
   --group glidex-users`). Until then those users resolve to `admin`
   with no host roles, so nothing they could do changes.
4. `auth.oidc` becomes a file-defined OIDC connector in the tenant named
   by `auth.oidc.tenant` (default `default`); `oidc:<issuer>` identities
   are re-keyed to `conn:<id>`.
5. Every name uniqueness index is rebuilt per tenant (no conflicts are
   possible: global uniqueness implies per-tenant uniqueness).

A standalone host that never creates a second tenant behaves as today:
one admin tenant, one default tenant, the same login page.

## 16. Testing

- **Policy decisions** (table-driven, security.md §14 style): a
  `tenant-admin` of A can't read B's VM (`base.tenant-boundary`), can't
  create a host network (`base.host-admin-tenant`) even with a
  `role.net-admin` link inserted directly into the store; an
  admin-tenant `system-admin` can; a suspended tenant's owner gets
  `base.tenant-suspended`; offering a network to another tenant's
  project is refused; a `tenant_grants` host network is usable by every
  project of that tenant and none other.
- **Route coverage:** every action is in exactly one of `tenant-scoped`,
  `host-scoped`, `shared`; the test fails otherwise.
- **Link boundary:** `PUT /projects/{id}/bindings` with a principal from
  another tenant → `403`; an admin-tenant principal → allowed and
  visible in the target tenant's audit.
- **Names:** same project/disk name in two tenants succeeds; a create
  in another tenant gives no signal about existing names.
- **Connectors, per protocol** against local mocks: OIDC (existing mock
  IdP cases, plus `tid`/`hd`/`oid` checks, Entra group overage),
  GitHub OAuth (org membership revoked → login refused), LDAP (StartTLS required, bind with wrong
  password, `objectGUID` key), MFA required without `amr`.
- **SCIM:** the Entra and Okta conformance request sequences recorded
  as fixtures; `active: false` revokes sessions and tokens within the
  request; a token of connector X can't touch connector Y; limits.
- **Pull sync:** an empty result from a failing API changes nothing;
  the safety stop blocks a mass disable; delta then full resync agree.
- **Secrets:** a connector read never contains secret bytes; the store
  file, a snapshot and `node.db` contain no plaintext (grep test with a
  known secret); KEK rotation re-encrypts every row.
- **Federation:** a GitHub Actions token for another repository, a
  wrong `aud`, an expired token, and a rule without a pinning claim are
  refused; the minted token expires at `ttl_secs`.
- **Fleet-ready model (T0):** Cedar decisions with two clusters in
  the entity set: `system-admin` linked on cluster A doesn't reach a
  project in cluster B; a `tenant-admin` covers the tenant's projects
  in both; a `Fleet` link covers both. A project's `cluster_id` can't
  change.
- **Fleets (T6),** two in-process clusters: directory changes made at
  either reach the other; a member's forwarded write for a tenant not
  placed on it is refused; a partitioned member keeps serving and
  refuses directory writes, then refuses provisioned users past
  `max_staleness`; a connector's secret never reaches a cluster it
  doesn't serve; promote with a new epoch fences the old authority;
  joining merges the admin tenant and maps `default`.
- **Both transports:** the tenancy API tests are table-driven over
  `{rest, grpc}`: same request → same result, same error code
  (`ApiError.error` = `ErrorInfo.reason`), same audit entry apart from
  `transport`. A test fails for any operation in the table that lacks
  either adapter, except the HTTP-only ones of §12.
- **Migration:** a pre-tenancy database (fixtures from
  `security_tests.rs`) migrates per §15 and every existing API test
  passes unchanged.

## 17. Milestones and work plan

Each milestone ships its API, CLI, UI and tests together and leaves
`main` releasable.

| # | Scope | Acceptance |
|---|---|---|
| **T0** tenants | needs grpc.md **G0** (scaffold, listeners, auth layer, error mapping, operation table); fleet-ready model for a fleet of one (`Fleet` entity, `Project in [Tenant, Cluster]`, `cluster_id`, `Tenant.clusters`, table scopes, every directory write through one service module so T6 can forward it); §4 records, `tenants` table, migration (§15 steps 1, 2, 5), Cedar schema and base policies (§9), tenant roles, link boundary, tenant cap, per-tenant names, `tenant_grants`, tenant API on REST and gRPC through the service layer (§12.1) and `gxctl tenant`, audit `tenant`/`target_tenant`, UI Tenants page and tenant filter | §16 policy, route-coverage, link, names and migration tests, each API test run on both transports; existing suites green. |
| **T1** connector framework | `connectors`, `connector_secrets` + KEK (§11), `ExternalIdentity` common path (§6.2), discovery and per-connector routes, file-defined PAM/OIDC connectors (§15 steps 3–4), PAM per tenant and peer→tenant resolution (D12), D11 rules, D9 token clamp, `gxctl directory`, UI Directories (PAM, OIDC) | Existing OIDC and PAM tests pass through connectors; secrets tests; peer resolution tests. |
| **T2** OIDC presets and GitHub | `google`, `azure`, `jumpcloud`, `oidc` presets with their checks; MFA; GitHub OAuth + device flow; `connectors/{id}/test` | Mock tests per preset; manual check against real Google, Entra, GitHub, JumpCloud tenants. |
| **T3** SCIM | §7.1, SCIM tokens, managed users/teams, D9 deprovisioning, field ownership (§7.3), `scim` kind | SCIM fixtures from Entra and Okta; deprovision-revokes test. |
| **T4** pull sync and attributes | `directory_sync` controller, LDAP, Google Directory, Graph, GitHub org/teams, safety stop; SSH public keys from connectors in `GET /users/me/ssh-keys` | Pull sync tests; LDAP against a containerized OpenLDAP in CI. |
| **T5** workload federation | `federations`, `POST /auth/exchange`, `jwt` and `aws` kinds, `gxctl federation` | Federation tests; a GitHub Actions workflow in this repo deploys to a test cluster without a stored token. |
| **T6** fleets | §13.2: fleet join and leave, directory feed and replicas, forwarded directory writes with D27's checks, secret distribution, staleness (D26), promote with epochs, `gxctl fleet`, UI fleet page. Can start once T1 has landed. | §16 fleet tests; a two-datacenter check with a real WAN link. |

**T0 tasks** (files):
1. `tenancy.rs`: `Tenant` (with `clusters`), `TenantStore`,
   `Project.tenant_id` and `cluster_id`, per-tenant-per-cluster name
   index, tenant cap in `overruns` and `state.rs::usage_locked`.
2. `store/db.rs`: `Tenants` table id and `TableId::scope()` (fleet or
   cluster) for every table; the fleet id in `meta`; migration step in
   `state.rs` beside `adopt_into_default_project`.
3. `auth/store.rs`: `tenant_id` on `User`, `Team`, `Token`; `MemberSource`
   becomes `Manual | Connector(String)` with serde aliases for `pam`/`oidc`.
4. `policies/glidex.cedarschema`, `base.cedar`, `roles.cedar`: §9.1–§9.2
   (`Fleet`, `Cluster in [Fleet]`, `Tenant in [Fleet]`,
   `Project in [Tenant, Cluster]`); `authz.rs` entity builder adds the
   `Fleet` and `Tenant` entities and `tenant`/`cluster` attrs.
5. `crates/glidex-api/proto/glidex/tenancy/v1/tenants.proto`;
   `src/service/tenancy.rs` (every T0 operation, §12.1); thin adapters
   in `api/access.rs` (REST, `?tenant=` filters) and
   `src/api/rpc/tenancy.rs`; the link boundary in the service's binding
   operations; marker and both-transport checks in
   `api/mod.rs::every_route_maps_to_a_schema_action`.
6. `api/net.rs`, `pci.rs`: `tenant_grants`; `base.share-same-tenant`.
7. `bin/gxctl/admin.rs`: `tenant` family; `glidex-ui`: Tenants page,
   tenant column/filter (bun).
8. `tests/tenancy_tests.rs`: §16 T0 cases; spec updates in security.md
   §6–§7, rest-api.md, cli.md, web-ui.md, data-model.md.

**T1 tasks** (files):
1. New module `src/directory/` (`mod.rs` connector model and settings
   validation, `secrets.rs` KEK + AEAD, `pam.rs`, `oidc.rs` moved from
   `auth/oidc.rs` and made per-connector).
2. `auth/mod.rs`: `ExternalIdentity` login path; `Principal.tenant_id`;
   peer resolution through PAM connectors; token clamp.
3. `config.rs`: file-defined connectors from `auth.pam`/`auth.oidc`.
4. `cluster/`: KEK transfer at server join, exclusion from snapshots
   and node caches; KEK rotation hooked to CA rotation.
5. `glidex-install`: generate `secrets-kek` credential on servers.
6. `directory/v1/*.proto`, `auth/v1/login.proto`;
   `src/service/{directory,login}.rs`; REST adapters for discover,
   per-connector login routes, connector CRUD, secrets, test; gRPC
   adapters for all but the HTTP-only routes (§12); `gxctl directory`
   on gRPC; UI Directories on REST.

T2–T6 get their own task lists when T1 lands; the per-protocol code
lives under `src/directory/<protocol>.rs`, presets under
`src/directory/presets/`.

## 18. Out of scope

- One person in several tenants with one login (D2); tenant-to-tenant
  trust.
- Tenant-authored Cedar policies (D18).
- Tenant-private images; per-tenant OVN routers beyond today's
  per-project VPC routers; per-tenant metering prices.
- Acting as an IdP for guest VMs (SSH certificates from directory
  identities) — a natural follow-up of T4's SSH keys.
- SCIM outbound (glidex provisioning other systems).
- SAML (D20).
- Tenant self-service signup (D21).
- SPIFFE/SPIRE. A later option for workload and agent identity: SPIFFE
  JWT-SVIDs fit federation's `jwt` kind (§8) as one more trusted
  issuer, and SPIRE could issue node identities in place of the cluster
  CA's node certificates (clustering.md §12.1). Needs its own design.
- GPUs, DPUs and other server-attached accelerators: modelling,
  scheduling and granting them to tenants (today's PCI grants, D15,
  stay as they are).

## 19. Resolved questions

1. **SAML:** not supported; SAML-only IdPs come in through an OIDC front
   (D20). SPIFFE/SPIRE is a later option for workload and agent identity
   (§18).
2. **Default tenant on upgrade:** existing projects go to a `default`
   tenant (D22, §15).
3. **Tenant self-service signup:** none; the admin tenant creates
   tenants (D21).
4. **DPUs and other accelerators:** deferred to their own design (§18).

## 20. Open questions

1. **Where the authority runs.** One of the fleet's clusters (§13.2), or
   a small dedicated directory cluster that runs no VMs? The design
   works either way; a dedicated one keeps the directory's blast radius
   apart from any datacenter's workloads.
2. **One sign-on across clusters** (D28): a fleet-wide login, a token
   valid in every cluster the tenant uses, and fleet-wide views (all of
   a tenant's projects, usage and audit in one place).
3. **Projects spanning clusters** (D25 says no): needed later for
   multi-datacenter services with one set of role links?
4. **Fleet-wide site policies and images**: per cluster for now.


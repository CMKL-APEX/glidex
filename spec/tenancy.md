# Tenants, directories and operators (design)

Status: **design**; T0, T1 and T2 are implemented on one directory and
one cluster (§23). Builds on
[security.md](security.md) §5–§7 (authentication, projects and teams,
Cedar) and [clustering.md](clustering.md) §12 (cluster authorization).
It lifts the "federation and SCIM provisioning" item of
[security.md §12](security.md#12-out-of-scope).

This document separates the organizations that **use** glidex from the
organizations that **operate** it, and lets each keep its own identity.

- A **tenant** is an organization's account. A **user tenant** owns
  logical resources: projects, and the VMs, disks and networks those
  projects allocate from one or more clusters. An **operator tenant**
  owns physical resources: clusters, their nodes, OVN, host networks,
  devices, images and site policies. One tenant may be both.
- A **directory** is where a tenant's people and logical records live:
  users, teams, connectors to the organization's own IdP (Entra,
  Google, GitHub, JumpCloud, LDAP, OIDC, PAM), and the tenants it holds,
  with their projects, role links and tokens. Every tenant is in exactly
  one directory; users log in at their directory, against their own
  IdP. Tenants that want single sign-on between them share a directory.
- A **cluster** is run by one operator tenant. It **trusts** the
  directories of the tenants it admits (and its operator's), each
  through an explicit handshake, and accepts their users' credentials
  addressed to it. There is no global registry: trust is pairwise,
  between one directory and one cluster.
- A **fleet** brokers that trust at scale. A fleet operator keeps a
  **registry** of directories, tenants and clusters; organizations
  register their own directories and tenants (self-service), operators
  register their clusters. A cluster in a fleet can admit tenants by
  policy, e.g. "any user tenant registered in this fleet", instead of
  one offer per tenant. A cluster belongs to at most one fleet and can
  move to another. A fleet has its **own directory**: fleet admins and
  the fleet's users (the tenant admins and cluster operators who
  register things) log in there, each organization with an **account**
  that owns its registrations (and, later, its consolidated billing).
  It holds none of the registered tenants' users or projects.
- A directory runs on a glidex control plane, the organization's own
  (it need run no VMs) or one an operator **hosts** for it, kept apart
  from every other directory on that host; operators can offer this
  as a service. A standalone host is a
  cluster that hosts one directory, whose `admin` tenant operates it;
  that is what an upgraded host looks like (§16).

```
 directory "acme"  (acme's Entra)          directory "uni"  (uni's LDAP + GitHub)
   tenant acme (user)                         tenant uni-ops (operator)
   users, teams, projects: web, data          tenant cs-lab (user)  ← SSO with uni-ops
        │ trusts / feed                         tenant bio-lab (user)
        │                    ┌──────────────────┘ │ trusts / feed
 ┌──────┴───────────────┐    │     ┌──────────────┴─────────┐
 │ cluster north        │◄───┘     │ cluster south          │
 │ operator: uni-ops    │          │ operator: dc-co (its   │
 │ admits acme, cs-lab  │          │   own directory "dcco")│
 └──────────────────────┘          │ admits acme            │
   acme/web: VMs in north + south  └────────────────────────┘
```

## 1. Starting point (17946fc)

| Area | Today | Problem |
|---|---|---|
| Hierarchy | `Project in Cluster`; teams and users are global (`security.md` §6.1). | No boundary between organizations: whoever holds `system.identity` or `system.projects` manages everyone, and nobody else can manage users at all. |
| Operators | One set of host roles; the operator and the people managing users are the same. | Hardware owned by one organization can't be offered to users of others. |
| Login sources | One PAM config and **one** OIDC issuer in `control-plane.json`. | A second organization can't bring its IdP; changing it needs a root-edited file and a restart. |
| Login scope | Users, sessions and tokens are per cluster. | A user of two clusters is two users with two logins. |
| Provisioning | JIT at login; team sync from the `groups` claim at login. | A user removed from the IdP keeps their session and **personal tokens (up to 365 days)**. |
| Names | Project, disk and network names are unique cluster-wide. | A name collision tells one tenant that another's object exists. |
| Machine identity | Service-account tokens are long-lived bearer secrets. | CI stores `gxt_` secrets instead of using the OIDC/STS identity it has. |
| Secrets | The only IdP secret is the systemd credential `oidc-client-secret`. | Connectors created through the API need secrets stored by the control plane. |

## 2. Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| D1 | Tenant boundary | Every project, VM, disk, network, router, guest credential, service account and federation belongs to **exactly one** tenant. Projects never span tenants. | The tenant is the organizational boundary for resources; Cedar checks it on every request. |
| D2 | Directories | Each tenant is in **exactly one directory**; a directory holds one or more tenants. A directory holds its users, identities, teams, connectors, and its tenants' projects, guest credentials, tenant and project role links, API tokens and federations. | Organizations keep their own identity and their own logical records; nothing about one organization is stored in another's directory. |
| D3 | Users and SSO | A user belongs to one directory and can be a member of any of that directory's tenants (by role links or team membership). Two tenants share logins exactly when they share a directory. A user is never a member of a tenant of another directory. | Single sign-on is a choice the organizations make by sharing a directory, not something glidex arranges across organizations. |
| D4 | User and operator tenants | A **user tenant** has projects; an **operator tenant** (`operator: true`) operates clusters. A tenant may be both. Only members of a cluster's operator tenant pass the **host-scoped** actions there (every `host.*`, `image.manage`, the cluster's `policy.*`, `quota.exceed`, `node.drain`, `host.cluster*`, admissions), enforced by a `forbid` in `base.cedar`. | Physical resources stay under their owner's control; a role link given to anyone else by mistake hands out nothing. |
| D5 | Where records live | **Directory:** users, identities, teams, connectors, tenants, projects, guest credentials, links on a `Tenant` or `Project`, API tokens, federations. **Cluster:** nodes, host networks, devices, images, site policies, admissions, trusted directories, links on the `Cluster` or a `Host`, and the VMs, disks, networks and routers that run there. | Logical records follow the organization; physical ones and what runs on them follow the hardware. |
| D6 | Projects span clusters | A project allocates VMs, disks, networks and routers in any cluster that admits its tenant (D9). Each of those is in one cluster and one project. Project role links apply in every cluster. | A service can run in several datacenters, on several operators' hardware, under one set of roles. L2 networks, disks and placement still stop at a cluster. |
| D7 | Directory owner | Each directory has an **owner tenant** (`owns_directory`). Its members, through directory roles, create the directory's tenants, assign domains and manage its connectors and users. They have no rights on any cluster beyond what its operator gives them. | The organization that runs its identity manages it; that confers nothing on hardware. |
| D8 | Pairwise trust | A cluster trusts a directory only after a **handshake** (§12): the operator offers admission to a tenant with a one-time token; the tenant's admin redeems it at its directory, which pins the cluster and registers it as an OIDC client; the cluster pins the directory. No fleet, no shared registry. | Independent organizations each decide whom they trust; adding a cluster or a directory touches only the pairs involved. |
| D9 | Admission, both sides | A tenant's projects can use a cluster only when the cluster's operator **admits** the tenant (with a cap, D18) and the tenant **opts in** to the cluster (the redeemed handshake). The cluster's site policies can refuse more (`useCluster`). The operator's side is an offer (§12.1) or, in a fleet, an admission policy (D34). | The operator decides whom it serves; the tenant decides which operators see its users and projects. |
| D10 | Per-cluster site policies | Each cluster keeps its own site policies (security.md §7.6), written by its operator. A directory has its own, for directory actions, written by its owner. | Each operator sets its own rules: accepted tenants, console network ranges, device passthrough. |
| D11 | Login at the user's directory | Users log in at their directory, which is an OIDC provider. A cluster accepts directory-issued tokens **addressed to it** (`aud`) from directories it trusts, and checks them against its replica (§7). Cluster-local credentials remain for the operator's host accounts and break-glass. | People authenticate against their own organization's IdP; no cluster holds a tenant's IdP secrets or a credential valid elsewhere. |
| D12 | Feeds, not shared stores | A cluster keeps a read-only replica of what each trusted directory sends it: the records of the tenants admitted on both sides. There is no Raft across clusters or directories and no write forwarding: directory writes are made at the directory with the caller's own credential. | Organizations and datacenters fail and partition independently; a cluster, compromised or not, can't write a directory on anyone's behalf. |
| D13 | Partitions | A cluster cut off from a directory keeps running workloads and serving credentials it can verify; new logins need the directory. Past `max_staleness` for that directory (the operator's setting, default 1 h) the cluster stops accepting its credentials, because a revocation may be missing; cluster-local credentials keep working. | Workloads must not depend on a WAN link; "removed from the IdP" must not stay ineffective indefinitely. Each operator chooses the balance. |
| D14 | Tenant administration | Role `tenant-admin` on a `Tenant`: projects and their per-cluster quotas, members, teams used by the tenant, links, opt-ins, federations. Role `tenant-auditor`: read everything in the tenant, its audit and usage. | Organizations run themselves without asking an operator or the directory owner. |
| D15 | Crossing the boundary | Members of the directory owner may be linked to tenant or project roles of that directory's tenants (delegation). Members of a cluster's operator reach the resources **running on that cluster**, of every tenant, through their cluster roles (support). Both are visible to the tenant and in its audit. Nobody else crosses a tenant boundary, and nobody crosses a directory boundary. | The owner bootstraps and helps its tenants; an operator supports what runs on its hardware; tenants can see both. |
| D16 | Connectors | A **connector** is one configured IdP or user source of a directory. It may do any of: **login**, **provisioning** (users and groups created, updated and disabled ahead of login), **attributes** (SSH public keys, display name, email). Presets (Google, Entra, GitHub, JumpCloud) are fixed configurations of four protocols: PAM, LDAP, OIDC/OAuth 2.0, SCIM 2.0. | Every "provider" is a preset, not a new code path; the security-critical parsing exists once. |
| D17 | Identity key | `(connector_id, subject)`, with the provider's **immutable** subject (`sub`, Entra `oid`, GitHub numeric id, LDAP `entryUUID`/`objectGUID`). Emails and login names are attributes, never keys. `unix:`/`pam:` keys keep the node suffix of clustering.md D13. | Emails get reassigned; GitHub logins get renamed. |
| D18 | Group → team | IdP groups map to teams of the directory (`group_teams`), or auto-create one team per group (`group_mode: auto`, `managed_by`). Teams carry rights only through role links. | One rights mechanism (links), visible in one place. |
| D19 | Quotas | Each admission carries the tenant's **cap on that cluster**, set by the operator. Tenant admins set each project's quota per cluster. Both are checked by the cluster at create, start and resize. Exceeding a cap needs `exceedQuota` from the operator. | Capacity belongs to whoever owns the hardware; the tenant splits its share. |
| D20 | Deprovisioning | A user disabled or removed by provisioning is **disabled at once** in the directory: sessions and tokens revoked, managed team memberships dropped; clusters drop the user's sessions as their feed delivers it. Connectors without provisioning clamp tokens to `token_max_days` (default 30). | Closes the "removed from the IdP, still has a 365-day token" gap. |
| D21 | Connector secrets | Stored **only in the directory**, encrypted with a key of that directory (§13). Write-only. Never in a feed. SCIM bearer tokens stored as SHA-256 only. | Clusters don't need them: logins happen at the directory. |
| D22 | Owner and operator connectors | Connectors of an owner tenant or an operator tenant must restrict who gets in (`jit: false`, non-empty `required_groups`, or provisioning) and require MFA by default (`amr`/`acr`). Google sends no `amr`: a `google` connector there needs `accounts: workspace` and `trust_2sv` (§5.1), and consumer accounts are refused. | A connector admitting "anyone in the IdP" would make every employee of that organization a directory or cluster administrator. |
| D23 | PAM and peer | Unix accounts exist on one cluster's hosts. A `pam` connector belongs to a directory **hosted on that same control plane** and checks that cluster's accounts; PAM and peer logins give sessions valid on that cluster only. Root and `glidex-admin` resolve to the operator tenant. `glidex-authd`'s `allowed_groups` stays the outer fence. | Host accounts can't be a credential for other clusters, or for an organization whose directory lives elsewhere. |
| D24 | Names | In a directory: tenant slugs, and per tenant the names of projects, service accounts and guest credentials. In a cluster: VM names per project, disk and project network names per tenant. Ids stay global UUIDs; flat REST paths stay (security.md decision 8). | No cross-tenant existence oracles; no id-based path changes. |
| D25 | Grants | Operators grant host networks and PCI devices on their cluster to a **tenant** (`tenant_grants`) or to projects. Project networks are shared only between projects of one tenant, within one cluster. | One grant per customer; cross-tenant L2 sharing stays an operator decision. |
| D26 | Machine identity | **Workload federation** at the directory: a tenant trusts an external JWT issuer (GitHub Actions, GitLab CI, any OIDC workload issuer) or AWS STS and maps matching claims to a service account; `POST /auth/exchange` returns short-lived (≤ 1 h) tokens for the clusters named. | CI uses the identity it already has. |
| D27 | Hosted directories | A control plane can host several directories, and operators can offer that as a service. Each has its own issuer URL (`https://<host>/d/<directory>`), signing keys, secrets key, connectors and audit, and none sees another's records. The host's operator can read what it hosts; an organization that won't accept that runs its own directory. | Small organizations shouldn't need to run a server to have an account; larger ones keep full control by self-hosting. |
| D28 | No tenant-authored Cedar | Site policies belong to operators (per cluster) and directory owners (per directory). | Tenant-authored Cedar would need per-tenant validation scopes and lock-out rules. |
| D29 | Two transports | Every operation is served over REST/JSON and gRPC from one service function (§14.1, [grpc.md](grpc.md)); only protocol-fixed endpoints (OIDC/OAuth, SCIM) are HTTP-only. | Built on the service layer from the start. |
| D30 | No SAML | Connectors speak OIDC/OAuth 2.0, LDAP, PAM and SCIM only; a SAML-only IdP connects through an OIDC front. | Every IdP listed offers OIDC; XML signature verification is a long-running source of bypasses. |
| D31 | Creating tenants and directories | A directory's owner creates its tenants. A host's operator creates the directories it hosts, each with its owner tenant and a first-admin invitation. No self-service signup. | Signup would need abuse controls and defaults nobody asked for. |
| D32 | Upgrade | An existing host becomes a cluster hosting one directory; tenant `admin` owns the directory and operates the cluster; existing projects move to tenant `default` in the same directory (so existing users keep one login), admitted on the cluster (§16). | The tenant boundary holds from the first start, and nothing a user does today changes. |
| D33 | Fleets broker trust | A **fleet** is a registry run by a fleet operator: registered directories (issuer and key pins), tenants (with their kind and the fleet's labels) and clusters (endpoint, CA pin, operator, admission terms). Members follow it on a signed feed. The fleet has its own directory (D38) for the people who run and use the registry; it holds none of the registered directories' users, teams, projects or credentials. | Pairwise offers don't scale to many tenants and many operators; a broker that holds registration data and its own users' accounts adds no new place where tenants' users or secrets live. |
| D34 | Admission policies | An operator whose cluster is in a fleet can admit by policy: Cedar site policies on `admitTenant`, evaluated when a tenant of the fleet opts in, create the admission with the policy's default cap and labels. A shipped template, `fleet.open`, admits every user tenant registered in the cluster's fleet. Without a matching policy the opt-in waits for the operator. | "Allow any user tenant of the fleet" becomes one setting instead of one handshake per tenant, and the operator can still narrow it. |
| D35 | Labels by provenance | A tenant carries three kinds of labels, each a separate Cedar attribute: `self_tags` (set by its directory owner), `fleet_labels` (set by the fleet operator, e.g. `verified`), `operator_labels` (set by this cluster's operator on the admission). Site policies say which they trust by naming the attribute. | An operator decides whose word it takes; a tenant can't label itself into a cluster that trusts only fleet or operator labels. |
| D36 | Self-service registration | A fleet user registers, in its **account** (D38), a directory, tenant or cluster and gets a one-time **claim code**; the owner of that directory, the tenant's admin or the cluster's operator redeems it **there**, which sends the registration signed with the directory's or cluster's own key. Fleet admins decide what each account may register (directories, user tenants, operator tenants, clusters) and whether registrations need approval (`approval`, the default, or `open`). Approval is needed once per directory and once per operator tenant: user tenants of an approved directory, and clusters of an approved operator tenant, are active as soon as they are claimed; fleet admins can still suspend any registration. A registered directory's key can afterwards change only by a rotation signed with its previous key, never by the fleet. | Organizations join without a person at the fleet in the loop; holding a fleet account isn't enough to claim someone else's directory or cluster, and a compromised or careless fleet can add new directories but can't take over existing ones. |
| D37 | Moving clusters | A cluster belongs to at most one fleet. Leaving keeps every established admission and directory pin as pairwise trust (the operator may instead end the ones the fleet's policy created, with notice to those tenants); joining another fleet applies that fleet's registry and the operator's policies from then on. Directories and tenants may register in several fleets. | Operators can change brokers without evicting tenants; tenants can use clusters of several fleets. |
| D38 | The fleet's directory and accounts | A fleet runs a directory of its own (the same directory software, D27), owned by the fleet operator's tenant: **fleet admins** are its members. Each organization that uses the fleet has an **account**, a tenant of that directory whose members (**fleet users**: mostly tenant admins and cluster operators of that organization) manage the account's registrations. An account can add a connector for its own IdP, so its people log in with their usual identity. Fleet admins manage accounts and their registry rights. Consolidated billing will attach to accounts (§20). | The registry needs owners and administrators like anything else; reusing the directory and tenant model gives logins, connectors, roles, audit and Cedar boundaries without a second system. |
| D39 | Google accounts | The `google` preset serves **Google Workspace** (`accounts: workspace`, the default) and **consumer Google accounts** (`accounts: any`). Workspace logins are allowed by the ID token's `hd` claim, which must be one of `settings.domains`, never by the email's domain. A consumer connector admits only people the directory already knows: an invitation (D40), an address in `allowed_emails`, or (with `jit: false`) a linked identity. It needs `email_verified`. No tenant claims a public mail domain (`gmail.com`, `googlemail.com`). | Anyone can open a consumer Google account under any email address, so only `hd` proves Workspace membership. Consumer logins are a way in for known people, not signup (D31). |
| D40 | Invitations | A directory owner or a tenant admin invites an **email address** to a tenant, with a tenant role, through one connector. The invitation is a link with a secret. The first login through that connector that brings the secret, with a **verified** email equal to the invited one, takes it: once, within 7 days. Only a hash of the secret is stored. It works with `jit: false`. | A gated connector needs a way in that doesn't need the IdP's subject in advance. The invitation names the tenant and the role up front. |

## 3. Threat model additions

| Actor | Trusted for |
|---|---|
| A directory (and its owner) | The identity of its users and the membership and roles of its tenants. A compromised directory compromises its own tenants' access to every cluster that trusts it, and nothing of other directories' tenants or of any operator's hosts. |
| The host of a hosted directory | What it hosts (D27). |
| An operator | Its clusters' hosts, and the resources of admitted tenants **running there** (D15). Not any directory, not other clusters, not tenants it doesn't admit. |
| A cluster's control plane | Its resources, and the replicas its trusted directories send it: records of tenants admitted on both sides (D9). Never connector secrets (D21), never a credential valid elsewhere (§7.3), no directory writes (D12). |
| Tenant administrators | Their tenant's projects, resources, members, links and opt-ins. Nothing on hosts; nothing in other tenants. |
| An organization's IdP | Identities and groups for the directory connected to it. A compromised IdP yields at most that directory's users' rights. |
| CI issuers trusted by a federation | Claims matching its rules, mapped to one service account. |
| A fleet user | Its account's registrations, as far as the fleet admins allow; never a registration it can't also claim from the directory or cluster itself (D36). |
| A fleet operator | Its registry and its directory: accounts and their rights, which directories, tenants and clusters are registered, their first pins, and `fleet_labels`. A compromised fleet can register rogue directories and tenants (bounded by each cluster's admission policies) and mislabel; it can't change a registered directory's keys (D36), see any user or project, or act on any cluster or directory. |

New assets: connector secrets, SCIM tokens, each directory's signing
keys and secrets key, provisioning state, admissions, trust pins. New
boundaries: IdP → directory (SCIM and pull-sync responses are untrusted
input), directory ↔ cluster (handshake, feed, tokens), and tenant ↔
tenant, directory ↔ directory and operator ↔ operator.

**Invariants:**
- nothing a tenant configures can produce a principal outside its
  directory, give a member of another directory any right, or reach a
  host-scoped action;
- nothing an operator configures affects another cluster or any
  directory;
- a credential a cluster receives is valid on that cluster only;
- a cluster acts on a directory's records only for tenants admitted on
  both sides;
- a fleet can't change the keys of a directory it already registered.

## 4. Data model

Every table is marked **directory** or **cluster** (`TableId::scope()`).
Directory tables carry a `directory_id` key prefix, so one control
plane can host several directories (D27) without mixing them. New
tables: `directories`, `tenants`, `connectors`, `connector_secrets`,
`connector_sync`, `scim_tokens`, `federations`, `trusted_clusters`
(directory side); `trusted_directories`, `admissions`, `feed_state`,
`fleet_membership` (cluster side); `fleet_directories`, `fleet_tenants`,
`fleet_clusters`, `fleet_accounts` (each account's registry rights),
`fleet_claims`, `fleet_settings` (on a control plane that runs a fleet,
next to the fleet's own directory tables).

| Record | Scope | Contents |
|---|---|---|
| `Directory` (new) | directory | `id`, `name`, `issuer` (`https://<host>/d/<id>`), `owner_tenant`, signing key ids, `created_at`. |
| `Tenant` (new) | directory | `id`, `directory_id`, `slug` (`[a-z0-9-]{1,32}`, unique in the directory), `display_name`, `owns_directory`, `operator`, `suspended`, `self_tags` (D35), `domains` (assigned by the owner), `limits {projects, members, connectors}`, `created_at`. |
| `TrustedCluster` (new) | directory | per tenant opt-in: `tenant_id`, `cluster_id`, `endpoint`, `ca_fingerprint`, OIDC client registration, `admitted` (as the cluster reports it), `feed_revision`. |
| `TrustedDirectory` (new) | cluster | `directory_id`, `issuer`, `ca_fingerprint`, JWKS (from the feed), `max_staleness`, `feed_revision`, `last_sync`. |
| `Admission` (new) | cluster | `tenant_id`, `directory_id`, `cap` (D19), `labels` (the operator's, D35), `status` (`offered` \| `pending` \| `admitted` \| `suspended`), `source` (`offer` \| `fleet:<id>`), `by`, `at`. |
| `FleetMembership` (new) | cluster | `fleet_id`, registry endpoint, fleet key pin, `joined_at`, registry revision. At most one. |
| `FleetDirectory`, `FleetTenant`, `FleetCluster` (new) | fleet | each with `account_id` (the account tenant in the fleet's directory that registered it) and the claim's state; directory: `issuer`, key pin, CA pin, `registered_at`, `status`; tenant: `directory_id`, `tenant_id`, `slug`, kind (`user` \| `operator`), `fleet_labels`, `status`; cluster: `cluster_id`, `endpoint`, CA pin, operator tenant, published admission terms (which tenants it admits by policy, default cap), `status`. Public registration data only. |
| `Project` | directory | + `tenant_id` (immutable), `quotas: {cluster_id → quotas}`. Name unique per tenant. |
| `Credential` (guest) | directory | moves from cluster to directory, so a project's guest logins serve its VMs in every cluster. Key `(project_id, username)`. |
| `Vm`, `Disk`, `Network`, `Router` | cluster | + `tenant_id` (from the project, immutable). |
| `User` | directory | + `directory_id` (immutable), `managed_by`, `external_id`, `email`, `disabled_reason`. Tenant membership comes from links and teams (D3). |
| `Identity` | directory | `provider` becomes `conn:<connector_id>`, or `pam`/`unix` with the node in the subject (D23); old `oidc:<issuer>` rows are re-keyed at migration. |
| `Team`, `TeamMember` | directory | + `directory_id`, `managed_by`, `external_id`; `source: manual \| connector:<id>`. |
| `LinkRecord` | both | links on a `Tenant` or `Project` are directory records; links on a `Cluster` or `Host` are cluster records, and take members of the operator only. |
| `Token` | directory | `gxt_` personal and service-account tokens + `tenant_id` (service accounts), `clusters: [cluster_id]` (§7.4). |
| `Network`, `PciDevice` | cluster | + `tenant_grants: [tenant_id]`. |
| `AuditEntry` | both | + `directory`, `tenant` (actor's) and `target_tenant` (resource's). |
| `Connector` | directory | below. |

```json
{
  "id": "6c1e…", "directory_id": "…", "kind": "azure",
  "display_name": "Acme Entra ID",
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
  "cluster": null,                  // pam only: the co-hosted cluster (D23)
  "source": "api",                  // api | file
  "created_at": 1760000000
}
```

`settings` is validated per kind (§5). `secrets` are never returned,
only whether each is set and when.

## 5. Connector kinds

| Kind | Login | Provisioning | Attributes | Subject | Notes |
|---|---|---|---|---|---|
| `pam` | `glidex-authd` on the co-hosted cluster | JIT at login | `~/.ssh/*.pub` via authd | `<name>@<node>` | D23. |
| `ldap` | simple bind over LDAPS / StartTLS (never plain) | pull: users and groups by filter | `sshPublicKey`, `mail`, `displayName` | `entryUUID` / `objectGUID` | AD, FreeIPA, OpenLDAP, JumpCloud LDAP. Bind with the user's DN after a search with the service account; password never stored. |
| `oidc` | Authorization Code + PKCE; device grant for gxctl | JIT; optional SCIM | claims | `sub` | Generic: Keycloak, Okta, Authentik, Auth0, Dex, Zitadel. Today's `auth.oidc` behavior (security.md §5.4) unchanged. |
| `google` | OIDC preset (§5.1). Workspace: `hd` must be one of `settings.domains`; consumer accounts gated (D39) | Workspace: pull via the Admin SDK Directory API (domain-wide delegation, read-only scopes, T4); consumer: none | custom schema field for SSH keys (Workspace, T4) | `sub` | Google ID tokens carry no groups and no `amr`. |
| `azure` | OIDC preset (`…/<tid>/v2.0`); `tid` checked | SCIM (Entra provisioning) or pull via Microsoft Graph (delta) | — | `oid` (not the pairwise `sub`) | Group overage is resolved through Graph when pull is configured; otherwise the login is refused rather than mapping no groups. Later: not in T2 (§18). |
| `github` | OAuth 2.0 web and device flow (not OIDC) | pull: org members and teams via a GitHub App | public SSH keys | numeric user id | Login requires membership of `settings.org`, checked at every login and sync. Later: not in T2 (§18). |
| `jumpcloud` | OIDC preset | SCIM or LDAP | via LDAP `sshPublicKey` | `sub` | |
| `scim` | — (pair with a login connector via `login_connector`) | SCIM 2.0 push | — | SCIM `externalId` | For IdPs that provision by SCIM but log in through another connector. |

**Presets** are stored with their kind for the UI's fields and checks;
their code is `oidc` or OAuth 2.0 plus a short validation and
claim-mapping module. Adding one (Okta, OneLogin, Keycloak) is a
settings schema and a claim map.

### 5.1 Presets in T2

A preset fixes what the IdP fixes (endpoints, scopes, the subject
claim) and checks the rest. Each has a module under
`src/directory/presets/`, over the `oidc` code.

| Preset | `settings` | Fixed | Checks at login | MFA |
|---|---|---|---|---|
| `google` | `accounts` (`workspace` default \| `any`), `client_id`, `domains` (Workspace: one or more), `allowed_emails`, `device_client_id`, `trust_2sv` (Workspace) | issuer `https://accounts.google.com`, scopes `openid email profile` | Workspace: `hd` ∈ `domains` (the `hd=` request parameter is only a hint for Google's account chooser). Consumer: `email_verified`, then an invitation, `allowed_emails`, or an existing identity | `trust_2sv: true` (the operator says the Workspace domain enforces 2-step verification) makes every login count as MFA; otherwise none does. Consumer: none |
| `jumpcloud` | `client_id`, `groups_claim` (the name set under the app's *Include group attribute* in JumpCloud; default `memberOf`), `allowed_emails` | issuer `https://oauth.id.jumpcloud.com/`, scopes `openid email profile` | as `oidc`; an email the token says nothing about counts as verified (the organization sets its users' addresses), `email_verified: false` is believed | `amr`/`acr` as `oidc` |
| `oidc` | as today | — | as today | `amr` `mfa`, or `acr` ∈ `mfa_acr_values` |

- **Two Google clients.** Google allows the device grant (gxctl) only
  for an OAuth client of type *TVs and Limited Input devices*. A
  `google` connector therefore has the browser's client (`client_id`,
  secret `client_secret`) and optionally a device client
  (`device_client_id`, secret `device_client_secret`). Without one,
  device login answers `400 device_flow_unsupported`.
- **A consumer connector that would admit anyone is refused** at
  create and update: `accounts: any` with `jit: true` needs a
  non-empty `allowed_emails`. Invitations work either way.
- **Where users land.** Neither consumer accounts nor Workspace ID
  tokens carry groups, so `group_teams` maps nothing for `google` until
  T4's pull sync. New users join `default_tenant`, or the invitation's
  tenant with its role.
- **Consent screen.** For Workspace, the OAuth client's user type
  *Internal* limits it to the organization on Google's side too. The
  wizard says so; glidex can't check it.
- `domains` and `allowed_emails` are compared case-insensitively.
  `domains` never holds a public mail domain.

**AWS.** IAM Identity Center offers SAML, not OIDC, to applications like
glidex (D30); AWS users log in through the IdP Identity Center itself
federates from. AWS *workload* identity (STS) is federation (§9).

**URLs** are per connector, on the directory:
`<issuer>/auth/c/<connector_id>/callback` and `<issuer>/scim/v2/c/<id>/`.

**Test.** `POST /connectors/{id}/test` checks discovery and JWKS, LDAP
bind with the service account, and Directory/Graph/GitHub API access,
per step. Never returns secrets or directory contents beyond counts.

## 6. Logging in at a directory

1. **Discover.** At a directory, `POST /auth/discover {hint}` with a
   tenant slug or an email address returns the tenant and its
   connectors, or `{connectors: []}` after the same delay. At a
   **cluster**, the same call looks the hint up in the domains its
   trusted directories have sent (§12) and names the directory to go
   to. Unknown hints reveal nothing.
2. **Login** through a connector of the directory. Each connector
   produces an `ExternalIdentity {connector_id, subject, email, name,
   groups, mfa}`. The common path then:
   - refuses if the connector is disabled;
   - looks up `(connector_id, subject)`;
   - if absent: takes the invitation the login brings (its secret, and
     a verified email equal to the invited one, §6.1), else JIT-creates the user in the directory
     when `jit` and `required_groups`/domain/`allowed_emails` rules
     pass; refused otherwise (same error and timing as a wrong
     password);
   - refuses `require_mfa` connectors without `mfa`;
   - syncs managed team memberships from `groups` (D18);
   - creates a **directory session** (browser) or a refresh token
     (gxctl), with `auth_time` and `amr`.
3. The user then works in any tenant of the directory they are a
   member of (D3); the UI and gxctl ask which when there are several.

### 6.1 Invitations (D40)

`{id, tenant_id, email, role, connector_id, invited_by, created_at,
expires_at, secret_hash, taken_by?, taken_at?}`, a directory record.

- **Who.** Tenant admins (`tenant.members`) invite to their tenant with
  any role they may link; directory owners (`directory.tenants`) to any
  tenant of the directory, including the first `tenant-admin` at tenant
  creation (§11). Actions `readTenantInvitations` and
  `manageTenantInvitations`; creating one also takes `manageTenantMembers`,
  and `manageTenantBindings` unless the role is `member`.
- **Role.** `member` (membership only), `role.tenant-admin` or
  `role.tenant-auditor`.
- **Connector.** An enabled `oidc` or `google` one: their logins carry a
  verified email. Into the directory owner's or an operator's tenant,
  only through a connector D22 allows there (as if `jit` were off) that
  requires MFA, so an invitation can't get around D22.
- **The link.** Creating one answers its URL once:
  `<directory>/invite/<id>#<secret>`. The secret is in the fragment, so
  it never reaches a log or a `Referer`. The page (signed out) drops the
  fragment from the address, asks `POST /auth/invitation
  {invitation: "<id>.<secret>"}` what it is for (`404` unless the secret
  matches an open one), and starts the login with `POST
  /auth/c/{id}/start {invitation}`, which answers `{redirect}`. The
  invitation stays in the login's server-side state until its callback.
- **Taking it.** At the login's callback, the invitation is taken when
  its secret matches, it is open (not taken, revoked or expired), and
  the identity's verified email equals `email`. The identity then
  binds to a new user, or to the user it already has. The tenant link
  with `role` is added, and the user is created even with `jit: false`.
  An invitation is never taken by email alone: a login without its
  secret doesn't take it, so a known email can't claim another
  person's invitation.
- **Refusals** look like any refused login (`403 login_not_allowed`).
  The audit says why.
- **Lifetime.** 7 days, at most 30 when created. Revoking deletes it.
  Taken and expired ones stay listed for 30 days, then go.

A connector's own rules still apply: an invitation
through a Workspace `google` connector still needs `hd` ∈ `domains`.

### 6.2 Sign-in methods

A user is reached by its identities (D17), and may have **several**: a
local account, a Google account, a JumpCloud one. Each is a way to sign
in as that user, with the same roles; adding one never replaces another.
Without linking, one person signing in two ways is two users.

- **Adding one** is the user's own act, proving both: signed in, the user
  starts `POST /auth/link/c/{id}/start` (`manageOwnSignIns` on itself, a
  step-up action, so a stolen session can't add its thief's account) and
  signs in at the IdP. The login state carries "add to user X" and the session that
  started it; at the callback (the login state is this browser's, by its
  `Lax` cookie) that session must still be live and X's — its `Strict`
  cookie doesn't come back on the IdP's cross-site redirect — and the connector's
  rules apply as at a login (groups, MFA, `hd`). The identity is added to
  X; it is refused if it is another user's (`taken`), a no-op if X's. The
  browser returns to the page with `?linked=<connector>` or
  `?link_error=taken|not_allowed|not_signed_in`.
- **Removing one:** the user's own with `manageOwnSignIns`, anyone's with
  `manageUsers`; never a user's last identity (`409 last_identity`), so
  nobody is locked out by accident, **unless the user is disabled** (it
  can't sign in anyway): then an administrator may take its last one, so
  that its person can add it to the user they really are (a Google login
  that made a second user before linking existed).
- **Labels:** `pam` is a host account's password ("Local password"),
  `unix` the host's local socket ("Host socket": gxctl on the host, no
  password), `conn:<id>` a connector by its display name. An
  administrator's removal may also end the user's sessions
  (`end_sessions`). Sessions and tokens belong to the user, not the
  identity: without that, they stay valid until they expire or are
  revoked.
- `LinkIdentity` (an administrator naming `pam:`, `unix:` or
  `oidc:<issuer>` and a subject) stays for local accounts; a connector's
  identity is added only by its user signing in.

`Principal` gains `directory_id` and the acting `tenant_id`; Cedar's
`Auth` context gains `mfa: Bool`, so site policies can require MFA
(e.g. `forbid … host.cluster.critical unless context.auth.mfa`).

## 7. Using a cluster

### 7.1 Browser

Each trusted directory registers the cluster as an OIDC client during
the handshake (§12).
1. A browser at a cluster's UI without a session discovers its
   directory (§6) and is sent there (Authorization Code + PKCE). A user
   with a directory session isn't asked again.
2. The cluster receives an ID token from that directory's issuer with
   `aud` = its cluster id, `sub` = the user, `sid`, `auth_time`, `amr`.
   It checks the signature against that directory's keys (from the
   feed), the audience, and that the user is in its replica, enabled,
   and a member of a tenant it admits.
3. It creates a **cluster session** (security.md §5.5: idle 30 min,
   absolute 12 h, bound to the directory and `sid`). Logout or
   revocation at the directory reaches the cluster on the feed and
   ends the session.

### 7.2 gxctl

`gxctl auth login --directory <url> [--tenant <slug>]` runs the device
grant (OIDC presets, GitHub) or the password flow (LDAP) at the
directory and keeps a **refresh token** for it. Before calling a
cluster, gxctl gets an **access token** for it from the directory: a JWT
with `aud` = the cluster id and the claims of §7.1, valid
`access_token_ttl` (default 1 h). The cluster verifies it offline. The
directory lists the clusters the tenant has opted into, with their
endpoints and pins, so one login reaches all of them
([gxctl-auth.md](gxctl-auth.md): profiles gain `directory`).

### 7.3 Why audience-bound

A cluster receives only credentials addressed to it, and a directory
accepts only credentials addressed to itself. A cluster can't replay
what it receives to another cluster or to any directory; refresh tokens
never leave the client and the directory.

### 7.4 API tokens

Personal and service-account tokens (`gxt_`) are directory records
valid on the clusters they list (`clusters`, default the one they were
created for; several, or `all` the tenant's, must be asked for). Their
hashes are in the feeds of those clusters only, so a cluster verifies
them offline. A token listing several clusters can be replayed between
them by a compromised one: the reason the default is one.

### 7.5 Cluster-local credentials

Peer identity on `api.sock` and PAM logins (D23) stay on their cluster.
Root and `glidex-admin` resolve to the operator tenant (break-glass,
security.md §5.2, clustering.md D14 unchanged).

### 7.6 Partitions (D13)

| Lost | Still works | Stops |
|---|---|---|
| Cluster ↔ a directory | Running VMs, all project operations of its users; cluster sessions until they end; access tokens until `exp`; API tokens; cluster-local credentials | New cluster sessions for that directory's users, feed updates |
| Past that directory's `max_staleness` | Cluster-local credentials, running VMs, other directories' users | That directory's credentials on this cluster |
| User ↔ their directory | Credentials the user already holds | New logins and new access tokens |

One directory's outage affects only its users. Responses carry
`X-Glidex-Directory-Lag` when the caller's directory replica is behind.

## 8. Provisioning (at the directory)

### 8.1 SCIM 2.0 (push)

- Base `<issuer>/scim/v2/c/<connector_id>/`, TLS only. Bearer token
  `gxs_<base62>` minted by `POST /connectors/{id}/scim-tokens` (shown
  once, SHA-256 stored, expiry ≤ 1 year, several for rotation); it
  authorizes exactly this connector and is not a Cedar principal.
- Supported: `/Users` and `/Groups` with `GET` (filters `userName eq`,
  `externalId eq`, `displayName eq`), `POST`, `PUT`, `PATCH` (the
  operations Entra, Okta and JumpCloud send, including Entra's
  `"value": "False"` for `active`), `DELETE`; `/ServiceProviderConfig`,
  `/Schemas`, `/ResourceTypes`. No bulk, sort, or `/Me`. Pages ≤ 200.
- Mapping: `externalId` → `User.external_id`, `active: false` or
  `DELETE` → D20, groups → managed teams. Unknown attributes are
  ignored. Groups create teams only with `group_mode: auto`.
- Limits: 20 requests/s per connector, 1 MiB bodies, directory limits
  as SCIM errors. Every write audited as `system:scim:<connector_id>`.

### 8.2 Pull sync

- Kinds `ldap`, `google`, `azure` (Graph), `github`; a controller on the
  directory host's leader, every `sync.interval` (default 15 min,
  minimum 5) and on `POST /connectors/{id}/sync`.
- Each run diffs the connector's managed users and teams, committed in
  write sets of ≤ 500 changes, with delta tokens in `connector_sync`
  and a daily full resync.
- **Safety stop:** a run that would disable more than `sync.max_disable`
  (default 10 % of managed users, at least 5) is not applied; the
  connector is `sync_blocked` until an owner confirms. An API error is
  never read as "no users".

### 8.3 Ownership of fields

A managed user's display name, email, `external_id` and disabled state
belong to its connector: manual edits return `409
managed_by_connector`. Links and manual team memberships stay the
directory's. Deleting a connector disables its users (default) or
detaches them.

## 9. Workload federation (at the directory)

```json
{
  "id": "…", "tenant_id": "…", "kind": "jwt",
  "issuer": "https://token.actions.githubusercontent.com",
  "audience": "<directory issuer>",
  "rules": [
    { "match": { "repository": "acme/infra", "ref": "refs/heads/main" },
      "service_account": "<token id>", "clusters": ["<cluster id>"], "ttl_secs": 900 }
  ]
}
```

- Kinds: `jwt` (GitHub Actions, GitLab, Kubernetes service-account
  issuers, Azure managed identity, Google service-account ID tokens)
  and `aws` (a presigned `sts:GetCallerIdentity`, verified by calling
  STS; match on account and role ARN).
- `POST /auth/exchange` (RFC 8693 shape) → access tokens (§7.2) as the
  mapped service account, for the rule's clusters, at most `ttl_secs`
  (≤ 3600), not renewable, audited with the rule and external subject.
- Rules match exact claim values only and must name at least one of
  `repository`/`project_path`/`sub`/role ARN. The service account
  belongs to the federation's tenant.

## 10. Authorization (security.md §7)

### 10.1 Schema

```cedarschema
namespace Glidex {
  type Auth = {
    method: String, transport: String, age_secs: Long,
    source_ip?: ipaddr, mfa: Bool,
  };
  // Every request is decided by one control plane, for one cluster or
  // one directory; `cluster` is set on clusters, `directory` on directories.
  type Ctx = { auth: Auth, cluster?: Cluster, directory?: Directory };
  type ProjectCtx = { auth: Auth, cluster?: Cluster, directory?: Directory, project: Project };
  type NetworkCtx = { auth: Auth, cluster?: Cluster, directory?: Directory, network: Network };

  entity Fleet { owner: Tenant };
  entity Directory { owner: Tenant };
  // Labels by provenance (D35); `fleets` and `operator_labels` as this
  // cluster knows them.
  entity Tenant in [Directory] {
    directory: Directory, owns_directory: Bool, operator: Bool,
    suspended: Bool, clusters: Set<Cluster>, fleets: Set<Fleet>,
    self_tags: Set<String>, fleet_labels: Set<String>, operator_labels: Set<String>,
  };
  entity Cluster { operator: Tenant, admitted: Set<Tenant>, fleet?: Fleet };
  entity Host in [Cluster];
  // Membership: a user or team is `in` each tenant it belongs to (D3).
  entity Team in [Tenant, Directory] { directory: Directory };
  entity User in [Team, Tenant, Directory] { disabled: Bool, directory: Directory };
  entity Token in [Tenant] { owner?: User, expired: Bool, tenant?: Tenant };
  entity Connector in [Directory] { directory: Directory };
  entity Project in [Tenant] { tenant: Tenant };
  entity Credential in [Project] { tenant: Tenant, project: Project };
  // What runs somewhere is in its project and in its cluster (D6).
  entity Vm in [Project, Cluster] { tenant: Tenant, project: Project, cluster: Cluster };
  entity Disk in [Project, Cluster] { tenant: Tenant, project: Project, cluster: Cluster };
  entity Network in [Cluster, Project] {
    tenant?: Tenant, project?: Project, all_projects: Bool, grants: Set<Project>,
    tenant_grants: Set<Tenant>, shares: Set<Project>,
  };
  entity PciDevice in [Host] { grants: Set<Project>, tenant_grants: Set<Tenant> };

  // Marker groups: every concrete action is in exactly one of these
  // (route-coverage test).
  action "tenant-scoped";
  action "host-scoped";
  action "directory-scoped";
  action "shared";

  // Directory roles, linked on a Directory.
  action "role.directory-admin";
  action "role.directory-auditor" in ["role.directory-admin"];
  action "directory.read" in ["role.directory-auditor", "directory-scoped"];
  action "directory.tenants", "directory.connectors", "directory.users", "directory.policy"
    in ["role.directory-admin", "directory-scoped", "step-up"];

  // Tenant roles, linked on a Tenant.
  action "role.tenant-admin";
  action "role.tenant-auditor" in ["role.tenant-admin"];
  action "role.owner" in ["role.tenant-admin", "role.system-admin"];
  action "role.viewer" in ["role.operator", "role.auditor", "role.tenant-auditor"];
  action "tenant.read", "tenant.audit", "tenant.usage" in ["role.tenant-auditor", "tenant-scoped"];
  action "tenant.members", "tenant.projects", "tenant.clusters" in ["role.tenant-admin", "tenant-scoped"];
  action "tenant.federations" in ["role.tenant-admin", "tenant-scoped", "step-up"];

  // Cluster roles (auditor, image-admin, net-admin, system-admin), linked
  // on a Cluster or Host, keep their groups and gain the host-scoped marker.
  action "cluster.admissions", "cluster.trust" in ["role.system-admin", "host-scoped", "step-up"];

  // Admission by policy in a fleet (D34): the tenant is the principal.
  action admitTenant
    appliesTo { principal: [Tenant], resource: [Cluster], context: Ctx };

  // The registry (D38). Registrations are resources of the account
  // (a tenant of the fleet's directory) that made them, so the tenant
  // boundary keeps accounts apart; fleet admins are members of the
  // directory's owner tenant and reach them as owners do (D15).
  entity Registration in [Tenant] { tenant: Tenant, kind: String };
  action "role.fleet-admin";
  action "fleet.registry" in ["role.fleet-admin", "directory-scoped", "step-up"];
  action "role.account-admin" in ["role.tenant-admin"];
  action "account.register" in ["role.account-admin", "tenant-scoped"];

  // Every allocation on a cluster (compound request, §10.4).
  action useCluster in ["shared"]
    appliesTo { principal: [User, Token], resource: [Cluster], context: ProjectCtx };

  // Existing groups gain a marker: vm.*, disk.*, credential.*, network.use,
  // project.members … in "tenant-scoped"; host.*, image.manage, policy.*,
  // quota.exceed, node.drain in "host-scoped"; readImage, readNetwork,
  // useNetwork, usePciDevice in "shared". system.identity, system.projects
  // are replaced by the directory and tenant groups above.
}
```

How links reach:
- a project role covers the project's credentials and its VMs and disks
  **in every cluster** (`Vm in [Project, …]`);
- a tenant role covers all the tenant's projects;
- a cluster role covers that cluster's hosts and what **runs on it**
  (`Vm in [… , Cluster]`), of every tenant, but no `Project` or
  `Credential` record;
- a directory role covers that directory's directory-scoped actions.

### 10.2 Base policies (added)

```cedar
// D1, D15: a tenant's resources are its members'; the directory owner
// may be linked in; an operator reaches what runs on its cluster.
// `context.tenant` is the tenant owning the resource, which the control
// plane fills in for every request (a request has one resource).
@id("base.tenant-boundary")
forbid (principal, action in Glidex::Action::"tenant-scoped", resource)
unless {
  context has tenant && (
    principal in context.tenant ||
    principal in context.tenant.directory.owner ||
    (context has cluster && principal in context.cluster.operator && resource in context.cluster))
};

// D4: a cluster's hosts belong to its operator.
@id("base.host-operator")
forbid (principal, action in Glidex::Action::"host-scoped", resource)
unless { context has cluster && principal in context.cluster.operator };

// D7: a directory belongs to its owner.
@id("base.directory-owner")
forbid (principal, action in Glidex::Action::"directory-scoped", resource)
unless { context has directory && principal in context.directory.owner };

// D9: admitted by the operator and opted in by the tenant.
@id("base.admission")
forbid (principal, action == Glidex::Action::"useCluster", resource)
unless {
  resource.admitted.contains(context.project.tenant) &&
  context.project.tenant.clusters.contains(resource)
};
// Who may allocate is decided by createVm & co.; this only checks the place.
@id("base.cluster-use")
permit (principal, action == Glidex::Action::"useCluster", resource);

// D34: a fleet admission policy admits only tenants registered in the
// cluster's fleet; offers (§12.1) don't go through admitTenant.
@id("base.admit-from-fleet")
forbid (principal, action == Glidex::Action::"admitTenant", resource)
unless { resource has fleet && principal.fleets.contains(resource.fleet) };

// A suspended tenant's resources can't be acted on by its members.
@id("base.tenant-suspended")
forbid (principal, action in Glidex::Action::"tenant-scoped", resource)
when { context has tenant && context.tenant.suspended && principal in context.tenant };

// D25: project networks are shared within one tenant only.
@id("base.share-same-tenant")
forbid (principal, action == Glidex::Action::"offerNetworkShare", resource)
when { resource has project && resource.project.tenant != context.project.tenant };
```

`base.network-grant` gains `|| resource.tenant_grants.contains(context.project.tenant)`,
and `base.pci-grant` the same. An operator narrows admission with site
policies, e.g.

```cedar
// The shipped fleet template (D34), enabled with `gxctl fleet join --admit open`.
@id("fleet.open")
permit (principal, action == Glidex::Action::"admitTenant", resource)
when { !principal.operator };

// Only tenants the fleet has verified, and that the operator marked as
// teaching, may allocate here.
@id("site.verified-teaching")
forbid (principal, action == Glidex::Action::"useCluster", resource)
unless {
  context.project.tenant.fleet_labels.contains("verified") &&
  context.project.tenant.operator_labels.contains("teaching")
};
```

A policy that uses `self_tags` trusts the tenant's own directory; one
that uses `fleet_labels` trusts the fleet; `operator_labels` trust only
the operator itself (D35).

### 10.3 Rules kept in code

- **Link boundary.** A link on a `Tenant` or `Project` takes a principal
  of the same directory that is a member of the tenant or of the
  directory's owner tenant (D15). A link on a `Cluster` or `Host` takes
  a member of the cluster's operator. A link on a `Directory` takes a
  member of its owner. Checked where the link is written.
- **Directory of the principal.** A cluster resolves a credential to a
  principal only through the directory that issued it, and refuses a
  principal of a directory it doesn't trust.
- **Caps and project quotas** (D19), next to `tenancy.rs::overruns`.
- **Names** (D24), in the transaction of the create.
- **Entities per request** include the principal with its teams and
  tenants, the deciding `Cluster` or `Directory`, and the resource's
  tenant, project and cluster. A missing entity fails closed.

### 10.4 Compound requests (security.md §7.4)

Every create of a VM, disk, network or router on a cluster adds
`useCluster` on that `Cluster` with the project in the context; so does
a start. A suspended admission refuses creates and starts, not stops or
deletes.

### 10.5 Roles

| Role | Link resource | Includes |
|---|---|---|
| project roles (`viewer` … `owner`) | project | unchanged; cover the project in every cluster |
| `tenant-auditor` | tenant | `viewer` in every project, `tenant.read`, `tenant.audit`, `tenant.usage` |
| `tenant-admin` | tenant | `owner` in every project, `tenant-auditor`, `tenant.members`, `tenant.projects`, `tenant.clusters`, `tenant.federations` |
| cluster roles (`auditor`, `image-admin`, `net-admin`, `system-admin`) | cluster / host | unchanged, for that cluster; `system-admin` adds `cluster.admissions`, `cluster.trust` and its site policies; operator members only (D4) |
| `directory-auditor` | directory | `directory.read` |
| `fleet-admin` | fleet | `fleet.registry`: accounts and their registry rights, approving and suspending registrations, `fleet_labels`, the fleet's settings; members of the fleet directory's owner tenant only |
| `account-admin` | account (a tenant of the fleet's directory) | `account.register` (create registrations and claim codes, within the account's rights), members, the account's connector; `tenant-admin` of the account |
| `directory-admin` | directory | `directory-auditor` (which reads every tenant of the directory, its members and bindings), `directory.tenants` (tenants, operator flags, self tags, domains, a tenant's members and bindings so its first admin can be named, fleet registrations), `directory.connectors`, `directory.users`, `directory.policy`; owner members only (D7) |

## 11. Operators, directory owners and tenants

**An operator** owns, through cluster roles, on each of its clusters:

| Area | Actions (unchanged names) |
|---|---|
| Cluster membership | init, join and rejoin tokens, remove, forget, purge, drain, detach, imports, CA rotation, snapshots |
| Host networking | OVS install, DPDK, bridges, uplinks, node-scope networks, OVN and the edge router |
| Devices | PCI/VFIO grants. GPUs, DPUs and other accelerators: later (§20). |
| Images | the cluster's image library |
| Trust and admission | which directories it trusts and which of their tenants it admits, with caps and its own labels (`cluster.trust`, `cluster.admissions`); its fleet, its admission policies there, moving to another fleet (D34, D37); `max_staleness` per directory |
| Policy | its site policies; audit and usage of what runs on it; support access to it (D15) |

**A directory owner** owns, through directory roles, in its directory:
tenants (*create* `{slug, display_name, domains, self_tags, operator}`,
optionally with a first `tenant-admin` invitation (§6.1); *suspend*/*resume*,
delivered to every cluster by its feed; *delete*, refused while a
cluster reports resources of the tenant), connectors, users, teams,
domains, self tags, its site policies, and its registrations in
fleets (D36).

**A fleet operator** owns the fleet's directory and, through
`fleet-admin`, the registry: accounts and what each may register,
approving or suspending registrations, `fleet_labels`, the fleet's
settings. It has no rights on any registered directory or cluster.

**A fleet user** is a member of an account in the fleet's directory,
usually a tenant admin or a cluster operator of that organization. With
`account-admin` it registers the organization's directories, tenants
and clusters and manages the account's members.

**A tenant admin** owns its projects (with per-cluster quotas), members,
links, opt-ins and federations, and sees every admission, delegated
link and operator support action affecting it.

## 12. Trust between directories and clusters

Trust is always pairwise: a cluster holds a pin for each directory it
trusts, and a directory a pin and an OIDC client for each cluster its
tenants use. It is established either by an offer (§12.1), with no
fleet involved, or through a fleet (§12.2), which supplies the pins
from its registry. Everything after that (feeds, tokens, partitions) is
the same.

### 12.1 Offers and the handshake (D8)

1. The operator offers admission: `gxctl admission offer --tenant <slug>
   --directory <issuer> --cap …` on its cluster
   (`cluster.admissions`, step-up). The cluster records an `offered`
   admission and prints an **offer token**: cluster id, API endpoint,
   the cluster CA's fingerprint, the tenant and directory it is for, a
   one-time secret, expiry (7 days).
2. The tenant's admin redeems it at the directory: `gxctl tenant opt-in
   --offer <token>` (`tenant.clusters`, step-up). The directory checks the
   token is for this tenant, connects to the cluster's endpoint pinning
   the fingerprint, and proves the one-time secret over mTLS with its
   own certificate. The cluster pins the directory's CA and issuer,
   stores the `TrustedDirectory`, and marks the admission `admitted`.
   The directory registers the cluster as an OIDC client, records the
   `TrustedCluster` opt-in, and starts the feed.
3. Either side ends it: the operator removes the admission (no new
   allocations; existing resources stay until the tenant deletes them
   or the operator does, with notice), or the tenant opts out (same,
   from its side). A directory with no admitted tenant left on a
   cluster stops feeding it, and the cluster forgets its records.

**The feed (D12).** For each cluster it has opted-in tenants on, a
directory serves `/d/<id>/feed/v1` over mTLS: its write sets that touch
records of those tenants, in revision order, **filtered for that
cluster**: the tenants, their projects and guest credentials, their
members (id, display name, disabled) and teams, their links, the token
hashes valid on that cluster, revocations, domains, and the directory's
public keys. The cluster applies each batch in one write and keeps the
revision in `feed_state`; one too far behind gets a filtered snapshot.
The cluster reports admissions and the resources it holds per tenant on
the same channel. Connector secrets are never in a feed (D21).

**Directory writes** are made at the directory with the caller's own
credential; the UI on a cluster links to the directory's pages for
them, and gxctl calls the directory. The cluster that served the page
is never in that path.

**An operator's own directory.** A cluster trusts its operator's
directory the same way, through a handshake made when the operator
tenant takes over the cluster (`gxctl cluster set-operator`, by
break-glass or by the current operator), so operator staff log in with
their organization's IdP.

**Losing a directory.** A directory host is a Raft cluster, so losing a
node changes nothing. Losing all of it affects only that directory's
users (§7.6). A directory can keep a **standby**: another control plane
it streams its full contents to (connector secrets excepted, re-entered
after a promote); `gxctl directory promote` there serves the directory
under a new **epoch** at the same issuer name, and clusters accept the
new keys only with the epoch the old directory's last feed announced.

**Not across clusters or directories:** Raft, L2 networks, disks, VM
placement, caps, site policies, image libraries.

### 12.2 Fleets (D33–D37)

**Running a fleet.** A fleet is a role of a glidex control plane, like a
directory (it can share a host with directories and clusters). It has
its own URL, signing key and **directory** (D38). The directory's owner
tenant is the fleet operator's, and its members are the fleet admins;
every other tenant of that directory is an **account**. Fleet admins
and fleet users log in there like at any directory (§6), through the
connectors the fleet admins enable, including an account's own IdP. The
registry is small and public to the fleet's members; it is served to
them as a signed feed, like a directory's.

**Accounts.** A fleet admin creates an account for an organization
(`gxctl fleet account create`), sets what it may register (directories,
user tenants, operator tenants, clusters) and invites its first
`account-admin`. With `accounts: open` an organization can create its
own account at sign-up, with the fleet's default rights. Account admins
add their colleagues or a connector to their own IdP.

**Registering (claim codes, D36).** Every registration has two halves:
the account asks, the thing itself confirms.
1. **A directory.** An account admin, logged in at the fleet, creates
   the entry (`gxctl fleet register directory`) and gets a claim code
   (single use, 7 days). The directory's owner redeems it at the
   directory (`gxctl fleet claim <code>`, `directory.tenants`, step-up):
   the directory sends its issuer and key fingerprints, signed with its
   signing key, over TLS pinned to the fleet's certificate (in the claim
   code). With `approval` it then waits for a fleet admin.
2. **A tenant.** Likewise, from the account (`gxctl fleet register
   tenant --directory <id> --kind user|operator`), redeemed by the
   tenant's admin at its directory (`tenant.clusters`). The fleet sees
   the tenant's id, slug, kind and contact, nothing else. A user tenant
   of an approved directory is active once claimed; an operator tenant
   waits for approval under `approval`, once.
3. **A cluster.** From the account of the cluster's operator (`gxctl
   fleet register cluster`), redeemed on the cluster with `gxctl fleet
   join --claim <code> [--admit open|none]` (`cluster.trust`, step-up):
   endpoint, CA pin, operator tenant, and the admission terms it
   publishes (whether it admits by policy, the default cap). `--admit
   open` enables the `fleet.open` template; the operator can write
   narrower policies instead. The cluster now follows the registry
   feed. A cluster whose operator tenant is approved is active once
   claimed.

The two halves are usually the same people with two logins (their
fleet account and their own directory); the claim code is what binds
them. An account can register only what it can also claim.

**Self-service use.** A tenant admin browses the fleet's catalog of
clusters (`gxctl fleet clusters`, the UI's Clusters page) and opts in to
one at its own directory (`gxctl tenant opt-in --fleet <url> <cluster>`). The
directory takes the cluster's pins from the registry and connects; the
cluster takes the directory's pins from the registry, evaluates
`admitTenant` with the tenant as principal and its labels (D35), and
creates the admission (`source: fleet:<id>`, the policy's cap and
labels) or leaves it `pending` for the operator. From there it is the
pairwise trust of §12.1: client registration, feed, tokens.

**Key continuity (D36).** The registry records a directory's first key
pin. Clusters and directories that already trust a directory keep their
own pin and follow only rotations signed with the previous key; a
registry entry that disagrees with a held pin is reported and ignored.
A fleet therefore can't redirect an existing tenant to a different
directory.

**Moving a cluster (D37).** `gxctl fleet leave` keeps every admission
and pin as pairwise trust; with `--drop-fleet-admissions` the
admissions with `source: fleet:<id>` move to `suspended` (no new
allocations; the tenants are notified on their feed and keep their
running resources until they or the operator remove them).
`gxctl fleet join` with another fleet then publishes the cluster there
and applies that fleet's registry from then on. Admissions made by
offers are never affected by fleet changes.

**Fleet outages.** A fleet is only consulted to register, to opt in,
and to look up pins for a new pair. Its outage stops those; every
established pair keeps working, including logins and feeds.


## 13. Directory secrets (D21, D27)

- Each directory has its own **data key**, random, wrapping its
  connector secrets with ChaCha20-Poly1305 (`ring`, a random 96-bit
  nonce per value; AAD = `directory_id ‖ connector_id ‖ field`). Data
  keys are wrapped by the host's KEK (32 random bytes: the systemd
  credential `secrets-kek`, else the file `auth.secrets_key`, default
  `<database dir>/secrets.kek`, made 0600 on first start; TPM2 sealing
  later), with the KEK's id stored next to each wrapped key. The KEK is on
  each server of the host, sent to joining servers with the CA key (and
  only with a CA key that matches the cluster's), never in Raft,
  snapshots, feeds or bundles. A server whose KEK is a credential can't
  take another one: every server gets the same credential.
- The KEK rotates on demand (`gxctl cluster rotate-secrets-key`): every
  server takes the new key and keeps the previous one, then the leader
  re-wraps the data keys in one write; a wrapped key names its KEK by id.
- Each directory has its own **signing keys** (ID tokens, access tokens),
  rotated on demand and every 90 days, the old one kept for verification
  until every token it signed has expired; public keys travel in feeds.
- Decryption and signing only on the host's leader, into zeroized
  buffers.
- *Honest limit:* a departed server of the host held the KEK and the
  ciphertexts; the leave flow lists the connectors and asks their
  owners to rotate IdP-side secrets, and rotates the signing keys.
- File-defined connectors (`source: file`) keep reading
  `oidc-client-secret` from `LoadCredential`.

## 14. API

Every operation is served over **both REST/JSON and gRPC** from one
implementation (§14.1, [grpc.md](grpc.md)). Directory operations answer
under the directory's issuer (`/d/<id>/…`, and at the root on a host
with one directory); cluster operations on the cluster.

| Operation (Cedar action) | Where | REST | gRPC | Permission |
|---|---|---|---|---|
| `discover` | both | `POST /auth/discover` | `LoginService.Discover` | none |
| `startLogin`, `finishLogin` | directory | `GET /auth/c/{id}/start`, `/callback` | — (redirects) | none |
| OIDC provider (`authorize`, `token`, JWKS, discovery) | directory | `/oidc/…` | — | none / client authentication |
| `startDevice`, `pollDevice` | directory | `POST /auth/c/{id}/device`, `…/poll` | `LoginService.StartDevice`, `PollDevice` | none |
| `passwordToken` | directory (PAM: the co-hosted cluster) | `POST /auth/token` (+ `connector`) | `LoginService.PasswordToken` | none (TLS or api.sock) |
| `accessToken` | directory | `POST /auth/access-token` (`{cluster}`) | `LoginService.AccessToken` | refresh token |
| `exchangeToken` | directory | `POST /auth/exchange` | `LoginService.ExchangeToken` | workload token |
| `createDirectory`, `listDirectories` | host | `POST/GET /directories` | `DirectoryService.Create`, `List` | the host cluster's `system-admin` |
| `listTenants` … `deleteTenant` | directory | `/tenants…` | `TenantService.…` | `directory.read` / `directory.tenants`; own tenant |
| `optIn` (an offer token, or a fleet cluster), `optOut`, `listClusters` | directory | `POST /tenants/{id}/clusters` (`{offer}` or `{fleet, cluster}`), `DELETE …/{cluster}`, `GET …` | `TenantService.OptIn`, `OptOut`, `ListClusters` | `tenant.clusters` |
| `createInvitation`, `listInvitations`, `revokeInvitation` | directory | `POST/GET /tenants/{id}/invitations`, `DELETE /invitations/{id}` | `InvitationService.Create`, `List`, `Revoke` | `tenant.members`; `directory.tenants` |
| `listTenantBindings`, `setTenantBinding`, `deleteTenantBinding` | directory | `/tenants/{id}/bindings` | `TenantService.…Binding` | `tenant.members` |
| projects, guest credentials | directory | `/projects…`, `/credentials…` | `ProjectService.…`, `CredentialService.…` | `tenant.projects`, project roles |
| users, teams, connectors, SCIM tokens | directory | `/users`, `/teams`, `/connectors…` | `DirectoryService.…`, `ConnectorService.…` | `directory.*`; own profile |
| SCIM 2.0 | directory | `/scim/v2/c/{id}/…` | — (RFC 7644) | SCIM token |
| federations | directory | `/tenants/{id}/federations` | `FederationService.…` | `tenant.federations` |
| `offerAdmission`, `listAdmissions`, `updateAdmission`, `removeAdmission` | cluster | `POST/GET /admissions`, `PATCH/DELETE /admissions/{tenant}` | `AdmissionService.…` | `cluster.admissions`; tenants see their own |
| `listTrustedDirectories`, `updateTrustedDirectory`, `removeTrustedDirectory` | cluster | `/trust/directories…` | `TrustService.…` | `cluster.trust` |
| resources (VMs, disks, networks, …) | cluster | unchanged paths | per grpc.md §9 | unchanged + `useCluster` (§10.4) |
| `createAccount`, `updateAccount`, `listAccounts` | fleet | `POST/PATCH/GET /fleet/v1/accounts` | `FleetService.…Account` | `fleet.registry` (own account: `tenant.read`) |
| `registerDirectory`, `registerTenant`, `registerCluster`, `deregister…` | fleet | `POST /fleet/v1/registrations` (`{kind, …}`), `DELETE …/{id}` | `FleetService.Register`, `Deregister` | `account.register` within the account's rights; returns a claim code |
| `claim` | directory or cluster | `POST /fleet/claim` (`{code}`) | `ClaimService.Claim` | `directory.tenants` (directory), `tenant.clusters` (tenant), `cluster.trust` (cluster); sends the signed half to the fleet |
| `listRegistry`, `approveRegistration`, `suspendRegistration`, `setFleetLabels` | fleet | `GET /fleet/v1/registry`, `POST …/{id}/approve`, `…/suspend`, `PUT …/{id}/labels` | `FleetService.…` | members (list); `fleet.registry` |
| `joinFleet` (a cluster claim), `leaveFleet`, `fleetStatus` | cluster | `POST /fleet` (`{claim}`), `DELETE /fleet`, `GET /fleet` | `ClusterFleetService.…` | `cluster.trust` (step-up); status: authenticated |
| `readAudit`, `readUsage` | both | `GET /audit`, `GET /usage` (+ `?tenant=`) | with their families | + `tenant.audit` / `tenant.usage` |

**HTTP-only, on purpose:** browser redirects, the OIDC provider
endpoints and SCIM. They still go through the service layer.

**Lists** take `page_size` (≤ 500, default 100) and `page_token`.

Errors: `409 managed_by_connector`, `409 tenant_in_use`, `403
tenant_suspended`, `403 not_admitted`, `403 untrusted_directory`, `403
tenant_quota_exceeded`, `422 invalid_connector`, `409 sync_blocked`,
`503 directory_unavailable`, `401 directory_stale`, `421
directory_is_elsewhere` (with the directory's URL).

### 14.1 Service layer (both transports)

1. **Operations are transport-neutral functions** in
   `src/service/{tenancy,directory,login,trust}.rs`, taking a `Call`
   (principal, Cedar action, request id, audit slot; grpc.md §6) and a
   proto request, returning a proto response or a `ServiceError`.
2. **All decisions live in the service function**: Cedar requests,
   not-found versus forbidden, caps, names, the link boundary,
   managed-field checks, secrets, the store write. Adapters only decode,
   call one function, and encode.
3. **Types come from proto** (`crates/glidex-api/proto/`).
4. **Errors are one enum with stable codes** (grpc.md §7).
5. **One operation, one Cedar action, one audit entry.**
6. **Secrets never appear in response messages** (grpc.md §4 check).
7. **Reads and writes are declared per operation** (grpc.md G-D6).
8. **Directory records are written only through `service::directory`**,
   scoped by `directory_id`; on a control plane that doesn't host the
   directory it answers `directory_is_elsewhere`. One module, so a host
   with one directory and separately hosted directories run the same code.

`control-plane.json`: `auth.pam` and `auth.oidc` keep working as
file-defined connectors of the host's first directory. New:
`trust.default_max_staleness`, `directory.access_token_ttl`,
`directory.sync_interval_min`, `tenancy.max_tenants`.

## 15. UI and CLI

**UI** ([web-ui.md](web-ui.md)):
- Login: "work email or organization", then the directory's connector
  buttons; a cluster's UI sends the browser to the right directory and
  back (§7.1). A user in several tenants picks one.
- Directory owner: **Tenants** (domains, self tags, operator flag, suspend, fleet registrations),
  **Directory** (connectors with the per-preset wizard, users, teams,
  site policies).
- Operator, per cluster: **Admissions** (offers, pending fleet opt-ins,
  caps, labels), **Fleet** (membership, published terms, admission
  policies, move), **Trusted directories** (lag, `max_staleness`), site
  policies, plus today's cluster pages.
- Fleet (at the fleet's directory): fleet admins get **Accounts**
  (rights, invitations) and **Registry** (directories, tenants,
  clusters, pending approvals, labels); fleet users get **My account**
  (members, connector) and **Registrations** (create, claim codes,
  status).
- Tenant admin: **Projects** (per-cluster quotas, where each project has
  resources), **Clusters** (the fleets' catalogs, opt in, accept offers,
  opt out, each admission),
  **Members** (with invitations: invite, copy the link once, revoke),
  **Federations**.
- Lists of VMs, disks and networks show their cluster; a tenant's view
  gathers them from every cluster it uses.

**CLI** ([cli.md](cli.md)):
```
gxctl auth login --directory <url> [--tenant <slug>] [--connector <id>]
gxctl tenant list|show|create|update|suspend|resume|delete|opt-in|opt-out
gxctl directory create|list|promote                     # on a host
gxctl connector list|show|add <kind>|update|secret set <field>|test|sync|remove
gxctl connector scim-token create|list|revoke
gxctl invite create <tenant> <email> --role R --connector C | list [<tenant>] | revoke <id>
gxctl federation list|add|remove
gxctl admission offer|list|update|remove                # on a cluster
gxctl trust list|update|remove                          # on a cluster
gxctl fleet create|registry|approve|suspend|label        # fleet admins
gxctl fleet account create|update|list|invite            # fleet admins; account admins: their own
gxctl fleet register directory|tenant|cluster            # account admins, at the fleet → claim code
gxctl fleet claim <code>                                 # at the directory (directory, tenant)
gxctl fleet join --claim <code> [--admit open|none] | leave | status   # on the cluster
gxctl fleet clusters [--fleet <url>]; gxctl tenant opt-in --offer <token> | --fleet <url> <cluster>
```

## 16. Migration

On the first start with the tenancy schema (one write), a host becomes
a cluster hosting one directory:

1. Create directory `local` (issuer = the host's URL), tenant `admin`
   (`owns_directory`, `operator`) owning it and operating the cluster,
   and the cluster's trust in `local` (no handshake: same control
   plane).
2. Create tenant `default` in `local`, opted in and admitted here with
   an unlimited cap; every project moves to it (project quotas become
   this cluster's entry), with its guest credentials.
3. Users who hold a system-role or grant link, or are `glidex-admin`
   peers, become members of `admin`; every user with links in a
   project becomes a member of `default` (some are both). Teams stay as
   they are, in `local`.
4. Links on the `Cluster` stay as this cluster's links. Holders of
   `role.system-admin` also get `directory-admin` on `local` and a
   delegated `tenant-admin` on `default` (D15), so they keep managing
   users and projects as before.
5. `auth.pam` becomes a file-defined PAM connector of `local` for this
   cluster (D23); `auth.oidc` a file-defined OIDC connector of `local`;
   `oidc:<issuer>` identities are re-keyed to `conn:<id>`.
6. API tokens become directory tokens valid on this cluster.
7. Name indexes are rebuilt per tenant (no conflicts are possible).

Because `admin` and `default` share a directory, existing users keep
one login for both (D3). Nothing a user does today changes.

## 17. Testing

- **Policy decisions** (table-driven, security.md §14 style), with
  directories `acme` and `uni` (tenants `uni-ops`, operator, and
  `cs-lab`), and clusters north (operator `uni-ops`) and south
  (operator `dc-co`):
  - acme's tenant-admin reads acme's VMs on north and south, nothing of
    `cs-lab`'s (`base.tenant-boundary`);
  - a `uni-ops` member with `system-admin` on north reaches acme's VM on
    north, not on south, and no acme project or credential record;
  - a `dc-co` member with `role.net-admin` on north (inserted into the
    store directly) is denied (`base.host-operator`);
  - `useCluster` on south for `cs-lab` is denied until south admits it
    **and** it opts in; a site `forbid` on `operator_labels` beats both;
  - `uni`'s owner, delegated `tenant-admin` on `cs-lab`, passes; on
    acme (another directory), the link itself is refused;
  - a user of `uni` who is a member of both `uni-ops` and `cs-lab`
    acts in each through one login;
  - directory-scoped actions are denied to everyone but the owner's
    members; a suspended tenant's members get `base.tenant-suspended`.
- **Route coverage:** every action is in exactly one marker group.
- **Link boundary:** each link kind refuses a principal of the wrong
  tenant or directory.
- **Names, caps, per-cluster project quotas.**
- **Connectors, per protocol** against local mocks (OIDC with
  `tid`/`hd`/`oid`, Entra group overage; GitHub org membership; LDAP
  StartTLS and `objectGUID`; MFA without `amr`).
- **Google (T2)** against a mock: a forged email domain without `hd` is
  refused; `hd` not in `domains` is refused; `email_verified: false` is
  refused; a consumer login without an invitation or allowlist entry is
  refused; the device grant uses the device client; `trust_2sv`
  decides MFA; D22 refuses consumer mode for the owner and operators.
- **Invitations:** taken once; not by another email, not without the
  secret, not after expiry or revocation; the secret is never stored or
  answered again; `jit: false` still lets the invitee in.
- **Real IdPs** (Google Workspace and consumer, JumpCloud) are checked
  by hand against an installed host, with test accounts kept outside
  the repository: their passwords are entered only in the IdP's own
  page, and OAuth client secrets only through `gxctl connector secret
  set` on standard input. [idp-checks.md](idp-checks.md) holds the
  checklist and its results, never accounts.
- **SCIM:** Entra and Okta sequences as fixtures; `active: false`
  revokes within the request; connector X's token can't touch Y.
- **Pull sync:** failing APIs change nothing; the safety stop holds;
  delta and full resync agree.
- **Secrets:** no read or feed contains secret bytes; a hosted
  directory can't decrypt another's (different data keys).
- **Federation:** wrong repository, `aud`, expiry, unpinned rules are
  refused; tokens work only on the rule's clusters.
- **Trust (T6),** two directories and two clusters in process:
  - an offer token redeemed by the wrong tenant, the wrong directory,
    twice, or after expiry is refused;
  - a cluster's feed holds only tenants admitted on both sides, never a
    connector secret;
  - an access token for north is refused by south and by every
    directory; a token from an untrusted directory is refused;
  - logout at the directory ends north's sessions as the feed delivers
    it;
  - cutting north off from `acme` leaves `uni` users unaffected; past
    `max_staleness`, `acme` credentials stop on north, PAM still works;
  - promote with a new epoch: north accepts the standby's keys only as
    announced.
- **Fleets (T7),** a fleet, two directories, two clusters:
  - fleet users log in at the fleet's directory; an account admin of
    account A can't see or change B's registrations
    (`base.tenant-boundary`); a fleet admin can;
  - an account without the right to register clusters gets no claim
    code for one;
  - a claim code redeemed at the wrong directory, by a principal
    without the right role there, twice, or after expiry is refused;
  - under `approval`, a claimed directory and a claimed operator tenant
    wait for a fleet admin; a user tenant of that directory and a
    cluster of that operator are then active as soon as claimed;
  - with `fleet.open`, a user tenant's opt-in creates an admission with
    the default cap and `source: fleet:<id>`; an operator tenant's
    doesn't; without a policy it stays `pending`;
  - a tenant's `self_tags` can't satisfy a policy on `fleet_labels` or
    `operator_labels`;
  - a registry entry with a new key for an existing directory is
    ignored by clusters that hold its pin; a rotation signed with the
    old key is followed;
  - with the fleet down, established pairs keep logging in and feeding;
  - leaving keeps admissions; `--drop-fleet-admissions` suspends only
    fleet-sourced ones; joining another fleet applies its registry.
- **Both transports:** every API test over `{rest, grpc}`.
- **Migration:** a pre-tenancy database migrates per §16 and every
  existing API test passes unchanged.

## 18. Milestones and work plan

Each milestone ships its API, CLI, UI and tests together and leaves
`main` releasable.

| # | Scope | Acceptance |
|---|---|---|
| **T0** tenants, operators, admission | needs grpc.md **G0**. The model for one host: §10 entities (`Directory`, `Tenant`, `Cluster.operator`, `Vm in [Project, Cluster]`, membership), table scopes with `directory_id` keys, `directories`, `tenants`, `admissions`, projects and guest credentials as directory records, `useCluster` and `base.admission`, the operator, owner and tenant boundaries, roles, link boundary, caps and per-cluster project quotas, names, `tenant_grants`, migration (§16 steps 1–4, 6–7), APIs on both transports through `service::directory`, gxctl `tenant`/`admission`, UI Tenants, Admissions and cluster columns | §17 policy, route-coverage, link, names, caps and migration tests on both transports; existing suites green. |
| **T1** connector framework | `connectors`, data keys and KEK, the `ExternalIdentity` path, discovery, per-connector routes, file-defined PAM/OIDC (§16 step 5), PAM co-hosting (D23), D22 rules, D20 clamp, `gxctl connector`, UI connectors (PAM, OIDC) | Existing OIDC and PAM tests pass through connectors; secrets and peer tests. |
| **T2** OIDC presets and invitations | §5.1: `google` (Workspace and gated consumer accounts, D39), `jumpcloud`, `oidc`; invitations (§6.1, D40); MFA per preset; per-preset connector tests, wizard and gxctl | Mock tests per preset and for invitations (§17); manual checks against real Google (both kinds of account) and JumpCloud. |
| **T2b** Entra and GitHub | `azure` (`tid`, `oid`, group overage) and `github` (OAuth 2.0 web and device flow, org membership); moved out of T2 | Mock tests; manual checks against real Entra and GitHub. T4's Graph and GitHub sync need it. Not scheduled. |
| **T3** SCIM | §8.1, managed users and teams, D20, field ownership | SCIM fixtures; deprovision-revokes test. |
| **T4** pull sync and attributes | `directory_sync`, LDAP, Google Directory, Graph, GitHub, safety stop; SSH keys from connectors | Pull sync tests; LDAP against OpenLDAP in CI. |
| **T5** workload federation | `federations`, `POST /auth/exchange`, `jwt` and `aws` kinds | Federation tests; a GitHub Actions workflow deploys without a stored token. |
| **T6** directories and clusters apart | hosted directories (D27), the directory as OIDC provider, access and refresh tokens, cluster sessions from ID tokens, the handshake, filtered feeds, admission reports, partitions and staleness, standby and promote, gxctl `auth login --directory`, `trust`, UI | §17 trust tests; a check with two organizations' directories and two clusters across a real WAN link. Can start once T1 has landed. |
| **T7** fleets | §12.2: the fleet role with its own directory and accounts (D38), registry and signed feed, claim codes and self-service registration with approval, the catalog and opt-in, `admitTenant` and the `fleet.open` template, labels by provenance, key continuity, leave and move, gxctl `fleet`, UI Registry and Fleet pages | §17 fleet tests; two operators' clusters in one fleet, one moved to a second fleet. Needs T6. |

**T0 tasks** (files):
1. `tenancy.rs`: `Directory`, `Tenant`, `Admission` and stores;
   `Project.tenant_id` and per-cluster quotas; caps in `overruns` and
   `state.rs::usage_locked`.
2. `store/db.rs`: new table ids, `TableId::scope()`, `directory_id` key
   prefixes for directory tables; guest credentials moved to directory
   scope; migration beside `adopt_into_default_project`.
3. `auth/store.rs`: `directory_id` on `User` and `Team`; membership;
   `Token.clusters` and service-account `tenant_id`; `MemberSource` =
   `Manual | Connector(String)` with serde aliases.
4. `policies/`: §10.1–§10.2; `authz.rs` adds `context.cluster` or
   `context.directory` and the new entities and attributes.
5. Protos `glidex/tenancy/v1/{tenants,projects,admissions}.proto`;
   `src/service/{tenancy,directory}.rs`; thin adapters in `api/` and
   `api/rpc/`; route-coverage marker check.
6. `api/vms.rs`, `api/storage.rs`, `api/net.rs`: `useCluster` in compound
   requests; `tenant_id` on resources; `tenant_grants`;
   `base.share-same-tenant`.
7. gxctl `tenant` and `admission`; UI Tenants, Admissions, cluster
   columns (bun).
8. `tests/tenancy_tests.rs`; spec updates in security.md §6–§7,
   rest-api.md, cli.md, web-ui.md, data-model.md.

**T1 tasks** (files):
1. `src/directory/` (`mod.rs` connector model and validation,
   `secrets.rs` data keys and KEK, `pam.rs`, `oidc.rs` moved from
   `auth/oidc.rs`).
2. `auth/mod.rs`: `ExternalIdentity` path; `Principal.directory_id` and
   acting tenant; peer and PAM resolution; token clamp.
3. `config.rs`: file-defined connectors.
4. `cluster/`: KEK transfer to joining servers, exclusion from
   snapshots and node caches, rotation with the CA.
5. `glidex-install`: the `secrets-kek` credential.
6. Protos `glidex/directory/v1/*`, `glidex/auth/v1/login.proto`;
   `src/service/{directory,login}.rs` and adapters; gxctl `connector`;
   UI connectors.

**T2 tasks** (one PR each, in order):
1. This spec (D39, D40, §5.1, §6.1).
2. Preset framework and `google`: `Kind::Google` over the OIDC code (`directory/presets/{mod,google}.rs`: fixed
   endpoints and scopes, settings validation, claim checks, MFA rule);
   the device client; D22 extended; mock-IdP tests.
3. Invitations: `directory::invitations` store, the callback path in
   `auth::external_login`, `InvitationService` on both transports
   (`glidex/directory/v1/invitations.proto`), `allowed_emails`; gxctl
   `invite`; UI under the tenant's members.
4. `jumpcloud` (`Kind::Jumpcloud`, `directory/presets/jumpcloud.rs`).
5. UI wizard per preset (fields, the redirect URI to register, Google's
   two clients and the consumer gate), `gxctl connector add <preset>`,
   connector test steps per preset, and `docs/` checklist for the real
   IdPs.

T3–T7 get their own task lists when they start; per-protocol code under
`src/directory/<protocol>.rs`, presets under `src/directory/presets/`.

## 19. Out of scope

- A user who is a member of tenants in two directories (D3): two users.
- Tenant-authored Cedar policies (D28).
- Tenant-private images; per-tenant metering prices.
- L2 networks or disks spanning clusters; moving a VM between clusters.
- Acting as an IdP for guest VMs (SSH certificates from directory
  identities), a follow-up of T4's SSH keys.
- SCIM outbound.
- SAML (D30).
- Tenant or directory self-service signup (D31), including public
  signup with Google accounts (§20).

## 20. Later designs

- **SPIFFE/SPIRE** for workload and agent identity: SPIFFE JWT-SVIDs fit
  federation's `jwt` kind (§9); SPIRE could issue node identities in
  place of the cluster CA's node certificates (clustering.md §12.1).
- **GPUs, DPUs and other server-attached accelerators**: modelling,
  scheduling and granting them to tenants; today's PCI grants (D25)
  stay.
- **Federation between fleets**: for now a tenant or directory that
  wants clusters of several fleets registers in each (D37).
- **Consolidated billing** per fleet account: usage reported by the
  account's clusters and tenants (metering.md) gathered at the fleet,
  priced and invoiced per account.
- **Public signup**: anyone with a Google (or other social) account
  signs in and gets a tenant of their own. It needs tenant creation per
  user, default caps and admission on clusters, abuse controls and
  account deletion, and would revisit D31 and D39.
- **A tenant's view across clusters**: resources, usage and audit from
  every cluster in one place, assembled by the UI and gxctl from each
  cluster's API at first, perhaps kept at the directory later.

## 21. Resolved questions

1. **SAML:** not supported (D30).
2. **Default tenant on upgrade:** existing projects go to `default`
   (D32, §16).
3. **Tenant creation:** by the directory's owner (D31).
4. **DPUs and accelerators:** later (§20).
5. **Organizations and identity:** each tenant is in one directory,
   connected to its own IdP; tenants share logins only by sharing a
   directory (D2, D3).
6. **Logical and physical resources:** user tenants own projects and
   what they allocate, across clusters; operator tenants own clusters,
   nodes and site policies (D4, D6).
7. **Different operators:** each cluster has its own operator, trust,
   admissions and site policies (D4, D8–D10).
8. **Directory hosting:** operators can offer it (D27).
9. **Labels:** by provenance; operators keep their own on admissions
   (D35).
10. **Brokering admission at scale:** fleets, with self-service
    registration and admission policies; clusters can move between
    fleets (D33–D37).
11. **Who manages a fleet:** fleet admins and fleet users log in at
    the fleet's own directory; each organization has an account that
    owns its registrations (D38).
12. **Approval defaults:** one approval per directory and per operator
    tenant; their user tenants and clusters need no second one (D36).
13. **Fleets of fleets:** later (§20); registering in several fleets
    covers it until then.

## 23. Implementation status

T0, T1 and T2 are implemented; T2b and T3–T7 are not started. What
follows is per milestone.

T0 is implemented on one host: a cluster that hosts directory `local`,
operated and owned by tenant `admin`, with tenant `default` holding the
projects from before tenants.

| Spec | Code |
|---|---|
| Directory, tenants, admissions, explicit members; tables and scopes (§4) | `src/tenants.rs`; `store/db.rs` (`Directories`…`TenantMembers`, `TableId::scope()`); `Project.tenant_id` in `src/tenancy.rs` |
| Schema, markers on every action, base policies, tenant and directory roles (§10) | `policies/`; `authz.rs` (`Tenant`, `Directory` entities, `every_action_has_exactly_one_marker`, `tenancy_boundaries`) |
| Tenancy context and memberships for every decision | `auth/mod.rs::complete_tenancy` (project tenants, tenant attributes, operator and admissions, memberships from members and links, `context.cluster`/`directory`/`tenant`) |
| Link boundary for host roles (D4) | `auth/mod.rs::is_member`; `api/access.rs::add_system_binding` (`422 not_operator_member`) |
| Admission on allocation (`useCluster`, D9) and tenant caps (D19) | `api/mod.rs::Caller::require_admitted` in VM, disk, network and router creates and VM starts; `state.rs::apply_quotas` (`403 tenant_quota_exceeded`) |
| Migration (§16 steps 1–4, 6–7) | `TenantStore::new` (bootstrap), `AuthService::migrate_tenancy`, `make_administrator`; bootstrap of fresh installs |
| Tenant and admission API on both transports (§14, §14.1) | protos `glidex/tenancy/v1/tenancy.proto`; `src/service/tenancy.rs`; REST `api/tenants.rs`; RPC `api/rpc/tenancy.rs` |
| gxctl `tenant`, `admission` on the generated connectrpc clients | `bin/gxctl/tenancy.rs`, `client::RpcTransport` |
| UI | capability checks on `Directory`/`Tenant` (`session.tsx`) |
| Tests | `tests/tenancy_tests.rs`; `cluster_tests.rs::rpc_writes_on_a_follower_reach_the_leader_with_their_trailers`; gxctl `rpc_clients_over_the_unix_socket`; unit tests in `authz.rs`, `tenants.rs`, `auth/mod.rs` |

Decided while building:
- **`Project in [Cluster, Tenant]`**, resources `in [Project]`: the
  cluster is the one deciding the request, so a cluster role covers the
  projects using that cluster as this cluster sees them. Each cluster only
  holds its own resources, so this reaches nothing elsewhere (§10.1 keeps
  the multi-cluster wording).
- **The resource's tenant is in the context** (`context.tenant`), which
  keeps the boundary one rule for every resource type.
- **Directory admins read and bootstrap tenants** of their directory
  (members, bindings), but act inside one only through a delegated
  tenant role (D15).
- **Step-up** applies to deleting tenants and to admissions; a personal
  token acting as its owner never counts as a recent login
  (security.md §7.5).

Closed out after T1 (the T0 parts):
- **UI**: Tenants (list, detail with members and tenant roles),
  Admissions, and a Tenant column on Projects (`pages/Tenants.tsx`,
  `TenantDetail.tsx`, `Admissions.tsx`).
- **`tenant_grants` (D25)**: `Network.tenant_grants` (`tenants` in
  `POST /networks` and `PUT /networks/{id}/grants`, gxctl `network
  grant --tenant`), `pci.allow[].tenants`; `base.network-grant` and
  `base.pci-grant` take them; `base.share-same-tenant` keeps project
  network shares inside a tenant, and an offer across tenants gets the
  answer a missing target does.
- **Every project's tenant is an entity** in each decision, whether or
  not the caller is a member of it: before, an operator acting through a
  cluster role on a tenant's project was refused `useCluster`.
- Names by id or slug where a tenant is named (`default_tenant`, grants).

- **Names unique per tenant (D24)**: **disks** since the storage
  family's port (G2, images.md §8); **networks** since the network
  family's (G3): a network has an `id` (UUID), the key of its record, of
  Cedar's `Network` entity, of VM NICs and of IPAM. A project network's
  name is unique in its tenant, a host network's on the cluster; where a
  request names a network, a name resolves among host networks and the
  project networks of the projects the caller sees (or, for a VM's NICs,
  in its project's tenant), and one several have is `409 ambiguous`.
  Schema 4 (`store::migrate_network_ids`) gives existing networks an id
  derived from the name (`network::legacy_id`, UUID v5, so each server of
  a cluster derives the same one) and rewrites the NICs and IPAM rows; a
  migrated network keeps its OVN objects (`ovn_key`: `gx-<name>`), new
  ones are `gx-<id>`, and new node networks' bridges are `gxbr-<first 8
  of the id>`. An import gives a network a new id if one here has it.
  Metering rows from before keep the name as the network's subject id.

Left for later, and why:
- **Per-cluster project quotas** and **guest credentials apart from the
  cluster** (their table is already directory-scoped): both only differ
  from today once a directory serves several clusters (T6).

**T1** (connector framework) is implemented on the same one-host shape.

| Spec | Code |
|---|---|
| Connector model, kinds, validation, D22 (§5, §13) | `src/directory/mod.rs` (`Connector`, `Kind`, `validate`, `ConnectorStore`); tables `Connectors`, `ConnectorSecrets`, `DirectoryKeys` (directory scope) |
| Data keys and KEK (§13, D21) | `src/directory/secrets.rs` (`Kek`, `seal`/`open`, `rewrap`); `auth.secrets_key` in `config.rs`; `#LoadCredential=secrets-kek` in the unit |
| KEK transfer to joining servers | `cluster/runtime.rs` (`SecretsKey`), `cluster/server.rs` (`join_ready` sends, `put_ca` installs after the CA key checks out), `state.rs::lend_secrets_key` (before `join/ready`) |
| File-defined connectors (§16 step 5) | `directory::file_connectors`: `file-pam` from `auth.pam`, `file-oidc` from `auth.oidc` (secret from `oidc-client-secret`), read-only |
| The `ExternalIdentity` path: rules, JIT, teams, `default_tenant`, MFA (`amr`/`acr`), D20 clamp | `auth/mod.rs::external_login`, `login_pam_with`, `oidc_for` (one client per connector, rebuilt when it changes); `directory/oidc.rs` (`ClientSecret::Stored`, `mfa`) |
| Discovery and per-connector routes (§6) | `api/access.rs` (`discover`, `connector_start`/`callback`/`device_start`/`device_poll`, `connector` on `/auth/login` and `/auth/token`) |
| `ConnectorService` on both transports (§14) | proto `glidex/directory/v1/connectors.proto`; `service/directory.rs`; REST `api/connectors.rs`; RPC `api/rpc/directory.rs`; actions under `directory.connectors` (`role.directory-admin`, step-up on writes) |
| gxctl and UI | `bin/gxctl/directory.rs` (`connector …`), `login --oidc --connector`, `auth login --connector`; a sign-in button per OIDC connector (`pages/Login.tsx`) |
| Tests | `tests/connector_tests.rs` (with LoginService, MFA, reauth checks, peer first sight, standalone rotation); `cluster_tests.rs::a_new_server_receives_the_secrets_key`, `the_secrets_key_rotates_on_every_server`; `network_tests.rs::host_networks_are_granted_to_tenants_and_shares_stay_in_one`; gxctl `connector_client_over_the_unix_socket`; unit tests in `directory/` and `authz.rs` (`tenant_grants_and_shares_within_a_tenant`) |

Decided while building:
- **File-defined connectors keep their identity keys** (`pam`,
  `oidc:<issuer>`) and member sources, so upgrading needs no re-keying;
  API connectors key identities `conn:<id>`.
- **ChaCha20-Poly1305 from `ring`** rather than XChaCha: `ring` is
  already a dependency, and random 96-bit nonces are safe for the few
  values a directory seals.
- **The KEK defaults to a file next to the database** so a plain
  install keeps its secrets across restarts; without a path (embedding,
  tests) it is a one-off key, with a warning.
- **API-connector OIDC callbacks** are `/api/auth/c/<id>/callback`; the
  file connector keeps `/api/auth/oidc/callback`.

Closed out after T1:
- **`LoginService`** (`glidex/auth/v1/login.proto`: `Discover`,
  `StartDevice`, `PollDevice`, `PasswordToken`) on `service/login.rs`,
  which the REST twins (`/auth/discover`, `/auth/c/{id}/device…`,
  `/auth/token`) call too; RPC adapter `api/rpc/login.rs`.
- **MFA in decisions**: sessions record whether the login was
  multi-factor (`Session.mfa`, from the connector's `amr`/`acr` check);
  Cedar's `context.auth.mfa` (tokens, peer and PAM logins: `false`), so a
  site policy can say `forbid … unless { context.auth.mfa }`; `whoami`
  shows `mfa` and `directory_id`.
- **`Principal::directory_id()`** (constant `local` until T6).
- **Peers through the PAM connector (D23)**: a local user seen first on
  `api.sock` joins the host PAM connector's `default_tenant`; local
  identities already carry the node (`<name>@<node>`) in a cluster, and
  root and `glidex-admin` resolve to the operator tenant.
- **KEK rotation** (`gxctl cluster rotate-secrets-key`, `POST
  /cluster/rotate-secrets-key`, `rotateSecretsKey` in
  `host.cluster.critical`): the leader sends every other server the new
  key with the current one as its previous (`PUT
  /cluster/v1/secrets-key`, outside Raft), takes it, then re-wraps every
  data key in one write. Each server keeps the previous key
  (`secrets.kek.prev`) and opens a data key with whichever its `kek_id`
  names, so a rotation cut short loses nothing and can be run again. A
  KEK from the `secrets-kek` credential is rotated by changing the
  credential instead (`409`).
- **Connector management pages** (`pages/Connectors.tsx`).
- **Step-up and capability checks**: `/authz/check` answers `reauth:
  true`, and `/authz/allowed` a `reauth` list, for what only a fresh
  browser login would allow; the UI shows those actions and its re-login
  dialog runs when they're used.
- `auth/oidc.rs` moved to `directory/oidc.rs`.

Left for later: the acting tenant (the UI and gxctl asking which tenant
when a user is in several; projects say their own tenant today), TPM2
sealing of the KEK, and rotating the KEK with the CA.

**T2** (presets and invitations) is implemented: tasks 1–5 of §18, then
the follow-ups from testing against real Google Workspace and JumpCloud
tenants. The real-IdP checklist ([idp-checks.md](idp-checks.md)) is the
operator's to run. Task 2:

| Spec | Code |
|---|---|
| Preset framework (§5.1) | `src/directory/presets/mod.rs` (`Preset`: issuer spellings, authorization parameters, `ClaimRule`); `Connector::oidc_profile` (the browser's client, or with `device` gxctl's), `OidcState::with_preset`; `Kind::is_oidc` |
| `google` (D39): Workspace by `hd`, consumer accounts gated, `email_verified`, `trust_2sv` as MFA, public mail domains refused, D22 | `src/directory/presets/google.rs` (`GoogleSettings`, `Rule`); proto `GoogleSettings`, `GoogleAccounts`; `allowed_emails` in `AuthService::external_login` |
| Google's device client | `AuthService::oidc_device_for`; `service/login.rs::device_client` (`400 device_flow_unsupported`); the connector test's device-grant step |
| gxctl and UI | `gxctl connector add google`; the UI lists `google` connectors and offers them at login (its form is task 5) |
| Tests | `presets/google.rs` unit tests; `connector_tests.rs::google_*` against the mock IdP |

Task 3:

| Spec | Code |
|---|---|
| Invitations (D40, §6.1): the store, a secret's hash, single use, lifetime, pruning | `src/directory/invitations.rs` (`InvitationStore`, `InviteToken`); table `invitations` (directory scope) |
| Taking one at login | `AuthService::external_login_invited`: the user (with `jit` off too), tenant membership and the role link; `OidcState::start`/`callback` carry it in the login state |
| API on both transports | proto `glidex/directory/v1/invitations.proto` (`InvitationService`), `LoginService.InspectInvitation`; `service/invitations.rs`; RPC `api/rpc/directory.rs`, `api/rpc/login.rs`; REST `api/invitations.rs`, `POST /auth/c/{id}/start` in `api/access.rs`; Cedar `readTenantInvitations`, `manageTenantInvitations` |
| gxctl and UI | `gxctl invite list|create|revoke`; the tenant page's Invitations card (the link shown once); the signed-out page `/invite/<id>` (`pages/Invite.tsx`) |
| Tests | `invitations.rs` unit tests; `connector_tests.rs::invitations_let_one_known_person_in_once`; UI e2e `invitations.spec.ts` |

Tasks 4 and 5, and the follow-ups:

| Spec | Code |
|---|---|
| `jumpcloud` (§5.1): JumpCloud's issuer and scopes, groups in its claim (`memberOf`), an unstated email counted as verified | `src/directory/presets/jumpcloud.rs`; proto `JumpcloudSettings`; preset checks answer MFA and email verification (`presets::Checked`) |
| The connector form per preset | `pages/Connectors.tsx`: Google (Workspace or any account, the gate, `trust_2sv`, the device client and its secret), JumpCloud (groups claim), the redirect URI to register; `isOidcKind` for the login page and invitations |
| gxctl | `connector add jumpcloud` |
| Who signs in how (D17: one person through two connectors is two users) | `components/ui.tsx::SignInMethods`, `signInLabel` (a local account, or a connector by name): the tenant page's members and roles, a project's members, and the Access page's users; a membership says it came through a connector or an invitation |
| Several sign-in methods per user (§6.2) | `AuthService::link_external`; `POST /auth/link/c/{id}/start` and the callback's `After::Link` (`directory/oidc.rs`, `CallbackError` keeps the login's purpose); Cedar `manageOwnSignIns` (`base.own-sign-ins`, step-up); `UnlinkIdentity` (own or `manageUsers`, never the last, `end_sessions`); WhoAmI's user lists its identities; UI **Your account** (`pages/Account.tsx`, from the header's name) and the Access page's per-identity remove |
| Delete a user (§6.3) | `IdentityStore::delete_user` (one write), `UserService.DeleteUser`; never oneself or the last `system-admin`/`directory-admin`; the Access page's Delete |
| The first tenant-admin at tenant creation (§11) | `CreateTenantRequest.first_admin`, `invitation_link`; the Tenants page's create form |
| One Directory page (§15) | `pages/Directory.tsx`: Overview, Tenants, Users & roles, Connectors, Policies as tabs; `/tenants`, `/access`, `/connectors`, `/policies` lead to them; a local account's label names its host in a cluster (`<name>@<node>`, clustering.md D13) |
| Per-host system roles (clustering.md §12.2) | `AddSystemBinding.node` links auditor, net-admin or host paths to one node's `Host`; `ListSystemBindings.node`; `gxctl system-binding … --node`; the Scope select on System roles and the node page's Roles on this host |
| Docs | the operator guide's Google, JumpCloud and invitations sections; [idp-checks.md](idp-checks.md) for the real providers |
| Tests | `presets/jumpcloud.rs` unit tests; `connector_tests.rs::jumpcloud_*`; UI e2e `connectors.spec.ts` |


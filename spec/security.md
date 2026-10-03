# Security: authentication, tenancy and authorization

Status: **implemented** on branch `security-authz` (see §17 for what
is where and what still needs a real host). §1 records the starting
point at `617594e`.

This document defines who may do what to a glidex host: how callers
prove who they are (§5), how users, teams and projects are modeled (§6),
how a request is authorized (§7), what `glidex-netd` trusts (§8), and
how the processes are confined (§9). It supersedes the "unauthenticated
REST API" non-goal in [README.md](README.md) and the authorization
bullets of [networking.md §14](networking.md#14-security).

## 1. Starting point (617594e)

| Area | Today | Problem |
|---|---|---|
| REST API | No authentication (`main.rs:193`). Loopback by default; `GLIDEX_LISTEN` exposes it as-is. | Any local user, or any web page through DNS rebinding, has every right the control plane has, including netd's. |
| Console WebSocket | No `Origin` check (`api.rs::console_ws`). | Browsers apply no CORS to WebSockets: any site can open a VM console on a host the browser can reach. |
| `glidex-ui` | Proxies `/api` and replaces `Host` with the upstream authority (`glidex-ui/src/main.rs::proxy_api`). Runs as `User=glidex`. | A `Host` check in the control plane cannot see the browser's `Host`. The UI shares the control plane's uid, so a UI compromise can open netd's full socket. |
| `glidex` group | Service group **and** netd authority **and** console access; the installer adds the invoking human (`glidex-install/src/main.rs:1083`). | Every member is a network administrator of the host and can bypass any API check. |
| VM runtime files | `/tmp/<prefix>-<id>.{sock,console.sock,log,cloudinit.img}` (`models.rs:157-176`), `UMask=0007`, no `PrivateTmp`. | Group members can drive any VM's hypervisor API (CH HTTP / QMP) directly; predictable names in a shared `/tmp`. |
| netd ownership | `detach_vm_port` / `release_vm` ignore the stored `owner_uid` (`glidex-netd/src/server.rs:561-580`). | Any full-socket peer can detach any VM's ports. |
| Host paths | `rootfs_path`, `kernel_image_path`, `firmware_path`, `cloud_init_path` and `vfio_devices` are taken as given. | Any caller can make the control plane open any file it can read, or pass through any PCI device. |
| NAT isolation | The NAT forward chain accepts anything entering from a NAT bridge (`glidex-ovs/src/nat.rs:265-268`), and nothing filters guest traffic to the host. | A VM on one NAT network can open connections to VMs on another, and to any host service listening on the gateway (e.g. the control plane with `GLIDEX_LISTEN=0.0.0.0`). Matters once projects own networks. |
| Guest credentials | Hashes are never returned (`credentials.rs:72-110`). | — (kept) |
| Downloads | Firmware, Cloud-Hypervisor and images are sha256-verified. Rust and Bun are installed with `curl … \| sh` (`glidex-install/src/main.rs:723,748`). | Unpinned scripts run on every install/update. |

## 2. Decisions

| # | Topic | Decision |
|---|---|---|
| 1 | Human login | **Local PAM and OIDC** (external IdP), both enabled by configuration; either may be turned off. |
| 2 | Host network administration | **The control plane performs net-admin ops for users** that hold `host.network` (§7). netd grants the control plane all ops; the control plane is the policy point. |
| 3 | Tenancy | **Projects**, with **teams** as groups of users. Every VM, disk and guest credential belongs to exactly one project. |
| 4 | Single authority | The **control plane** authenticates and authorizes every request. `glidex-ui` is a proxy with no authority of its own. |
| 5 | Identity from the kernel where possible | Local callers (gxctl, the UI, the control plane to netd) are identified by `SO_PEERCRED` on Unix sockets, never by claims in a request. |
| 6 | PAM runs as root, not in the control plane | A non-root process cannot verify other users' passwords with `pam_unix` (`unix_chkpwd` only checks the caller's own). A small root helper, **`glidex-authd`** (§5.3), does it. It is kept out of netd so netd stays network-only. |
| 7 | Deny by default | Every route maps to one Cedar action; a route without one fails a test (§14). |
| 8 | Flat REST paths | Resources stay at `/vms/{id}`, `/disks/{id}`, …; the project is a field and a filter, not a path segment. IDs are global UUIDs, so this does not weaken isolation and keeps existing clients working. |
| 9 | Policy engine | **Cedar**, evaluated in process with the `cedar-policy` crate (§7). Roles are template-linked policies; invariants are `forbid` policies; every policy is validated against a schema. |
| 10 | Site policies | Admins manage additional Cedar policies **through the API** (and the UI later), stored in ReDB with version history (§7.6). They can add custom roles and restrictions but can't relax the shipped `forbid` policies. |
| 11 | Roles | A small shipped set that grows only for real needs: project `viewer`, `operator`, `editor`, `owner`; host `auditor`, `image-admin`, `net-admin`, `system-admin` (§7.3). |
| 12 | Quotas | `system-admin` may exceed project quotas (`quota.exceed`); every overrun is audited (§6.3). |
| 13 | PAM provisioning | PAM users are **created on first login** when they pass `pam.allowed_groups` (§5.3). |
| 14 | Project networks | Project `owner`s create and delete **NAT networks private to their project**, without `net-admin` (§6.2). |
| 15 | Step-up window | **10 minutes** for host-wide network changes and policy writes (`base.step-up`). |
| 16 | Sharing project networks | A project network is private unless **both** projects agree explicitly: the network's owner offers it, the receiving project's owner accepts. Either side can end it (§6.2.1). |
| 17 | Policy change control | Site policy changes need step-up and are audited; **no two-person approval**. |

## 3. Threat model

**Assets.** Guest data (disks, consoles, cloud-init seeds, guest
credentials), host network configuration (bridges, uplinks with IP
migration, NAT, DPDK), host devices (VFIO), files readable by the
service user, and the control plane's own state (`glidex.db`).

**Actors.**

| Actor | Trusted for |
|---|---|
| root, `glidex-admin` members | Everything on the host (break-glass). |
| System administrators (role `system-admin`) | All projects and identity configuration, through the API. |
| Network administrators (role `net-admin`) | Host networking, through the API. |
| Project members | Their projects' resources, within their role and quota. |
| Other local users | Nothing beyond `netd-ro.sock` (`probe`). |
| Remote / browser-origin attackers | Nothing. |
| A guest | Nothing. Hypervisor escape is out of scope (§12). |

**Trust boundaries.** Browser ↔ `glidex-ui`; network ↔ control plane
(TLS); local user ↔ control plane (`api.sock`); control plane ↔
`glidex-authd`, ↔ `glidex-netd` (peer uid); control plane ↔ IdP (TLS,
signed ID tokens).

## 4. Identities and processes

```
Browser ──HTTPS── glidex-ui (glidex-ui) ──Unix── ┐
gxctl (local) ──Unix: /run/glidex-cp/api.sock ───┤
gxctl / automation (remote) ──HTTPS + token ─────┴─► glidex-control-plane (glidex)
                                                        │ authn → authz → audit
                                     ┌──────────────────┼──────────────────┐
                                     ▼                  ▼                  ▼
                           glidex-authd (root)   glidex-netd (root)   OIDC IdP (HTTPS)
                           PAM, glidex peer only  per-op policy
```

| Unix identity | Members | Grants |
|---|---|---|
| user `glidex`, group `glidex` | **Only** the control plane | netd full socket, `glidex-authd` socket, `/dev/kvm`, VFIO. |
| user `glidex-ui` (new, own group) | The UI service | `ui.sock` (as a proxy, §5.6). Nothing else. |
| group `glidex-users` (new) | Humans allowed to use gxctl locally | `api.sock`. Rights still come from role links (§7.3). |
| group `glidex-admin` (new) | Break-glass administrators | netd full socket directly, and `system-admin` on `api.sock`. |

The installer stops adding humans to `glidex`; it adds the invoking user
to `glidex-users` and `glidex-admin` (bootstrap, §11).

## 5. Authentication

Every request ends up with a **principal**: `{user_id, auth_method,
authenticated_at, session_id | token_id, source}`. Requests without one
get `401` (except `GET /health`, `GET /auth/methods`, and the login and
OIDC endpoints).

### 5.1 Transports

| Transport | Address | Accepted credentials |
|---|---|---|
| Local Unix socket | `/run/glidex-cp/api.sock` | Peer uid (§5.2). |
| UI Unix socket | `/run/glidex-cp/ui.sock` | Session or token only; the peer must be `glidex-ui` (§5.6). |

Both socket files are mode `0666`: the access control is the per-request
peer-uid check, not the file mode. (Giving them group `glidex-users`
would need the `glidex` user to be a member of that group.)
| TCP, loopback | `127.0.0.1:8841`, `[::1]:8841` | Token or session. Loopback is **not** trusted. |
| TCP, other | `GLIDEX_LISTEN` | Token or session, **TLS required**: the control plane refuses to start on a non-loopback address without `tls.cert` and `tls.key`. |

The same rule applies to `GLIDEX_UI_LISTEN`: `glidex-ui` refuses a
non-loopback address without TLS, and sets cookies `Secure` whenever
it serves TLS.

### 5.2 Local peer identity (gxctl)

On `api.sock` the control plane reads `SO_PEERCRED` (reusing
`glidex_netd::auth::peer`):

- The peer's Unix groups become members of Cedar teams `unix:<group>`
  **for this request only** (§7.3). uid 0 is treated as a member of
  `unix:glidex-admin`.
- uid 0, or a member of the configured `admin_group` (default
  `glidex-admin`) → user `unix:<name>`, allowed everything by
  `base.break-glass`. Only that check adds the break-glass team: a host
  group that merely has the same name grants nothing.
- a member of `glidex-users` → user `unix:<name>`, created on first use
  (just-in-time); rights from its role links only.
- anyone else, including `glidex-ui` (which uses `ui.sock`) → connection closed.

`gxctl` talks to `api.sock` by default; `--url https://…` plus a token
selects TCP.

### 5.3 Local PAM (`glidex-authd`)

- Root daemon, socket-activated: `/run/glidex-authd/auth.sock`,
  `root:glidex 0660`. On accept it checks the peer uid **equals** the
  `glidex` user (not group membership) and closes otherwise.
- Protocol: newline-delimited JSON like netd. One op:
  `{"op":"authenticate","args":{"user":"…","password":"…","service":"glidex"}}`
  → `{"ok":{"uid":…,"groups":["…"]}}` or `{"error":{"code":"denied"}}`.
  The error never says whether the user exists.
- Runs `pam_authenticate` **and** `pam_acct_mgmt` (expired or locked
  accounts fail) with PAM service `/etc/pam.d/glidex` (installed as
  `@include common-auth` / `common-account`, or the distro equivalent).
- Allowed only for accounts in `glidex-users` (configurable,
  `pam.allowed_groups`).
- **Provisioned on first login** (`pam.jit`, default `true`). The first
  successful login creates the user with identity `pam:<name>`, linked
  to `unix:<name>` if that exists. A new user has no role links, so they
  can only see `whoami` and manage their own tokens until someone adds
  them to a project or team.
- Team mapping: `pam.group_teams` maps Unix groups returned by
  `glidex-authd` to teams, re-evaluated at every login, the same way as
  OIDC (§5.4). Mapped memberships are marked `source: pam`.
- Rate limits: 5 failures per user per 15 minutes, 30 per minute
  globally; a fixed 1 s delay on every failure. The password is never
  logged and is zeroized after use.
- Implementation notes (`crates/glidex-authd`): the group check runs in
  authd itself, from `allowed_groups` in `/etc/glidex/authd.json`
  (default `["glidex-users"]`); the control plane's `pam.allowed_groups`
  must match it. Denials that never reach PAM (unknown user, wrong group)
  also wait as long as the last PAM failure took, so timing doesn't tell
  them apart. Errors carry a fixed `message` next to `code`. The socket
  directory is `0755` (systemd creates it root-owned for the socket
  unit, and the `glidex` user must traverse it); the socket's `0660`
  mode and the peer-uid check are the access control. libpam is loaded
  at run time (`libpam.so.0`), so building needs no PAM headers.
- PAM login over the control plane is accepted only on TLS or loopback.
- A second op, `public_keys` (`{"user": …}`, no password), returns the
  user's own `~/.ssh/*.pub` lines for users who may log in (others get
  the same `denied`). authd reads them in a thread whose filesystem
  identity is switched to the user, opening only regular files the user
  owns, without following a final symlink, up to 16 KiB each, and keeps
  only lines that look like public keys. The control plane exposes it
  as `GET /users/me/ssh-keys` for the caller's own local (PAM/Unix)
  account only, to prefill new guest credentials.

### 5.4 OIDC

- Authorization Code flow with **PKCE** and a `nonce`; the control plane
  is a confidential client. Client secret from systemd
  `LoadCredential=oidc-client-secret`, never from the environment or a
  world-readable file.
- Discovery from `oidc.issuer`; ID token checks: signature (JWKS,
  cached, refreshed on unknown `kid`), `iss`, `aud`, `exp`, `iat`
  skew ≤ 60 s, `nonce`. Algorithms limited to `RS256`/`ES256`.
- Redirect URI on the UI origin: `https://<ui>/api/auth/oidc/callback`.
  `state` binds the browser session; it is single-use and valid 10
  minutes.
- Identity key: `(issuer, sub)`. `email` / `name` are display only and
  never used for authorization.
- Provisioning: `oidc.jit` (default `true`) creates the user on first
  login if `oidc.allowed_domains` / `oidc.required_groups` match.
- Team mapping: `oidc.groups_claim` (default `groups`) maps IdP groups to
  teams (`oidc.group_teams`), re-evaluated at every login. Mapped
  memberships are marked `source: oidc` and are removed when the claim
  no longer has the group; manual memberships are left alone.
- Remote gxctl: OAuth 2.0 Device Authorization Grant when the IdP
  supports it (`gxctl login --oidc`), else an access token (§5.5).

### 5.5 Sessions and tokens

**Sessions** (browser):
- 256-bit random id; only `SHA-256(id)` is stored (`sessions` table).
- Cookie `gx_session`: `HttpOnly; Secure` (when TLS); `SameSite=Strict;
  Path=/`.
- Idle timeout 30 minutes, absolute 12 hours. The id rotates at login;
  logout and user disable delete it.
- `authenticated_at` is kept for step-up (`base.step-up`, §7.3).

**Access tokens** (CLI, automation):
- Format `gxt_<base62 of 32 random bytes>`. Only the SHA-256 is stored
  (`api_tokens`). Shown once at creation, never logged.
- Two kinds:
  - *personal*: acts as its user, narrowed by its own role links;
  - *service account*: belongs to a project, has its own role links
    in that project, and survives its creator leaving.
- A personal token is allowed only what both it and its owner are
  allowed, checked on every request (§7.5). Revoking the owner's roles
  narrows the token at once.
- Required expiry: default 90 days, maximum 365. `last_used_at` and
  `last_used_from` are recorded.
- Sent as `Authorization: Bearer gxt_…`.

### 5.6 `glidex-ui` and browser protections

- The UI connects to the control plane over `ui.sock`. The control plane
  treats requests from the `glidex-ui` peer as *proxied*: they must
  carry a session cookie or a token, and only from this peer does it
  read `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Forwarded-Host`.
  The UI overwrites, never appends to, these headers.
- **Host check (UI):** `GLIDEX_UI_HOSTS` allowlist (default
  `localhost`, `127.0.0.1`, `[::1]` with the listen port). Other
  `Host` values get `421`. This blocks DNS rebinding.
- **Origin check (control plane):** state-changing methods and every
  WebSocket upgrade must carry an `Origin` in `auth.allowed_origins`
  (default: the UI's origins). Bearer-token requests without `Origin`
  (non-browser clients) are allowed.
- **CSRF:** requests authenticated by cookie and not `GET`/`HEAD` must
  send `X-Glidex-CSRF` equal to the session's CSRF value (returned by
  `GET /auth/whoami`). `SameSite=Strict` and the Origin check are the
  other two layers.
- **Console tickets:** `POST /vms/{id}/console/ticket` (needs
  `vm.console`) → `{ticket, expires_in: 30}`. The WebSocket URL carries
  `?ticket=`; tickets are single use, bound to the principal and VM, and
  the session is re-checked at upgrade.
- Response headers from the UI: `Content-Security-Policy: default-src
  'self'; connect-src 'self'; frame-ancestors 'none'`,
  `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and
  `Strict-Transport-Security` under TLS.

## 6. Tenancy model

### 6.1 Entities

| Entity | Key | Notes |
|---|---|---|
| User | `user_id` (UUID) | `display_name`, `disabled`, `created_at`. |
| Identity | `(provider, subject)` | `unix:<name>`, `pam:<name>`, or `oidc:<issuer>` + `sub`. Several identities may link to one user; linking is done by a `system-admin`, never automatically by email. A local `unix:` and `pam:` identity for the same login name link automatically. |
| Team | `team_id` | Members are users, with `source: manual \| pam \| oidc`. |
| Project | `project_id`, unique `name` (`[a-z0-9-]{1,32}`) | Owns VMs, disks, guest credentials, service-account tokens; has quotas (§6.3). |
| Role link | `(template, principal, resource)` | A Cedar template-linked policy (§7.3). Principal: user, team or token. Resource: a project, or `Host::"local"` for system roles. |

### 6.2 Resource scoping

| Resource | Scope | Rule |
|---|---|---|
| VM, disk | project | `project_id` set at create (request field, or the caller's default project); immutable. A VM may only attach disks and guest credentials of its own project. |
| Guest credential | project | Key becomes `(project_id, username)`. |
| Image | system | Shared, read-only library. Any authenticated user may list and use images; pulling and deleting need `image.manage`. |
| Host network (NAT, bridged) | system, granted to projects | Created by `net-admin`; `grants: [project_id] \| "all"`. Project editors attach their VMs to granted networks only. Projects sharing a network share an L2 segment: give each project its own network when they must be isolated. |
| Project network (NAT only) | project | Created and deleted by the project's `owner` (`network.manage`), usable only by that project unless explicitly shared (§6.2.1); never opened to all projects or granted by `net-admin` (`base.project-network-private`). Constraints: NAT only, no uplinks or bridged mode; the subnet is a free `/24` from netd's `nat_supernet`; the bridge name is generated (`gxp-<8 hex>`), not chosen by the user; delete is refused while a VM is attached. Counts against the `networks` quota. |
| OVS bridges, uplinks, DPDK, OVS install | system | `host.network`. |
| PCI devices (VFIO) | system, granted to projects | `/etc/glidex/control-plane.json` `pci.allow: [{bdf, projects}]`. A VM may only pass through BDFs granted to its project. |
| Host paths | system | `host.paths` (§7.4). |

A VM without a guest credential has **no login**: the generated
cloud-init seed has no SSH keys and a locked password. There are no
host-wide defaults (the control-plane user's keys, a site password), since
one key or password would open VMs in every project.

#### 6.2.1 Sharing a project network

A project network can be shared with another project only when both
sides take an explicit step:

1. **Offer.** The owner of the network's project (`network.share` on the
   network) offers it to a target project, by project **id**. Ids are
   UUIDs, so projects can't be found by guessing names. The offer
   expires after 7 days. The response doesn't say whether the target
   exists.
2. **Accept.** The owner of the target project (`network.share` on the
   target project) accepts. Only then does the target project appear in
   the network's `shares`, and its editors can attach VMs
   (`base.network-grant`). Cedar decides who may accept. Whether a
   matching, unexpired offer exists is checked in code, under the same
   ReDB transaction that records the acceptance.
3. **End.** Either side can end the share at any time:
   - the source owner *unshares*;
   - the target owner *leaves*.

   Both are refused with `409 in_use` while VMs of the target project
   are attached. Deleting the network ends all its shares and is
   refused while any VM, from any project, is attached.

The network stays in its owning project: only that project's quota
counts it, and only that project's owners can change or delete it.
Share offers, acceptances and ends are audited on both projects.
Projects sharing a network share its L2 segment (§6.2). The NAT
isolation of §8 still separates it from every other network.

VM names are unique per project. Disk names stay globally unique, so a
disk can still be named without its project. Lookups
by name need `?project=`, or resolve within the caller's default project.

### 6.3 Quotas

Per project: `vms`, `vcpus`, `memory_mib`, `disk_gib`, `running_vms`,
`networks` (project networks, default 2). New projects created without
explicit quotas get the site default `quotas.default` from
`control-plane.json` (omitted limits are unlimited, except `networks`,
which stays 2; `null` is unlimited); the `default` project takes it once,
on the first start that sees it. Unknown quota names are refused. Checked under the `VmManager`
write lock at create, start, disk create, resize and network create.
`null` means unlimited.

Over quota, the control plane asks Cedar for `exceedQuota` on the
project. If that is allowed (`system-admin`, through `quota.exceed`),
the operation goes ahead and the audit entry records
`quota_exceeded: {limit, used, requested}`. Otherwise →
`403 quota_exceeded` with the limit and current use. Quotas
themselves stay in code because they need live counts.

## 7. Authorization (Cedar)

Authorization decisions are made by [Cedar](https://www.cedarpolicy.com)
policies. The control plane evaluates them in process with the
`cedar-policy` crate (`cedar-policy = "4.13"`). There is no separate
policy service.

Why Cedar:
- Roles, project scoping, grants and host-wide rules can be written as
  policies instead of `if` chains spread over handlers.
- Policies are checked against a schema before they are loaded (strict
  validation). A typo in a policy fails startup instead of quietly
  allowing or denying.
- `forbid` always overrides `permit`. Invariants such as step-up and
  same-project-only are `forbid` policies that no role or site policy
  can override.
- Sites can add their own rules without code changes (§7.6).
- Every decision names the policies that produced it, which goes into
  the audit log (§10).

What stays in code: authentication (§5), quotas (§6.3, they need
counts), path canonicalization (§7.4), and netd's own peer check (§8).

### 7.1 Schema

File `crates/glidex-control-plane/policies/glidex.cedarschema`, embedded
with `include_str!`. Namespace `Glidex`. Abridged:

```cedarschema
namespace Glidex {
  type Auth = {
    method: String,        // "peer" | "pam" | "oidc" | "token"
    transport: String,     // "unix" | "tcp"
    age_secs: Long,        // seconds since this principal last authenticated
    source_ip?: ipaddr,    // TCP clients (from X-Forwarded-For only via glidex-ui)
  };
  type Ctx = { auth: Auth };
  type ProjectCtx = { auth: Auth, project: Project };
  type NetworkCtx = { auth: Auth, network: Network };

  entity Host;                                    // one: Host::"local"
  entity Team;
  entity User in [Team] { disabled: Bool };
  entity Token { owner?: User, expired: Bool };   // personal (owner) or service account
  entity Project in [Host];
  entity Vm in [Project] { project: Project };
  entity Disk in [Project] { project: Project };
  entity Credential in [Project] { project: Project };
  entity Image in [Host];
  // Host networks: parent Host, no `project`. Project networks: parent Project.
  // shares: projects that accepted a share of a project network (§6.2.1).
  entity Network in [Host, Project] {
    project?: Project, all_projects: Bool, grants: Set<Project>, shares: Set<Project>,
  };
  entity PciDevice in [Host] { grants: Set<Project> };

  // Roles are action groups. A role includes every role listed under it.
  action "role.system-admin";
  action "role.owner", "role.auditor", "role.net-admin", "role.image-admin"
    in ["role.system-admin"];
  action "role.editor"   in ["role.owner"];
  action "role.operator" in ["role.editor"];
  action "role.viewer"   in ["role.operator", "role.auditor"];

  // Marker group: everything in it needs a recent login (base.step-up).
  action "step-up";

  // Permission groups (§7.2).
  action "vm.read", "disk.read" in ["role.viewer"];
  action "vm.operate", "vm.console" in ["role.operator"];
  action "vm.write", "disk.write", "credential.read", "credential.write",
         "network.use" in ["role.editor"];
  action "project.members", "network.manage", "network.share" in ["role.owner"];
  action "host.read" in ["role.auditor", "role.net-admin"];
  action "policy.read", "system.audit" in ["role.auditor"];
  action "host.network", "host.devices" in ["role.net-admin"];
  action "host.network.critical" in ["host.network", "step-up"];
  action "image.manage" in ["role.image-admin"];
  action "policy.write" in ["role.system-admin", "step-up"];
  action "system.identity", "system.projects", "host.paths", "quota.exceed"
    in ["role.system-admin"];

  // Concrete actions, one per API operation (examples).
  action readVm, getConsoleInfo in ["vm.read"]
    appliesTo { principal: [User, Token], resource: [Vm], context: Ctx };
  action listVms in ["vm.read"]
    appliesTo { principal: [User, Token], resource: [Project], context: Ctx };
  action startVm, stopVm, pauseVm in ["vm.operate"]
    appliesTo { principal: [User, Token], resource: [Vm], context: Ctx };
  action openConsole in ["vm.console"]
    appliesTo { principal: [User, Token], resource: [Vm], context: Ctx };
  action createVm in ["vm.write"]
    appliesTo { principal: [User, Token], resource: [Project], context: Ctx };
  action deleteVm, attachDisk, detachDisk, attachDevice, detachDevice in ["vm.write"]
    appliesTo { principal: [User, Token], resource: [Vm], context: Ctx };
  action useDisk in ["vm.write"]
    appliesTo { principal: [User, Token], resource: [Disk], context: ProjectCtx };
  action useCredential in ["vm.write"]
    appliesTo { principal: [User, Token], resource: [Credential], context: ProjectCtx };
  action attachNetwork, detachNetwork in ["network.use"]
    appliesTo { principal: [User, Token], resource: [Vm, Project], context: Ctx };
  action useNetwork        // in no group: who may attach is `attachNetwork`
    appliesTo { principal: [User, Token], resource: [Network], context: ProjectCtx };
  action createProjectNetwork in ["network.manage"]
    appliesTo { principal: [User, Token], resource: [Project], context: Ctx };
  action deleteProjectNetwork in ["network.manage"]
    appliesTo { principal: [User, Token], resource: [Network], context: Ctx };
  // Source side, on the network; context.project is the target project.
  action offerNetworkShare, unshareNetwork in ["network.share"]
    appliesTo { principal: [User, Token], resource: [Network], context: ProjectCtx };
  // Target side, on the target project.
  action acceptNetworkShare, leaveNetworkShare in ["network.share"]
    appliesTo { principal: [User, Token], resource: [Project], context: NetworkCtx };
  action usePciDevice in ["host.devices"]
    appliesTo { principal: [User, Token], resource: [PciDevice], context: ProjectCtx };
  action useHostPath in ["host.paths"]
    appliesTo { principal: [User, Token], resource: [Host], context: Ctx };
  action exceedQuota in ["quota.exceed"]
    appliesTo { principal: [User, Token], resource: [Project], context: Ctx };
  action readOvsStatus, listBridges, listUplinks, listPciDevices in ["host.read"]
    appliesTo { principal: [User, Token], resource: [Host], context: Ctx };
  action createNetwork, deleteNetwork, grantNetwork, createBridge, ensureUplink
    in ["host.network"]
    appliesTo { principal: [User, Token], resource: [Host, Network], context: Ctx };
  action installOvs, initDpdk, confirmUplink, commitUplink, deleteUplink,
         deleteBridge in ["host.network.critical"]
    appliesTo { principal: [User, Token], resource: [Host], context: Ctx };
  action readImage, readNetwork
    appliesTo { principal: [User, Token], resource: [Image, Network], context: Ctx };
  action pullImage, deleteImage in ["image.manage"]
    appliesTo { principal: [User, Token], resource: [Host, Image], context: Ctx };
  action readPolicy, validatePolicy, simulatePolicy in ["policy.read"]
    appliesTo { principal: [User, Token], resource: [Host], context: Ctx };
  action writePolicy, deletePolicy in ["policy.write"]
    appliesTo { principal: [User, Token], resource: [Host], context: Ctx };
  // … disks, credentials, projects, teams, users, tokens, audit: same pattern.
}
```

The schema and the policies in §7.3 and §7.6 pass strict validation
with `cedar-policy` 4.13. A sample of decisions was also checked:
- a team link works for its members;
- step-up denies with `base.step-up` once the login is more than 600 s old;
- `auditor` can read VMs in every project but can't start them;
- a project owner can create a network in their own project only;
- a project network can't be granted to another project, nor attached
  to a VM in another project by someone who owns both projects;
- a cross-project `useDisk` is denied with `base.same-project`.

Rules for the schema:
- Every route maps to exactly one concrete action (`Authz::<A>` in the
  handler, §7.7). Permission groups and roles are action groups and are
  never requested directly.
- `ensure_uplink` with `confirm: true` is requested as `confirmUplink`,
  so it falls under step-up. `step-up` is a marker group in no role;
  anything placed in it needs a recent login.
- `age_secs` is `0` for peer-identified local users (§5.2) and for
  tokens. Tokens cover host-network actions only through their own
  links (§7.5).

### 7.2 Permission groups

| Group | Scope (link resource) | Covers |
|---|---|---|
| `vm.read` | project | List and get VMs, console info |
| `vm.operate` | project | Start, stop, pause |
| `vm.console` | project | Console tickets / WebSocket |
| `vm.write` | project | Create and delete VMs; attach and detach disks, devices and networks; use project disks and credentials |
| `disk.read` · `disk.write` | project | Disks: list/get · create, delete, resize, extend root |
| `credential.read` · `credential.write` | project | Guest credentials (SSH keys are only returned with `credential.read`) |
| `network.use` | project | Attach VMs to networks granted to the project |
| `project.members` | project | Role links and service-account tokens in the project |
| `network.manage` | project | Create and delete the project's own NAT networks |
| `network.share` | project | Offer and end shares of the project's networks; accept and leave shares offered to the project (§6.2.1) |
| `host.read` | host | OVS status, bridges, uplinks, `GET /pci-devices` |
| `host.network` | host | Host networks (create, delete, grant), bridges, uplinks |
| `host.network.critical` | host | OVS install, DPDK init, confirmed uplinks (IP migration), uplink commit and delete, bridge delete. Subject to step-up. |
| `host.devices` | host | PCI grants, any PCI device regardless of grants |
| `host.paths` | host | Boot and disk paths outside managed directories (§7.4) |
| `image.manage` | host | Pull and delete images |
| `policy.read` · `policy.write` | host | List, validate and simulate policies · create, update and delete site policies (step-up) |
| `quota.exceed` | host | Go over a project quota (§6.3) |
| `system.identity` · `system.projects` · `system.audit` | host | Users, identities, teams, auth settings · projects, quotas, links in any project · `GET /audit` |

### 7.3 Policies shipped with glidex

**Roles.** There is a small fixed set; site policies can add more
(§7.6).

| Role | Link resource | Includes |
|---|---|---|
| `viewer` | project | `vm.read`, `disk.read` |
| `operator` | project | viewer + `vm.operate`, `vm.console` |
| `editor` | project | operator + `vm.write`, `disk.write`, `credential.*`, `network.use` |
| `owner` | project | editor + `project.members`, `network.manage`, `network.share` (project NAT networks, §6.2) |
| `auditor` | host | viewer in every project + `host.read`, `policy.read`, `system.audit`. Read-only; no consoles, no guest SSH keys. |
| `image-admin` | host | `image.manage` |
| `net-admin` | host | `host.read`, `host.network` (including critical ops), `host.devices` |
| `system-admin` | host | every role above, plus `system.identity`, `system.projects`, `policy.write`, `host.paths`, `quota.exceed` |

Plus one single-permission grant: `grant.host-paths` (`host.paths`
without the rest of `system-admin`).

Why this set: it covers the separations a lab host needs — project
members, someone who can see everything but change nothing, someone
who curates images, someone who runs the network — and nothing more.
New shipped roles need a real use that site policies can't cover
cleanly.

**Role templates** (`policies/roles.cedar`). A role assignment is a
*template-linked policy*: `?principal` is a `User`, `Team` or `Token`,
and `?resource` is a `Project` (project roles) or `Host::"local"`
(host roles). Because `Project in Host`, a link on the host covers
every project.

```cedar
@id("role.viewer")
permit (principal in ?principal, action in Glidex::Action::"role.viewer", resource in ?resource);
@id("role.operator")
permit (principal in ?principal, action in Glidex::Action::"role.operator", resource in ?resource);
@id("role.editor")
permit (principal in ?principal, action in Glidex::Action::"role.editor", resource in ?resource);
@id("role.owner")
permit (principal in ?principal, action in Glidex::Action::"role.owner", resource in ?resource);
@id("role.auditor")
permit (principal in ?principal, action in Glidex::Action::"role.auditor", resource in ?resource);
@id("role.image-admin")
permit (principal in ?principal, action in Glidex::Action::"role.image-admin", resource in ?resource);
@id("role.net-admin")
permit (principal in ?principal, action in Glidex::Action::"role.net-admin", resource in ?resource);
@id("role.system-admin")
permit (principal in ?principal, action in Glidex::Action::"role.system-admin", resource in ?resource);
@id("grant.host-paths")
permit (principal in ?principal, action in Glidex::Action::"host.paths", resource in ?resource);
```

The control plane only links project roles to a `Project`, and host
roles and grants to `Host::"local"`. Linking needs `project.members`
(project roles, in that project) or `system.projects` (anything).

**Base policies** (`policies/base.cedar`). These are the invariants and
can't be changed or removed by configuration or the API:

```cedar
@id("base.disabled-user")
forbid (principal is Glidex::User, action, resource) when { principal.disabled };

@id("base.expired-token")
forbid (principal is Glidex::Token, action, resource) when { principal.expired };

// Step-up: host-wide network changes and policy writes need a login
// in the last 10 minutes.
@id("base.step-up")
forbid (principal, action in Glidex::Action::"step-up", resource)
unless { context.auth.age_secs <= 600 };

// A VM only uses disks and credentials of its own project.
@id("base.same-project")
forbid (principal, action in [Glidex::Action::"useDisk", Glidex::Action::"useCredential"], resource)
unless { resource.project == context.project };

// Which networks a project may use. This is a forbid so that no role can
// widen it: an owner of project A who edits project B still can't put
// A's private network on a VM in B.
@id("base.network-grant")
forbid (principal, action == Glidex::Action::"useNetwork", resource)
unless {
  resource.all_projects || resource.grants.contains(context.project) ||
  (resource has project && resource.project == context.project) ||
  (resource has project && resource.shares.contains(context.project))
};
// Who may attach is decided by attachNetwork (§7.4); useNetwork only
// checks the network against the project.
@id("base.network-use")
permit (principal, action == Glidex::Action::"useNetwork", resource);

// Which PCI devices a project may use without host.devices.
@id("base.pci-grant")
permit (principal, action == Glidex::Action::"usePciDevice", resource)
when { resource.grants.contains(context.project) };

// Project networks are never opened up by net-admin grants; sharing
// them takes the two-sided offer/accept of §6.2.1.
@id("base.project-network-private")
forbid (principal, action == Glidex::Action::"grantNetwork", resource)
when { resource has project };
@id("base.project-network-only")
forbid (principal, action in [Glidex::Action::"deleteProjectNetwork",
                              Glidex::Action::"offerNetworkShare",
                              Glidex::Action::"unshareNetwork"], resource)
unless { resource has project };
@id("base.share-project-network-only")
forbid (principal, action in [Glidex::Action::"acceptNetworkShare",
                              Glidex::Action::"leaveNetworkShare"], resource)
unless { context.network has project };
// Offering a network to its own project means nothing.
@id("base.share-elsewhere")
forbid (principal, action == Glidex::Action::"offerNetworkShare", resource)
when { resource has project && resource.project == context.project };

// The shared image library and network names are visible to every user.
@id("base.read-shared")
permit (principal, action in [Glidex::Action::"readImage", Glidex::Action::"readNetwork"], resource);

// Break-glass: root and glidex-admin over api.sock (§5.2).
@id("base.break-glass")
permit (principal in Glidex::Team::"unix:glidex-admin", action, resource);
```

`base.network-grant` and `base.pci-grant` decide *which* networks and
devices a project may use. *Who* may attach them is checked separately
by the compound requests in §7.4: the caller also needs `attachNetwork`
or `attachDevice` on the VM or project. The two rules differ on
purpose:
- The network rule is a `forbid` and binds everyone, `net-admin`
  included. A network not granted to the project has to be granted
  first.
- The PCI rule is a `permit`, so `host.devices` (`net-admin`) can still
  pass through any device.

Teams named `unix:*` get members only from `SO_PEERCRED` groups on
`api.sock`. They can't be created, joined or synced from OIDC, so OIDC
users can't reach `base.break-glass`.

### 7.4 Compound requests

Some API calls need more than one decision. **All of them must
allow.**

| API call | Cedar requests |
|---|---|
| `POST /vms` | `createVm` on the Project; `useDisk` on each data disk; `useCredential` on each credential; `attachNetwork` on the Project and `useNetwork` on each network if any; `usePciDevice` on each VFIO device; `useHostPath` on `Host::"local"` if any path is outside managed directories; `readImage` on the boot image |
| `POST /vms/{id}/disks` | `attachDisk` on the Vm; `useDisk` on the Disk |
| `POST /vms/{id}/devices` | `attachDevice` on the Vm; `usePciDevice` on the device |
| `POST /vms/{id}/networks` | `attachNetwork` on the Vm; `useNetwork` on the network |
| `POST /ovs/bridges/{b}/uplinks` with `confirm: true` | `confirmUplink` on `Host::"local"` |

**Paths.** Any explicit `kernel_image_path`, `rootfs_path` or
`cloud_init_path` needs `useHostPath`. So does a `firmware_path` other
than the default firmware files of the two hypervisors (compared after
`canonicalize`). Managed disks are named with `image`, `root_disk` and
`data_disks` instead. Paths into the disk or image directories are not
treated as managed: they would reach other projects' disks, or let a VM
write to a shared base image.

**VFIO devices** must be `/sys/bus/pci/devices/<DDDD:BB:DD.F>` (or the
bare address); anything else is refused before authorization.

**Not found vs forbidden.** For a project resource the control plane
first asks `readVm` (or `readDisk`, …). If that is denied the answer is
`404`. If the read is allowed but the requested action isn't, `403`.

### 7.5 Tokens

A token is a Cedar principal of its own (`Token`), with its own links.
- A **personal token** (`owner` set) with no links of its own acts as
  its owner. With links, it is evaluated **twice**, as the `Token` and
  as its owner `User`, and allowed only if both allow. Either way it can
  never exceed its owner, and removing the owner's role narrows the
  token at once.
- A **service-account token** (no `owner`) is evaluated once. It can
  only be linked to roles in its own project.

Step-up actions through a token need the token to be linked to the
role itself (e.g. `role.net-admin`; and its owner, for a personal
token). A personal token acting purely as its owner never counts as a
recent login.

### 7.6 Site policies

Site policies are additional Cedar policies that system administrators
manage **through the API**; a UI page comes later (§15). They can add
`permit`s (custom roles) and `forbid`s (restrictions). They can't relax
the base policies, because `forbid` always wins.

**Storage.**
- ReDB `site_policies`: `id`, `text`, `description`, `enabled`,
  `version` (u64), `updated_by`, `updated_at`.
- Every change also writes a row to `site_policy_versions` (`id`,
  `version`) → `text`, `author`, `time`. The last 50 versions of each
  policy are kept.
- Policy text isn't secret, but it describes the site's access rules:
  it's readable only with `policy.read`.

**Writing** (`PUT /authz/policies/{id}`, `policy.write`, step-up):
1. **One policy per request.** The text must parse as exactly one
   policy, not a template. Its `@id` must equal `{id}` and start with
   `site.`. The `base.`, `role.`, `grant.` and `link.` prefixes are
   reserved.
2. **Optimistic concurrency.** The request carries the current
   `version` (`0` to create). A mismatch returns `409 conflict`.
3. **Strict validation of the whole set.** The candidate set (current
   set plus this change) is validated in strict mode. Errors return
   `422` with the validator messages.
4. **Lock-out check.** Under the candidate set the caller must still
   be allowed `writePolicy` on `Host::"local"`. Otherwise the write is
   refused with `409 would_lock_out`.
5. **Commit.** The row and its version history are written in one ReDB
   transaction, then the in-memory set is swapped (§7.7). Delete and
   enable/disable follow the same steps.

**Limits.** At most 64 KiB per policy and 200 site policies.

**Tooling.**
- `POST /authz/validate` (`policy.read`) validates a candidate policy
  without saving it.
- `POST /authz/simulate` (`policy.read`) takes candidate changes and a
  list of `{principal, action, resource, context?}` requests. It
  returns the decision and the determining policy ids under both the
  current and the candidate set. The UI's policy editor will use it to
  show what a change does before it's saved.
- `glidex-control-plane --check-policies <file>` validates offline
  against the shipped schema.

**Break-glass is immune to site policies.** Requests from principals in
`unix:glidex-admin` (root and `glidex-admin` on `api.sock`) are
evaluated against the **base policies only**, without site policies or
links. However badly a site policy is written, a local administrator
can always remove it. This is the only principal for which the policy
set differs.

**Read-only file source (optional).** Hosts under configuration
management may also place policies in `/etc/glidex/policies/*.cedar`
(`root:glidex 0640`). They're loaded at startup and on
`POST /authz/reload` (`policy.write`). The API lists them as source
`file` and refuses to modify them. File and API ids must not collide.

Examples:

```cedar
// Consoles only from the campus network.
@id("site.console-campus-only")
forbid (principal, action == Glidex::Action::"openConsole", resource)
when { context.auth has source_ip && !context.auth.source_ip.isInRange(ip("10.0.0.0/8")) };

// No device passthrough in the teaching project.
@id("site.teaching-no-vfio")
forbid (principal, action == Glidex::Action::"usePciDevice", resource)
when { context.project == Glidex::Project::"<teaching-project-id>" };

// Lab assistants may start, stop and open consoles in every project.
@id("site.lab-assistants")
permit (principal in Glidex::Team::"<lab-assistants-team-id>",
        action in [Glidex::Action::"vm.operate", Glidex::Action::"vm.console", Glidex::Action::"vm.read"],
        resource in Glidex::Host::"local");
```

Policies refer to projects and teams by id (UUID), not by name, so a
rename doesn't change their meaning. `GET /authz/policies` lists every
loaded policy and template with its id, source (`base`, `role`, `link`,
`site`, `file`), version and text.

### 7.7 Enforcement

```rust
// crates/glidex-control-plane/src/authz.rs (sketch)
pub struct Authz {
    schema: cedar_policy::Schema,
    policies: arc_swap::ArcSwap<cedar_policy::PolicySet>,
    authorizer: cedar_policy::Authorizer,
}

impl Authz {
    /// Allow only on Decision::Allow with no evaluation errors.
    pub fn check(&self, p: &Principal, a: Action, r: &ResourceRef, ctx: Ctx)
        -> Result<Allowed, Denied>;
}
```

1. **Building the policy set.** The set is base + role templates +
   links (from the `policy_links` table) + enabled site policies (ReDB
   and files). A second set, base only, serves break-glass principals
   (§7.6). It is
   validated in `ValidationMode::Strict` and published with an atomic
   swap.
   Cedar gives policies parsed from text the ids `policy0`, `policy1`
   and so on. The loader therefore re-registers each policy and template
   under its `@id` annotation, and refuses a policy with no `@id` or a
   duplicate one. Links get the id `link.<uuid>`, the same as their
   `policy_links` row. A link or site-policy change rebuilds and swaps
   the set before the API call returns. Revocation takes effect on the next request.
2. **Entities for each request.** Only the entities needed are built:
   the principal, its teams, the owner (for tokens), the resource and
   its ancestors (Project, Host), and any entity named in the context.
   They're built with `Entities::from_entities(…, Some(&schema))`, so
   attribute types are checked too. There's no global entity store and
   no caching across requests.
3. **Errors.** A Deny whose determining policies include `base.step-up`
   becomes `401 reauth_required`. Other Denies become `403` (or `404`,
   §7.4). The determining policy ids go to the audit log, not to the
   client.
4. **Fail closed.** Cedar skips a policy whose evaluation errors. For a
   `forbid` that would mean *allow*. So `check` denies whenever
   `response.diagnostics().errors()` is non-empty and logs the error.
   Strict validation should make this unreachable; a test pins it
   (§14).
5. **Requests.** `Request::new(principal, action, resource, context,
   Some(&schema))`, so the request is validated too: wrong resource
   types are rejected before evaluation.
6. **Handlers.** Every route is declared once in `api::routes` with its
   action; a route layer hands the action to the handler through the
   `Caller` extractor and records each decision for the audit entry.
   A test checks every route's action exists in the schema. Managers
   don't take a proof value; instead they re-check project consistency
   themselves (a VM only gets disks, credentials and networks of its
   project), so a handler mistake can't cross projects.
7. **Lists.** `GET /vms` and similar first narrow to projects where the
   caller has a link (directly, through a team, or on the host), then
   check `readVm` per item. Each check takes microseconds at
   single-host scale. Cedar's partial evaluation is experimental and
   isn't used.
8. **UI capability checks.** `POST /authz/check` takes
   `[{action, resource}]` and returns booleans for the caller, so the
   UI can hide what the caller can't do. The server still checks every
   real request.

## 8. `glidex-netd`

netd does **not** use Cedar. It stays a small root process with a
peer-uid check and a per-op allowlist. The control plane is the policy
point for users (decision 2). netd only decides which local processes
may ask it for what.

1. **Policy** in `/etc/glidex/netd.json`:
   ```json
   { "policy": { "glidex": ["*"], "glidex-admin": ["*"] } }
   ```
   Group name → allowed ops (`*` or op names, `list_*` glob). The default
   is the above (decision 2); a host may narrow it, e.g. to stop the
   control plane from installing packages. A denied op returns
   `permission_denied`.
2. **Ownership.** `detach_vm_port` and `release_vm` compare the
   stored `owner_uid` with the peer uid and return `not_owned` on a
   mismatch. Root is exempt.
3. **Audit context.** Mutating requests may carry
   `"on_behalf_of": {"user": "…", "project": "…", "request_id": "…"}`.
   netd logs it next to the peer uid. It is informational and never
   used for authorization.
4. **Isolation between NAT networks.** The forward chain of
   `inet glidex` drops traffic from one glidex NAT bridge to another
   (`iifname "gx*" oifname "gx*" iifname != oifname drop`, placed before
   the per-bridge accepts). An `input` chain lets traffic in from NAT
   bridges only for DHCP (udp 67) and DNS (udp/tcp 53) to the gateway,
   and drops the rest. Project networks (§6.2) depend on this.
5. **Project networks need no new ops.** The control plane creates
   them with `ensure_bridge` and `ensure_nat` like host networks; netd
   doesn't know about projects.
6. The status socket (`netd-ro.sock`, `hello` and `probe`) is unchanged.

## 9. Process and host hardening

| Unit | Changes |
|---|---|
| `glidex-control-plane` | `RuntimeDirectory=glidex-cp` (`0755`, preserved across restarts) holds `api.sock`, `ui.sock` and `vms/` (`0700`); per-VM files in `/run/glidex-cp/vms/<id>/` (`0700`): API socket, console socket, console log, cloud-init seed. `UMask=0077`. `NoNewPrivileges=yes`, `PrivateTmp=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `ReadWritePaths=/var/lib/glidex-control-plane`, `DevicePolicy=closed`, `DeviceAllow=/dev/kvm rw`, `DeviceAllow=char-vfio rw`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK`, `LoadCredential=` for `tls-key`, `oidc-client-secret`. |
| `glidex-ui` | `User=glidex-ui`, `InaccessiblePaths=/run/glidex /var/lib/glidex-control-plane /run/glidex-authd`, `CapabilityBoundingSet=`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`, existing `ProtectSystem`/`PrivateDevices` kept. |
| `glidex-netd` | `ProtectHome=yes`, `CapabilityBoundingSet` limited to what the op set needs (to verify on a host: `CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN CAP_DAC_OVERRIDE CAP_CHOWN CAP_FOWNER`, plus package-manager needs for `install_ovs`). |
| `glidex-authd` | Socket-activated, `PrivateNetwork=yes`, `ProtectSystem=strict`, `ProtectHome=read-only`, `NoNewPrivileges=yes` (PAM modules that need setuid helpers are not supported). |

- **Not set on the control-plane unit: `NoNewPrivileges`,
  `RestrictAddressFamilies`, `LockPersonality`** (and anything else that
  makes systemd set no_new_privs for a non-root unit, such as
  `SystemCallFilter` or `PrivateDevices`). cloud-hypervisor gets
  `CAP_NET_ADMIN` from its file capability to bring taps up, and the
  kernel ignores file capabilities under no_new_privs. They can come back
  once netd fully prepares taps, so the hypervisor needs no capability.
  The unit also makes `/run/glidex/vhost` writable for vhost-user sockets.
- Data: `.glidex/{images,disks}` become `0700`; `glidex.db` stays `0600`.
- Installer supply chain: pin the rustup-init and Bun versions and
  verify their published sha256 before running them, instead of
  `curl … | sh`.

Why per-VM `0700`: the hypervisor API socket (CH HTTP, QMP) gives full
control of the VM, with no glidex checks. Only the control plane may
open it. Humans reach consoles through `vm.console` (§5.6). `gxctl
connect` uses the API's console WebSocket instead of the raw socket.

## 10. Audit

- One JSON line per mutating request, and per login, logout, failed
  login, token create/revoke, role link change, and policy reload. It goes to the journal
  (`SYSLOG_IDENTIFIER=glidex-audit`) and to an `audit` ReDB table kept
  for 90 days (`audit.retention_days`).
- Fields: `time`, `request_id`, `principal` (user id, display name,
  method, token or session id prefix), `source` (peer uid or client IP),
  `action` (Cedar action or netd op), `project`, `target`, `result`,
  `error_code`, and `policies`: the ids of the Cedar policies that
  determined the decision (`response.diagnostics().reason()`), e.g.
  `["role-link:3f2a…"]` or `["base.step-up"]`.
- Never logged: passwords, tokens, session ids, cookies, OIDC codes or
  tokens, guest password hashes, SSH private keys. Request bodies are
  logged only for an allowlist of non-secret fields.
- `GET /audit?project=&user=&since=` needs `system.audit`, or
  `project.members` for that project's entries.

## 11. Bootstrap and migration

1. The installer creates `glidex-ui`, `glidex-users` and `glidex-admin`,
   adds the invoking user to `glidex-users` and `glidex-admin`, and
   **removes** that user from `glidex` if a previous install added them.
2. On first start with the new schema the control plane creates project
   `default`, assigns every existing VM, disk and guest credential to
   it, and grants every existing network to `default`.
3. On first start (no `role.system-admin` link exists yet) every member
   of `glidex-admin` (the installer added the invoking user) gets
   `role.system-admin` on `Host::"local"` and `role.owner` on
   `default`, for its `unix:<name>` identity (which a PAM login of the
   same name shares). No default passwords or tokens are created.
4. OIDC stays disabled until `oidc.issuer` and the client credentials
   are configured. PAM is enabled when `glidex-authd` is installed.
5. There is no switch to turn authentication off in the binary. The
   library keeps `api::create_router` with authentication disabled
   (every request is a break-glass principal, still decided by Cedar)
   for embedding and tests.

## 12. Out of scope

- Hypervisor escape: all VMs run as the `glidex` uid, so an escape
  reaches every project. Per-VM uids or a jailer are future work.
- Multi-host, federation, and SCIM provisioning.
- Cedar partial evaluation for list queries (experimental upstream; §7.7).
- Encryption of disks at rest.
- Fine-grained network policy between VMs on a shared network.

## 13. Configuration and API additions

`/etc/glidex/control-plane.json` (`root:glidex 0640`, no secrets):

```json
{
  "listen": ["127.0.0.1:8841", "[::1]:8841"],
  "tls": { "cert": "/etc/glidex/tls/cp.crt" },
  "auth": {
    "allowed_origins": ["http://localhost:5173"],
    "session": { "idle_minutes": 30, "absolute_hours": 12 },
    "tokens": { "default_days": 90, "max_days": 365 },
    "pam": {
      "enabled": true,
      "allowed_groups": ["glidex-users"],
      "jit": true,
      "group_teams": { "unix-group": "team-name" }
    },
    "oidc": {
      "enabled": false,
      "issuer": "https://idp.example.org",
      "client_id": "glidex",
      "scopes": ["openid", "profile", "email", "groups"],
      "groups_claim": "groups",
      "jit": true,
      "allowed_domains": [],
      "required_groups": [],
      "group_teams": { "idp-group": "team-name" }
    }
  },
  "authz": { "policy_files_dir": "/etc/glidex/policies", "policy_history": 50 },
  "quotas": { "default": { "networks": 2 } },
  "pci": { "allow": [] },
  "audit": { "retention_days": 90 }
}
```

New endpoints:

| Endpoint | Permission |
|---|---|
| `GET /auth/methods` | none |
| `POST /auth/login` (`{method: "pam", username, password}`) | none |
| `GET /auth/oidc/start`, `GET /auth/oidc/callback` | none |
| `POST /auth/logout`, `GET /auth/whoami` | authenticated |
| `GET/POST/DELETE /tokens` | own tokens; `project.members` for service accounts; `system.identity` for others' |
| `GET/POST/PATCH /users`, `/users/{id}/identities` | `system.identity` (`GET /users/me` for self) |
| `GET/POST/PATCH/DELETE /teams`, `/teams/{id}/members` | `system.identity` |
| `GET /projects` | lists projects the caller has any link in |
| `POST/PATCH/DELETE /projects` (quotas included) | `system.projects` |
| `GET/PUT/DELETE /projects/{id}/bindings` | `project.members` (project-role links only; cannot link system roles) |
| `GET/PUT/DELETE /system/bindings` | `system.projects` (system-role and grant links on the host) |
| `POST /authz/check` | authenticated (answers for the caller only) |
| `GET /authz/policies`, `GET /authz/policies/{id}`, `GET /authz/policies/{id}/versions` | `policy.read` |
| `PUT /authz/policies/{id}`, `DELETE /authz/policies/{id}`, `POST /authz/reload` | `policy.write` (step-up) |
| `POST /authz/validate`, `POST /authz/simulate` | `policy.read` |
| `POST /projects/{id}/networks`, `DELETE /networks/{name}` (project network) | `network.manage` |
| `POST /networks/{name}/shares` (`{project}`), `DELETE /networks/{name}/shares/{project}` | `network.share` on the network (offer, unshare) |
| `GET /projects/{id}/network-shares`, `POST /projects/{id}/network-shares/{network}/accept`, `DELETE /projects/{id}/network-shares/{network}` | `network.share` on the target project (list offers, accept, leave) |
| `PUT /networks/{name}/grants` (`{grants, all_projects}`) | `host.network` |
| `POST /auth/session` | a peer-identified user on api.sock: a browser session for themselves (without break-glass) |
| `GET /vms/{id}/console/log` | `vm.console` (console output replaces reading the log file) |
| `PATCH /users/me` (`{default_project}`) | authenticated |
| `GET /users/me/ssh-keys` | authenticated (the caller's own local account only; `available: false` with a reason otherwise) |
| `POST /auth/oidc/device`, `POST /auth/oidc/device/poll` | none (device grant for gxctl; returns a 1-day personal token) |
| `POST /vms/{id}/console/ticket` | `vm.console` |
| `GET /audit` | §10 |

Existing endpoints gain `project` in responses and `?project=` on
lists. `cedar-policy = "4.13"` and `arc-swap` are added to the
control plane's dependencies. The error model gains `401 unauthenticated`, `401
reauth_required`, `403 forbidden`, `403 quota_exceeded`, `409
would_lock_out`, `422 invalid_policy`.

ReDB tables (control plane): `users`, `identities`, `teams`,
`team_members`, `projects`, `policy_links`, `site_policies`,
`site_policy_versions` (`id, template, principal,
resource`; turned into linked policies at load), `sessions`, `api_tokens`,
`audit`; `vms`, `disks` and `credentials` records gain `project_id`.

## 14. Testing

- **Route coverage:** a test walks `create_router` and fails for any
  route without an `Authz` action (except the §5 allowlist), or whose
  action isn't in the schema.
- **Policy validation:** base, roles and the example site policies pass
  `ValidationMode::Strict` against the schema in CI.
- **Policy decision tests:** table-driven `(principal, action, resource,
  context) → Allow/Deny + determining policy ids`. They cover each role,
  team inheritance, host links covering projects, cross-project denials,
  `base.same-project`, network and PCI grants, step-up at 599/601 s,
  disabled users, expired tokens, the token ∩ owner rule, break-glass
  only through `unix:` teams, and site `forbid` beating a role `permit`.
- **Fail closed:** a policy forced to error at runtime (validation
  bypassed in the test) gives Deny.
- **Site policy API:**
  - an invalid policy gets `422` and the old set still answers;
  - a stale `version` gets `409`;
  - a reserved id prefix is refused;
  - a policy that would forbid the caller's `writePolicy` gets
    `would_lock_out`;
  - break-glass still works under a site `forbid (principal, action,
    resource);`;
  - `simulate` reports both decisions.
- **Quotas:** a `system-admin` over quota succeeds and is audited; an
  `owner` over quota gets `403`.
- **PAM provisioning:** first login creates a user with no rights;
  `group_teams` sync adds and removes `source: pam` memberships.
- **Project networks:** an owner creates one, it's unusable from
  another project, it counts against the quota;
- **Network sharing:**
  - an offer alone grants nothing;
  - after the target owner accepts, the target's editors can attach;
  - an editor (not owner) can neither offer nor accept;
  - unshare or leave gets `409 in_use` while the target's VMs are attached;
  - an offer expires after 7 days;
  - `grantNetwork` on a project network is still refused.
- **NAT isolation (host test):** VMs on two NAT networks can't reach
  each other or host services other than DHCP/DNS.
- **API tests:**
  - `401` without credentials;
  - `403` with a wrong role;
  - `404` across projects;
  - quota errors;
  - `reauth_required` after 10 minutes;
  - a bad `Origin` is refused;
  - missing CSRF fails;
  - a reused or expired console ticket fails;
  - a token never exceeds its owner.
- **Path tests:** symlink and `..` escapes are refused without `host.paths`.
- **netd:** policy denial; `not_owned` on detach and release from another uid.
- **authd:** a peer other than `glidex` is rejected; rate limit; an
  expired account fails `pam_acct_mgmt` (with a test PAM stack).
- **OIDC:** against a local mock IdP. Cases: wrong `aud`, `iss`,
  `nonce`, expired token, unknown `kid` refresh, `alg: none` refused,
  group-to-team sync.
- **UI e2e:** login, CSRF header, console via ticket, a foreign `Host` is refused.

## 15. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **S0** containment | netd owner checks; NAT isolation (§8.4); `glidex-ui` user; per-VM runtime dirs under `/run/glidex-cp` with `UMask=0077` and `PrivateTmp`; UI `Host` check; control plane `Origin` check; refuse non-loopback listeners without TLS | Existing tests pass; new netd ownership and Origin/Host tests. |
| **S1** identity | `api.sock` with peer identity; gxctl on `api.sock`, console through the WebSocket; users/identities tables; sessions; tokens; TLS; `glidex-authd` + PAM login with first-login provisioning and `group_teams`; console tickets; CSRF; `GLIDEX_AUTH=off` | §14 auth tests; installer creates groups and stops adding humans to `glidex`. |
| **S2** tenancy and Cedar | `authz.rs` with `cedar-policy`; schema, base and role policies; `policy_links`; projects, teams, quotas with `exceedQuota`; the eight shipped roles; `Authz<A>` on every route; compound requests; migration to `default`; path and PCI rules; network grants; project networks and sharing; `/authz/check` | Route-coverage, strict validation and policy decision tests; cross-project API tests; migration test from a pre-S2 database. |
| **S3** OIDC | code + PKCE, JWKS, JIT, group-to-team mapping, device grant for gxctl | Mock-IdP tests. |
| **S4** netd policy, step-up, site policies | `netd.json` policy; `on_behalf_of`; `base.step-up` mapped to `401 reauth_required`; site policy API (store, versions, validate, simulate, lock-out check, break-glass set), file source, `--check-policies` | netd policy, reauth and site policy API tests. |
| **S5** audit and hardening | audit table and `GET /audit`; systemd hardening of all units; pinned toolchain installs; remove `GLIDEX_AUTH=off` | Audit redaction test; units verified on a host. |
| **S6** UI for access control | Login page (PAM and OIDC); projects, teams and role links; tokens; policy editor using `validate` and `simulate` | UI e2e tests. |

## 16. Open questions

None. Earlier questions were answered and recorded as decisions 10–17 (§2).

## 17. Implementation status

| Spec | Code |
|---|---|
| Cedar schema and shipped policies (§7.1, §7.3) | `crates/glidex-control-plane/policies/`, engine in `src/authz.rs` |
| Projects, quotas, migration (§6, §11) | `src/tenancy.rs`, `src/state.rs` (`create_vm_in`, `usage_locked`, `adopt_into_default_project`) |
| Identity store, principals, sessions, tokens, tickets, audit (§5, §10) | `src/auth/mod.rs`, `src/auth/store.rs` |
| OIDC (§5.4) | `src/auth/oidc.rs` |
| API, route table, compound checks (§7.4, §7.7, §13) | `src/api/` |
| Listeners, TLS (§5.1) | `src/serve.rs`, `src/config.rs` |
| Private VM runtime directories (§9) | `src/paths.rs` |
| PAM helper (§5.3) | `crates/glidex-authd` |
| netd ownership, policy, admin socket, audit context, NAT isolation (§8) | `crates/glidex-netd`, `crates/glidex-ovs/src/nat.rs` |
| Tests (§14) | `tests/security_tests.rs`, `tests/network_tests.rs`, unit tests in each module, `crates/glidex-authd/tests`, `crates/glidex-netd/tests` |

Still to verify on a real host: the generated nftables rules (§8.4),
systemd sandboxing with VFIO, hugepages and taps (§9), a real PAM stack
including expired accounts, and OIDC against the production IdP.

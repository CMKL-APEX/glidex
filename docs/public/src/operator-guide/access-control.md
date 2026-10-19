# Access control

Once a request is [authenticated](authentication.md), Glidex decides
whether it is allowed with [Cedar](https://www.cedarpolicy.com/)
policies. You rarely write policies yourself: you give people **roles**
through **bindings**, and the shipped policies turn those into
decisions. Site policies add rules on top when roles aren't enough.

The pieces:

| | |
|---|---|
| **Users** | People, from the local socket, a password login or single sign-on. |
| **Teams** | Groups of users. A role given to a team applies to its members. |
| **Tokens** | Personal tokens act for their owner; service-account tokens have their own roles (see [Access tokens](authentication.md#access-tokens)). |
| **Projects** | Hold VMs, disks, credentials and project networks, with quotas. |
| **Tenants** | Own projects. A host has tenant `admin`, which operates the host, and tenant `default`, which holds the projects. |
| **Bindings** | A role, given to a user, team or token, on a project, a tenant or the host. |
| **Policies** | Base policies and role definitions shipped with Glidex, plus your site policies. |

[Concepts](../concepts.md) explains projects and tenants for users; [Projects](../user-guide/projects.md)
covers working inside one.

In the web UI, the **Directory** page (in the header) gathers these, one
tab each, shown to those allowed to see it:

| Tab | Holds |
|---|---|
| **Overview** | The directory, the tenant that owns it, counts of tenants, users, teams and connectors, and what belongs to the directory versus this cluster. |
| **Tenants** | Tenants, and creating one; each tenant has its own page. |
| **Users & roles** | Users and their sign-in methods, teams, and system roles. |
| **Connectors** | Login connectors (see [Login connectors](authentication.md#login-connectors)). |
| **Policies** | Base and site policies (see [Site policies](#site-policies)). |

The directory holds users, teams, connectors, tenants, projects, tenant
and project roles, and tokens; every server of a cluster has the same
copy. System roles, site policies and admissions belong to the cluster.

## Safe defaults

Out of the box:

- Nobody can do anything without a role, except read the image catalog
  and network names, and manage their own tokens.
- Root and members of `glidex-admin` on the local socket are break-glass
  administrators: always allowed, whatever the bindings and site
  policies. Use it to recover, not for daily work.
- A VM only uses disks and credentials of its own project, and only
  networks its project was granted. No role or site policy can change
  that.
- A disabled user and an expired token are refused everything.
- Changes that can cut a host off, and policy writes, need a recent
  login (see [Sessions and
  re-authentication](authentication.md#sessions-and-re-authentication)).

## The first administrators

On its first start the control plane makes every member of
`glidex-admin` (the installer adds you) a Glidex administrator:

- `system-admin` on the host;
- `owner` of the `default` project;
- member of the `admin` tenant, `tenant-admin` of `admin` and `default`,
  and `directory-admin` of the host's directory.

"Members" are the users listed in the group (`usermod -aG` adds them
there), plus the user the control plane runs as, if that user belongs
to the group (its primary group counts). The installed service runs as
`glidex`, which doesn't, so on a normal install it is the listed
members; the second case is for a control plane you run by hand as
yourself.

It does this only while nobody has `system-admin`, so adding people to
`glidex-admin` later makes them break-glass users on the socket, but
gives them no roles in the web UI. Give roles instead.

<!-- TODO(verify): the API offers no call to give directory-admin / directory-auditor to more people (only the first-start bootstrap grants them); confirm before documenting how. -->

## Roles

### Project roles

Given on one project. Each includes the one above it.

| Role | May |
|---|---|
| `viewer` | See the project, its VMs, disks and usage. |
| `operator` | Also start, stop and pause VMs, and open their consoles. |
| `editor` | Also create, change and delete VMs and disks, manage credentials, and attach networks. |
| `owner` | Also manage the project's bindings and service-account tokens, its networks and network sharing, and read its audit log. |

### Host roles

Given on the host (on a cluster, on every host of it; some can be given
on [one host](#on-one-host-of-a-cluster) instead). Only members of the
`admin` tenant can have them.

| Role | May |
|---|---|
| `auditor` | Read everything (as `viewer` on every project), the policies, the audit log and usage. |
| `image-admin` | Manage the shared image library. |
| `net-admin` | Manage host networking: bridges, uplinks, Open vSwitch, PCI devices. |
| `system-admin` | Everything above, as `owner` on every project, plus policies, bindings, cluster membership and admissions, host paths, and going over quotas. |
| `grant.host-paths` | Only: give the API paths of files on the host (disk images, kernels). |

### Tenant roles

| Role | May |
|---|---|
| `tenant-admin` | Create and delete the tenant's projects, set their quotas, manage the tenant's members and bindings. Includes `owner` on its projects. |
| `tenant-auditor` | Read the tenant, its members and bindings, and its projects. |

## Giving roles

Bindings name a principal as `user:<id>`, `team:<id>` or `token:<id>`.
Find ids with `gxctl user list`, `gxctl team list` and `gxctl token
list`.

On a project (also on the project's page in the web UI):

```bash
gxctl binding add web editor user:3f0c…
gxctl binding add web operator team:<team-id>
gxctl binding list web
gxctl binding remove web <binding-id>
```

On the host (the web UI's **Directory** page, **Users & roles**, **System
roles**). The person must be in the `admin` tenant first:

```bash
gxctl tenant add-member admin user:3f0c…
gxctl system-binding add net-admin user:3f0c…
gxctl system-binding list
gxctl system-binding remove <binding-id>
```

### On one host of a cluster

In a cluster, a system role covers every host. `auditor`, `net-admin`
and the host-paths grant can instead be given on one host, by node name
or id:

```bash
gxctl system-binding add net-admin user:3f0c… --node hv2
gxctl system-binding list --node hv2
```

| | Whole cluster | One host |
|---|---|---|
| `auditor` | Reads everything, cluster-wide | Reads that node only, not the node list or other hosts |
| `net-admin` | Every host's networking and devices | That host's Open vSwitch, uplinks and devices only |
| `grant.host-paths` | Host paths on every host | Host paths on that host |
| `system-admin`, `image-admin` | As above | Not possible: `400` (they act on cluster-wide things) |

- `system-binding list` without `--node` shows the cluster's bindings
  and every host's; a per-host one shows `on host:<node-id>`.
- The person must still be a member of the `admin` tenant.
- An unknown node is `404`; a node that has left is `409 node_departed`.
- A binding on a host that later leaves the cluster does nothing (node
  ids aren't reused); remove it with `system-binding remove`.
- In the web UI: the **System roles** card of **Users & roles** has a
  **Scope** column and, for a new binding, a **Scope** choice (the whole
  cluster or one host); a node's page has a **Roles on this host** card.
  A standalone host has no scope to choose.

On a tenant:

```bash
gxctl tenant bind default tenant-admin user:3f0c…
gxctl tenant bindings default
```

A binding takes effect at once, on every server of a cluster.

### Teams

```bash
gxctl team create staff
gxctl team add-member staff <user-id>
gxctl team remove-member staff <user-id>
```

Teams can also be filled from outside:

- `group_teams` of a login connector maps a Unix or identity provider
  group to a team; the user's membership is updated at every login.
  Memberships you added by hand are left alone.
- On the local socket, each of the caller's Unix groups is a team
  `unix:<group>`. Bind it to give a whole Unix group a role for `gxctl`
  on the host: `gxctl binding add web operator team:unix:lab`. It applies
  only to requests on the socket.

### Tenants and admissions

On one host the tenants are mostly bookkeeping: `default` holds the
projects, `admin` operates the host. You can add tenants to keep groups
of projects apart: a tenant's members only reach its own projects, and
a tenant only allocates on a host that admits it, within its cap.

```bash
gxctl tenant create research --name "Research group"
gxctl admission set research vms=20 memory_mib=131072
gxctl admission list
gxctl tenant list
```

The web UI has the **Directory** page's **Tenants** tab, and an
**Admissions** page, for the same.

#### A new tenant's first administrator

When creating a tenant in the web UI, you can fill in **First
administrator's email** and **Signs in through** (a connector). Glidex
then creates an [invitation](authentication.md#invitations) for that
address as the tenant's `tenant-admin`, and shows its link once: send it
to them. If the invitation can't be made (a bad address, an unusable
connector), the tenant isn't created either. Over the API,
`POST /tenants` takes `first_admin: {email, connector, days}` and
answers the link in `invitation_link`, once. `gxctl tenant create` has
no option for it; create the tenant, then `gxctl invite create <tenant>
<email> --connector <c> --role role.tenant-admin`.

## Removing people

From the **Users & roles** tab (needs `directory-admin`):

- **Disable** a user: their sessions end and their tokens stop working,
  but the user, their roles and their history stay. **Enable** undoes it.
- **Remove** one of a user's sign-in methods (a user can have several:
  a local account, a Google account, a JumpCloud one). A user's last
  sign-in method can only be removed once the user is disabled. Their
  sessions and tokens belong to the user, not to the sign-in method, so
  they keep working until they expire or are revoked; over the API,
  `DELETE /users/{id}/identities/{key}?end_sessions=true` ends the
  sessions too.
- **Delete** a user, after typing their name to confirm. One change
  removes the user with their sign-in methods, sessions, personal
  tokens, team memberships, role bindings and tenant memberships; the
  page then says how many of each went.

Deleting keeps the service-account tokens the user made (they belong to
their project), the audit log entries, and invitations they took. It
can't be undone. It is refused:

- for yourself: `409 cannot_delete_self` (another administrator can);
- for the last holder of `system-admin` or `directory-admin` on its
  resource (a team that holds it counts): `409 last_admin`. Give the role
  to someone else first.

A deleted person who can still sign in (a local account in
`glidex-users`, a connector that creates users) gets a new, empty user
at their next login. To keep someone out, disable them, or remove their
access at the source. `gxctl` has no command for deleting a user; the
API has `DELETE /users/{id}`.
<!-- TODO(verify): directory-admin can't be given through the API, so the last directory-admin from the first start can't be deleted; confirm whether that is intended. -->

## Quotas

Each project has quotas: `vms`, `vcpus`, `memory_mib`, `disk_gib`,
`running_vms`, `networks`, plus `routers` and `external_ips` for
cluster networks. New projects get `quotas.default` from
`control-plane.json` (by default 2 networks, 1 router, 1 external
address, the rest unlimited). Tenant administrators change them:

```bash
gxctl project quota web vms=10 vcpus=32 memory_mib=none
gxctl project show web
```

`none` means unlimited. A request that would go over a quota is refused,
except from a `system-admin`, whose overruns are recorded in the audit
log.

## Site policies

Site policies are Cedar policies you add. They can:

- **forbid** something roles would allow. A forbid always wins.
- **permit** something no role covers.

They cannot relax the base policies: those are forbids too.

### Writing one

Each site policy is one Cedar policy whose `@id` starts with `site.`
followed by 1–64 of `a-z`, `0-9`, `.`, `_`, `-`. It uses the `Glidex`
namespace. For example, refuse consoles over the network unless the
login was multi-factor:

```cedar
@id("site.console-needs-mfa")
forbid (principal, action == Glidex::Action::"openConsole", resource)
when { context.auth.transport == "tcp" && !context.auth.mfa };
```

Or protect one project's VMs from deletion:

```cedar
@id("site.prod-no-delete")
forbid (principal, action == Glidex::Action::"deleteVm", resource)
when { resource.project == Glidex::Project::"<project-id>" };
```

What a policy can look at:

- `principal`: a `Glidex::User`, `Glidex::Team` (via `in`) or
  `Glidex::Token`;
- `action`: one action per API operation, such as `openConsole`,
  `createVm`, `deleteVm`, `startVm`, `stopVm`;
- `resource`: the object, such as a `Glidex::Vm` (with `.project`) or a
  `Glidex::Project`;
- `context.auth`: `method` (`peer`, `pam`, `oidc`, `token`),
  `transport` (`unix`, `tcp`), `age_secs` since login, `mfa`, and
  `source_ip` for TCP clients;
- `context.project`, `context.tenant` where the action has one.

Every policy is validated against the schema before it is accepted;
the validation errors name what is wrong. The **Directory** page's
**Policies** tab in the web UI lists the base policies, role definitions and site policies with
their full text, as a reference.
<!-- TODO(verify): no public list of Cedar action names yet; link to the API reference once it lists them. -->

### Applying it

Site policies need `system-admin` and a recent login. From the web UI's
**Policies** tab (**Directory** page): **+ New site policy**, write it, **Validate**, then
**Simulate** to compare decisions for sample requests under the current
policies and with your draft, then **Save**. With `gxctl`:

```bash
gxctl policy validate site.console-needs-mfa console.cedar
gxctl policy put site.console-needs-mfa console.cedar --description "MFA for consoles"
gxctl policy list
gxctl policy show site.console-needs-mfa
gxctl policy put site.console-needs-mfa console.cedar --disable   # keep, but not in force
gxctl policy history site.console-needs-mfa
gxctl policy delete site.console-needs-mfa
```

- A change applies at once, on every server of a cluster.
- Each save is a new version; `authz.policy_history` (50) versions of
  each are kept.
- There can be at most 200 site policies, each at most 64 KiB.

### `would_lock_out`

Glidex refuses a policy change that would leave you unable to manage
policies, with `409 would_lock_out` ("after this change you could no
longer manage policies"). Fix the policy so it doesn't forbid you, for
example by excluding a team of administrators, and try again.

If you do lock yourself, or everyone, out, a break-glass administrator
on the host's socket can still undo it; site policies don't apply to
them:

```bash
sudo gxctl policy delete site.too-broad
```

### Policy files

Site policies can also be files, for sites that manage configuration
with their own tools. Put `*.cedar` files in
`authz.policy_files_dir` (`/etc/glidex/policies`, `root:glidex`, mode
0750). A file may hold several policies; each needs its own
`@id("site.…")`, and an id can't be used by a file and the API at the
same time. File policies show in `gxctl policy list` and the web UI as
read-only.

The control plane reads the files at start. Check a policy before you
put it there (`validate` takes one policy at a time), then reload after
changing the directory:

```bash
gxctl policy validate site.lab-rules lab-rules.cedar
sudo install -m 0640 -o root -g glidex lab-rules.cedar /etc/glidex/policies/
gxctl policy reload
```

If a file doesn't parse or validate, `reload` fails and the policies in
force stay as they were; at start, the control plane stops with the
error.

## Audit log

Glidex records every write, every login and every denied request: who,
from where, the action, the target, the result and the policies that
decided it. Read it from the web UI's **Audit** page or:

```bash
gxctl audit --limit 50
gxctl audit --project web
gxctl audit --user <user-id> --since 1767225600000   # Unix milliseconds
```

`auditor` and `system-admin` read all of it; a project's owners read
their project's. Entries older than `audit.retention_days` (90) are
deleted. Each entry is also written to the control plane's journal:

```bash
journalctl -u glidex-control-plane | grep glidex_audit
```

Ship the journal to your log system if you need the trail kept longer
or out of the host's reach.

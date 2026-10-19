# Projects

A project is where your work lives. Every VM, disk, guest credential and
project network belongs to exactly one project; who may do what is
granted per project; quotas cap what a project may use.

Images are different: they are a library shared by every project (see
[Images and disks](images-and-disks.md)).

Every installation has a project named `default`, created at install. It
can't be deleted. The members of the `glidex-admin` group at the control
plane's first start are made its owners.

## Seeing your projects

**Web UI:** **Projects** in the header lists the projects you are a
member of, with each one's use against its quotas. Click a project for
its page: quotas, members, networks.

**gxctl:**

```bash
gxctl project list
gxctl project show lab
```

If you are in no project yet, the list is empty: ask a project owner or
an administrator to add you.

## Choosing a project

Most commands work in one project at a time: lists show its VMs and
disks, and new resources are created in it.

**Web UI:** the **Project** selector in the header. Every page then shows
that project's resources. On a project's page, **Select this project**
does the same. Your choice is also saved as your default project.

**gxctl:** a project is chosen, in this order, by

1. `--project <name|id>` (or `-p`, or `GLIDEX_PROJECT`) on the command:

   ```bash
   gxctl --project lab list
   ```

2. a `"project"` key in the login profile in use;
3. your default project, kept by the control plane and shared with the
   web UI:

   ```bash
   gxctl project use lab     # set it
   gxctl project use -       # clear it
   ```

4. if none is set and you belong to exactly one project, that project.

`gxctl whoami` shows your default project and the one this session uses.
Over the API, pass `?project=<name|id>` or a `"project"` field.

## Roles

Each member of a project has one of four roles. Each role includes
everything of the roles above it.

| Role | Can |
|------|-----|
| `viewer` | See the project, its VMs, disks and usage |
| `operator` | Also start, stop, pause and change VMs, and open their consoles |
| `editor` | Also create and delete VMs and disks, attach disks and networks to VMs, and see and manage guest credentials |
| `owner` | Also manage members and service-account tokens, read the project's audit log, and create and share project networks |

Viewers and operators don't see guest credentials.

Host-wide roles (for example `image-admin` for pulling images, or
`net-admin` for host networking) and how administrators assign them are
described in [Access control](../operator-guide/access-control.md).

## Members

A member is a user or a team bound to a role in the project. Owners
manage members.

**Web UI:** the project's page, **Members** card. The table lists each
binding; **Remove** deletes one. To add one, choose a **Role**, a
**Principal type** (User or Team) and the user or team, then **Add role**.
If you can't list users, type the user's id (they can find it with
`gxctl whoami`).

**gxctl:**

```bash
gxctl binding list lab
gxctl binding add lab editor user:<user-id>
gxctl binding add lab viewer team:<team-id>
gxctl binding remove lab <binding-id>
```

Roles can be written short (`editor`) or in full (`role.editor`). A
principal is `user:<id>`, `team:<id>` or `token:<id>`. `binding list`
shows each binding's id.

Over the API: `GET`/`POST /projects/{id}/bindings` and
`DELETE /projects/{id}/bindings/{binding-id}`.

## Teams

A team is a named group of users; binding a team to a project gives every
member of it that role. Administrators create teams and manage their
members (the **Directory** page's **Users & roles** tab in the web UI,
`gxctl team …`):

```bash
gxctl team list
gxctl team create ops
gxctl team add-member ops <user-id>
gxctl team remove-member ops <user-id>
```

Teams can also come from outside Glidex:

- on the local socket, each of your Unix groups counts as a team named
  `unix:<group>`, so `team:unix:lab-users` grants a role to that group's
  members when they use `gxctl` on the host;
- the operator can map groups from the site's PAM or single sign-on setup
  to teams.

`gxctl whoami` lists the teams you are in.

## Quotas

A project's quotas cap what it may hold. Each is a number or unlimited:

| Quota | Counts |
|-------|--------|
| `vms` | VMs |
| `running_vms` | VMs running at once |
| `vcpus` | vCPUs of all VMs |
| `memory_mib` | Memory of all VMs, in MiB |
| `disk_gib` | Disk space, in GiB |
| `networks` | Project networks (2 by default) |
| `routers` | Project routers (1 by default) |
| `external_ips` | Outside addresses of routers (1 by default) |

A request that would go over a quota is refused with the limit, what is
in use and what was asked for. Administrators may go over a quota; that
is recorded in the audit log. New projects get the site's default
quotas: unless the operator changed them, only the last three are
limited.

**Web UI:** the **Projects** list and the project's **Quotas and usage**
card show use against each limit. **Edit quotas** (shown if you may
change them) takes a number per quota; an empty field is unlimited.

**gxctl:**

```bash
gxctl project quota lab                       # show use/limit
gxctl project quota lab vms=10 memory_mib=none
```

`none` (or `unlimited`) removes a limit; keys you don't name keep their
value. `gxctl` sets the first six quotas in the table; `routers` and
`external_ips` are set in the web UI or the API.

Changing quotas is an administrator's task, as is creating and deleting
projects.

## Creating and deleting projects

If you may create projects, **Projects** shows **+ Create Project**:
give a **Name** (1-32 lowercase letters, digits and dashes), an optional
**Description** and, optionally, an **Owner**. Or:

```bash
gxctl project create lab --description "Lab VMs"
```

`gxctl project create` adds no owner; add one with `binding add`.

To delete a project it must not own any VMs, disks or credentials:
**Delete project** on its page, or `gxctl project delete lab` (it asks
for confirmation). Its service-account tokens are revoked with it.

On an installation with several tenants (organisations), each project
belongs to one tenant, shown in the **Tenant** column. See
[Access control](../operator-guide/access-control.md).

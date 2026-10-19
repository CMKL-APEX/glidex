# User guide

This guide is for people who run VMs on a Glidex host or cluster that
someone else installed: logging in, working inside a project, and
creating, connecting to and removing VMs and what they use.

If you are setting Glidex up, start with the
[Quick start](../quick-start.md) and the
[Operator guide](../operator-guide/README.md). The
[Concepts](../concepts.md) page explains the words used here: projects,
images, disks, credentials, networks.

## Three ways in

Everything in this guide can be done in at least one of these:

- **Web UI**, at `https://<host>:5173` by default. Sign in with your
  account; the header has a project selector and a page per resource
  (VMs, Images, Disks, Credentials, Networking, Projects, Usage). Pages
  follow changes as they happen, without reloading.
- **`gxctl`**, the command-line client. Run `gxctl` for an interactive
  shell (`gxctl>` prompt, Tab completion, `help` lists every command), or
  `gxctl <command>` to run one command and exit:

  ```bash
  gxctl list                  # one command, then exit
  gxctl                       # the shell
  ```

  On the host itself it uses the local socket and needs no login; from
  another machine it logs in once and keeps a token.
- **The API**, REST over HTTPS on port 8841 by default, plus an RPC API,
  authenticated with a bearer token. The web UI and `gxctl` use it too,
  so anything they do can be scripted. See the [REST API](../api/rest.md)
  and [RPC API](../api/rpc.md) references.

Each page shows the web UI path and the `gxctl` commands, with a short
API example where it helps. The full command list is in the
[gxctl reference](../reference/gxctl.md).

## Pages

- [Logging in](logging-in.md): the web UI, invitations, your sign-in
  methods, `gxctl` on the host and from another machine, login profiles,
  API tokens, `whoami`.
- [Projects](projects.md): what a project holds, members and roles,
  teams, quotas, choosing the project you work in.
- [Images and disks](images-and-disks.md): the image catalog, pulling
  images and firmware, creating, attaching, resizing and deleting disks.
- [Guest credentials](credentials.md): the login cloud-init creates in a
  VM, passwords and SSH keys, custom cloud-init seeds.
- [Virtual machines](vms.md): create, start, stop, resize, delete.
- [Networks](networks.md): attaching VMs to networks, project networks
  and sharing.
- [The console](console.md): the serial console in the browser and in
  `gxctl`.
- [Usage](usage.md): resource usage, bandwidth and metering reports.

## What you can do depends on your role

Glidex checks every request against your roles. Within a project you are
a viewer, operator, editor or owner (see [Roles](projects.md#roles)); a
few things, such as pulling images or changing host networking, need a
host-wide role that administrators hold. The web UI hides or disables
what you can't do; `gxctl` and the API answer `not allowed: <action>`,
naming what was refused. If you need more access, ask a project owner or
an administrator.

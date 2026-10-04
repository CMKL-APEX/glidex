# Web UI

Two crates cooperate to deliver the UI:

- `crates/glidex-ui` (Rust bin) — serves the production build
  (`bun run build` → `ui/dist`, or `GLIDEX_UI_DIR`) on
  `GLIDEX_UI_LISTEN` (default `127.0.0.1:5173`), with unknown paths
  answered by `index.html` (client-side routes), and proxies `/api/*`
  to the control plane, WebSocket upgrades included (see *Server*
  below). `glidex-ui --dev` instead runs `bun run dev` inside
  `crates/glidex-ui/ui/` (Vite HMR). No rendering happens here.
- `crates/glidex-ui/ui` — the actual Vite + React + TypeScript app.

The installer builds the UI, copies `dist` to
`/usr/local/share/glidex/ui` and runs `glidex-ui.service` as the
`glidex` user (see [installer.md](installer.md)).

## Server

`glidex-ui` environment (spec/security.md §5.6):

| Variable | Default | Meaning |
|---|---|---|
| `GLIDEX_UI_DIR` | `crates/glidex-ui/ui/dist` | The built UI. |
| `GLIDEX_UI_LISTEN` | `127.0.0.1:5173` | Listen address. A non-loopback address without TLS is refused at startup. |
| `GLIDEX_UI_HOSTS` | `localhost,127.0.0.1,[::1]` (any port) | Allowed `Host` values, `host[:port]` comma-separated (a port pins it). Anything else gets `421 Misdirected Request`, static files and `/api` alike (DNS rebinding). |
| `GLIDEX_UI_TLS_CERT`, `GLIDEX_UI_TLS_KEY` | unset | PEM certificate chain and key: serve HTTPS (rustls with `ring`, HTTP/1.1). The key may instead be the systemd credential `ui-tls-key` (`LoadCredential=`). |
| `GLIDEX_API_SOCKET` | `/run/glidex-cp/ui.sock` | The control plane's UI socket, used when it exists. It only accepts the `glidex-ui` user. |
| `GLIDEX_API_URL` | `http://127.0.0.1:8841` | The control plane over TCP when there is no socket. Setting it without `GLIDEX_API_SOCKET` skips the default socket (development against a scratch control plane; also the packaged unit until it runs as `glidex-ui`). |

On every proxied request it replaces (never appends to) `X-Forwarded-For`
(the TCP peer), `X-Forwarded-Proto` (`https` under TLS) and
`X-Forwarded-Host` (the request's `Host`), and drops `Forwarded`. The
control plane trusts these only from the `glidex-ui` peer on `ui.sock`;
over TCP it goes by the browser's session cookie alone. `Origin`,
`Cookie`, `X-Glidex-CSRF` and `Authorization` pass through untouched.

Every response carries `Content-Security-Policy: default-src 'self';
connect-src 'self'; img-src 'self' data:; style-src 'self'
'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action
'self'` (inline styles: React and xterm.js set `style` attributes),
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
`X-Frame-Options: DENY`, and under TLS `Strict-Transport-Security:
max-age=31536000`.

## Authentication flow

1. `SessionProvider` (`session.tsx`) calls `GET /auth/methods` and
   `GET /auth/whoami`. A `401` shows the login page instead of the app.
2. The login page offers what `methods` says: a PAM form
   (`POST /auth/login {method: "pam", username, password}`) and/or
   **Sign in with SSO** (`location = /api/auth/oidc/start?return_to=<path>`;
   the control plane redirects back with the session cookie).
3. The `gx_session` cookie is `HttpOnly; SameSite=Strict`; the page never
   sees it. The client keeps the session's `csrf` value (from `whoami` or
   the login response) and sends it as `X-Glidex-CSRF` on every non-GET.
4. `401 unauthenticated` from any call returns to the login page.
   `401 reauth_required` (step-up, e.g. saving a policy) opens a
   re-login dialog (PAM, or SSO with `reauth=1`) and retries the request
   once. `403` messages are shown where the action was attempted.
5. **Log out** in the header: `POST /auth/logout`.
6. The header's project selector lists `GET /projects`; the choice is
   saved with `PATCH /users/me {"default_project": id}`. Lists of VMs,
   disks and credentials pass `?project=`, creates pass `project`, the
   VM form only offers networks the project may use, and the pages
   remount when the project changes.
7. Buttons and pages the caller can't use are hidden after
   `POST /authz/check {checks: [{action, resource: {type, id}}]}`
   (e.g. `createVm` on the Project; `createProject`, `listUsers`,
   `manageSystemBindings`, `readPolicy`, `writePolicy`, `readAudit` on
   `{type: "Host"}`). The server still checks every request.

## Stack

- **Vite 6** dev server on `:5173`.
- **React 19** with **react-router-dom 7** for client-side routing.
- **TypeScript 5** (strict).
- **Tailwind CSS 3** for styling; config in `ui/tailwind.config.js`.
- **@xterm/xterm + @xterm/addon-fit** for the VM console page.
- **bun** is the package manager (the lockfile is `bun.lock`).

## Dev-server proxy

`ui/vite.config.ts` proxies everything under `/api` to the control
plane on `:8841` (or `GLIDEX_API_URL`), stripping the `/api` prefix. WebSocket upgrades
are forwarded (`ws: true`) so `/api/vms/:id/console/ws` resolves to
`ws://localhost:8841/vms/:id/console/ws`.

Consequence: the frontend never has to know the server URL.
Everything is same-origin from the browser's perspective.

## Source layout

```
ui/src/
├── App.tsx                 # SessionProvider + <Routes> for the pages
├── main.tsx                # ReactDOM.createRoot entrypoint
├── api.ts                  # Typed fetch wrappers: cookies, CSRF, 401 handling
├── session.tsx             # whoami, project selection, host capabilities, useCan()
├── live.tsx                # LiveProvider: one EventSource on /api/watch; useLive(), useLiveRefresh()
├── hooks.ts                # useDirectory(): users and teams when listable
├── types.ts                # API types and helpers
├── index.css               # Tailwind entry
├── components/
│   ├── Header.tsx          # Nav (by capability), activity, project selector, user, Log out, API health
│   ├── Footer.tsx          # Build info: version tag (if any), git branch, commit
│   ├── Activity.tsx        # "N in progress" indicator and list; Spinner
│   ├── ReauthDialog.tsx    # Step-up re-login, then retry
│   ├── AddBindingForm.tsx  # Role + user/team picker for role links
│   ├── ui.tsx              # Shared bits: cards, badges, binding table
│   ├── Loading.tsx
│   ├── Modal.tsx
│   ├── CreateVmForm.tsx    # POST /vms form (boot mode, firmware image, boot disk source, credential picker, restart behaviour, start now)
│   ├── VmActions.tsx       # Start/Shut down/Stop/Pause/Delete buttons (Shut down: power button, 60 s)
│   ├── VmStateBadge.tsx    # State pill (→ desired) plus what the controller is still doing
│   └── VmCard.tsx          # Dashboard VM row: state badge, Ready reason
└── pages/
    ├── Dashboard.tsx       # List VMs, open create modal; follows the live stream (polls without it)
    ├── VmDetail.tsx        # VM details, Ready reason, restart notice, actions, events, Open Console link
    ├── VmConsole.tsx       # xterm.js + console WebSocket; Reconnect after it closes
    ├── Credentials.tsx     # List / add / edit / delete guest logins
    ├── Images.tsx          # Cloud image and firmware catalogs (Pull), downloaded images and firmware with progress
    ├── Disks.tsx           # Disks: create, resize, extend root, delete, partitions
    ├── Networking.tsx      # OVS status, bridges and uplinks, networks (networking.md §12)
    ├── Login.tsx           # PAM form and/or Sign in with SSO
    ├── Projects.tsx        # Projects with usage / quotas; create (system-admin)
    ├── ProjectDetail.tsx   # Members, quotas, project networks, shares
    ├── Access.tsx          # Users, teams, system roles (admins)
    ├── Tokens.tsx          # Access tokens: create (secret shown once), revoke
    ├── Policies.tsx        # Cedar policies; site-policy editor, validate, simulate, history
    ├── Audit.tsx           # Audit log with project / user / since filters
    └── NotFound.tsx
```

## Routes

| Path | Component |
|---|---|
| `/` | `Dashboard` |
| `/vms/:id` | `VmDetail` |
| `/vms/:id/console` | `VmConsole` |
| `/credentials` | `Credentials` |
| `/images` | `Images` |
| `/disks` | `Disks` |
| `/projects` | `Projects` |
| `/projects/:id` | `ProjectDetail` |
| `/access` | `Access` (shown with `listUsers`, `listTeams` or `readSystemBindings`) |
| `/tokens` | `Tokens` |
| `/policies` | `Policies` (shown with `readPolicy`) |
| `/audit` | `Audit` (shown with `readAudit`, or to project owners for their projects) |
| `*` | `NotFound` |

Without a session every path shows the login page.

## API client

`ui/src/api.ts` sends every call through one `request()` helper:
`credentials: "same-origin"`, `X-Glidex-CSRF` on non-GET requests, and
the 401 handling above. Error bodies become `ApiRequestError`s
(`status`, `code`, `details`, message `"<error>: <message>"`); non-JSON
errors (a proxy's 502, a 421) are wrapped too. The base URL is
hard-coded `/api` — the Vite proxy or glidex-ui forwards it.

## VM state display

VMs have a desired state and an observed one
([reconciliation.md §7.4](reconciliation.md#74-defaults-and-api-projection)).
`types.ts` mirrors that: `VmState` is `created | starting | running |
paused | stopping | stopped | failed | unknown`, `PowerState` is
`running | paused | stopped`, and `VmResponse` carries `desired_state`,
`generation`, `observed_generation`, `restart_required`, `conditions`
and `last_exit`.

- **`vmActivity(vm)`**: what the controller is still doing, or `null`
  once converged (`settled(vm)`): `Deleting` (`deleting`), `Starting`,
  `Stopping`, `Pausing` or `Resuming` while the VM isn't where
  `desired_state` says (`created` counts as `stopped`), `Applying
  changes` while `observed_generation < generation`, and `Reconciling`
  while `Ready` isn't true. `VmStateBadge` shows the state pill
  (`<state> → <desired>` while moving) with a spinner and that text;
  VmDetail adds an "In progress" box, and `notReadyReason(vm)` (`Ready`
  reason and message) is shown under it. VmDetail also shows the restart
  policies and the last exit (`describeExit`).
- **Refreshing.** Dashboard and VmDetail re-fetch when a VM changes on
  the live stream (below). Without the stream they re-fetch every 2 s
  while any VM shown is not settled, and stop once everything is. A lifecycle call
  waits for the controller (`?wait`), so the page also re-fetches 0.5 s
  after sending it to show the progress meanwhile. A VM that disappears
  while its page is open (deleted) says so instead of an error.
- **Actions** (`VmActions`): Start is offered for `created`, `stopped`,
  `paused`, and `failed` unless the VM is already meant to run; a VM
  being deleted shows no actions. Delete asks for confirmation (a root
  disk created with the VM goes with it). Stop is
  offered while running or paused **or** whenever the desired state is
  not `stopped`, so a VM stuck starting or crash-looping can be stopped.
  `api.ts` sends `?wait=60` with start, pause, stop (at least the grace
  plus 20 s, at most 300) and delete; a failed reconcile comes back as
  an `ApiRequestError` like any other.
- **VmDetail** also shows "Configuration changes take effect at the next
  start." while `restart_required`, and an **Events** list (newest
  first, warnings highlighted) from `GET /vms/{id}/events`, refreshed
  whenever the VM's state or observed generation changes.

## Create VM form

Besides the config, `CreateVmForm` sets the VM's policies
([reconciliation.md §7.4](reconciliation.md#74-defaults-and-api-projection)):
**If It Crashes** (`restart_policy`: restart with backoff, the default,
or leave it stopped), **After a Host Reboot** (`on_host_boot`: start it
again if it was running, the default, or leave it stopped), and **Start
it now** (`power: "running"`; unchecked, the VM is created stopped).
`onSubmit` returns a promise: a refused create keeps the form open,
with what was typed and the error under it.

## Live stream

`LiveProvider` (`src/live.tsx`, around the routes in `App.tsx`) opens one
`EventSource` on `/api/watch?project=<selected project>`
([rest-api.md](rest-api.md#live-stream-get-watch)) for the whole app and
keeps the latest copy of every VM, disk, image and network the user can
see (`useLive()`); it is live once the snapshot has arrived (`synced`).
When the server ends the stream (`expired`) or it errors, EventSource
reconnects and the next snapshot replaces the maps.
`useLiveRefresh(kinds, refresh)` re-runs a page's refresh (debounced
300 ms) when an object of those kinds changes, and returns whether the
stream is live. Dashboard and VmDetail follow `vm`, Disks `disk` and
`vm`, Images `image`, `disk` and `vm` (firmware users), Networking `network`; each polls as
before only while the stream isn't live (no EventSource, an old control
plane, a buffering proxy, `503 too_many_watchers`).

## Reconciliation activity

Writes are carried out by the controllers after the API answers
([reconciliation.md](reconciliation.md) D5), so the header's
`Activity` shows whether any are still at work in the selected project:
"Up to date", or a spinner with "N in progress" that opens a list (kind,
name, what, linked to the object's page). It is built with `vmActivity`,
`diskActivity` (`Waiting for its image`, `Creating`, `Resizing`,
`Busy (op)`, `Deleting`, or a resize waiting for its VM to stop),
`imageActivity` (deleting, downloading, verifying) and `networkActivity`
(`Deleting`, or `Deleting (<Ready reason>)` such as
`Deleting (NetdUnavailable)`). While the live stream is open it is
computed from the stream's maps. Otherwise it polls `GET /vms`,
`/disks`, `/images` and `/networks` every 3 s while something is in
progress and every 15 s otherwise; a disk resize that only waits for
its VM to stop, and a network deletion waiting on something (netd, say),
count as idle for that. `api.ts` dispatches `glidex:changed` on
`window` after every successful write to `/vms`, `/disks`, `/images` or
`/networks`, and the polling indicator looks again half a second later.

## Access-control pages

- **Projects** — usage against each quota (`null` = unlimited); a
  system-admin creates projects (optionally with an owner). The project
  page shows its id (share offers name projects by id), quotas (editable
  with `updateProject`), members (`GET/POST/DELETE /projects/{id}/bindings`,
  roles viewer / operator / editor / owner = `role.*`; principals picked
  from the user and team lists when the caller may list them, else typed
  as ids), project NAT networks (`POST /projects/{id}/networks`, delete,
  offer to a project id, unshare / withdraw) and networks shared with the
  project (`GET /projects/{id}/network-shares`, accept, leave).
- **Access** — users (identities, default project, disable / enable),
  teams (create, delete, add / remove manual members; `pam` / `oidc`
  memberships are shown but managed by the login sync), and system roles
  (`/system/bindings`: auditor, image-admin, net-admin, system-admin,
  `grant.host-paths`).
- **Tokens** — the caller's tokens and the service accounts it may
  manage. Create (name, expiry, optional narrowing roles; service accounts
  of the selected project with `manageServiceTokens`) shows the secret
  once, in a copy box with a warning; revoke.
- **Policies** — every loaded policy with its source badge (base, role,
  link, site, file) plus disabled site policies; text view. With
  `writePolicy`, site policies open in a monospace editor: **Validate**
  (`POST /authz/validate`), **Simulate** (`POST /authz/simulate` with the
  draft as the candidate change and requests built from principal,
  action and resource; current vs. candidate decision and determining
  policies), **Save** (`PUT /authz/policies/{id}` with the current
  version: `409 conflict`, `409 would_lock_out` and `422` validator
  messages are shown), **Delete** (`?version=`), and **History**
  (`/versions`, loadable into the editor).
- **Audit** — `GET /audit` newest first, filtered by project, user and
  since; a row expands to request id, determining policies and details.

## Credentials page

`pages/Credentials.tsx` lists `CredentialInfo` rows and opens `Modal`
forms to add or edit. Passwords use `type="password"` inputs with a
confirmation field and are sent once, in the create/update request; the
UI never receives a hash, only `has_password`. SSH keys are edited as a
textarea, one key per line. `CreateVmForm` shows a credential `<select>`
only for Cloud-Hypervisor firmware boot, fed by `GET /credentials`.

## Images and disks pages

`pages/Images.tsx` shows two catalogs, each entry with a Pull button:
cloud images (`GET /images/catalog`) and UEFI firmware
(`GET /images/firmware-catalog`, [images.md](images.md#41-firmware-catalog);
an entry copied from a host package says Import (Imported once done),
and is disabled with the package to install when it isn't). Below, downloaded images in two
tables: cloud images (with their linked disks) and firmware (with its
hypervisor and the VMs booting through it; Delete is disabled while any
do). Each has a progress bar while downloading; an image being deleted
shows "Deleting…". It follows the live stream, or without
it polls `GET /images` every 1.5 s while anything is downloading,
verifying or being deleted. Delete doesn't wait (`204`, or `202` while
the controller finishes); the page shows the progress.
A failed image has **Retry** (`POST /images/{id}/retry`); the page
looks again a second later, when the controller has restarted it.
"Download from URL" opens a form for `{url, sha256?, name?}`, with a
type (cloud image, or UEFI firmware for a chosen hypervisor: `kind:
"firmware"`, `hypervisor`).
`pages/Networking.tsx`'s Status column shows a network's phase, or
"deleting" (with the `Ready` message, reason on hover) while a deletion waits (netd
unreachable, a VM still on it); like image deletes, network deletes
don't wait (`204` or `202`). Without the live stream the page polls
every 3 s while a network is being deleted.
`pages/Disks.tsx` lists disks with Resize / Extend root / Delete actions
(Delete is disabled while the disk is attached), and clicking a name shows
its partition table (`GET /disks/{id}`). A refused shrink shows
`details.min_size_bytes`. `CreateVmForm`'s firmware-boot "Boot Disk"
select offers a new disk from a ready image (with a root size), an
unattached existing disk, or a file path. It defaults to the image option
when one is ready. Its "UEFI Firmware" select lists the ready firmware
images built for the chosen hypervisor, newest first (preselected, the
server's default too), and follows a hypervisor change; with none it
names the catalog entry to pull and the form won't submit. It sends
`firmware` (the image id), never a host `firmware_path`. `VmDetail`
shows the VM's firmware image by name (`GET /images/{id}`), and the
Disks page's create form never offers a firmware image as contents.

## Footer

`components/Footer.tsx`, under every signed-in page, shows the build the
UI came from: the version tag on the built commit (if any), the git
branch and the commit (marked "(modified)" when built from a tree with
uncommitted changes). `vite.config.ts` reads them from git at build (or
dev-server start) time into `__GLIDEX_BUILD__`; `GLIDEX_BUILD_TAG`,
`GLIDEX_BUILD_BRANCH` and `GLIDEX_BUILD_COMMIT` override them for builds
without a checkout (CI also supplies `GITHUB_REF_NAME`). A detached HEAD
shows no branch.

## `VmConsole` contract

The console page is the non-obvious component. It:

1. Creates an `xterm` `Terminal` and fits it to a ref'd container.
2. Gets a single-use ticket (`POST /api/vms/:id/console/ticket`, valid
   30 s), then opens
   `ws(s)://<location.host>/api/vms/:id/console/ws?ticket=<ticket>` with
   `binaryType = "arraybuffer"`. A failed ticket request is shown as the
   page's error.
3. Maps:
   - **WS → term**: `message` event → `term.write(Uint8Array)` for
     binary frames, `term.write(string)` for text frames (used by
     the server to surface a connect failure).
   - **term → WS**: `term.onData(d => ws.send(TextEncoder.encode(d)))`.
4. Tracks a `Status` enum (`connecting | connected | closed | error`)
   for the status pill shown in the page header.
5. On unmount, disposes the input handler, detaches the socket's
   handlers and closes it, takes the terminal off the page, and disposes
   it on the next tick. xterm 5.5's `open()` queues a timer that reads
   the renderer; disposing before it runs (React StrictMode mounts twice
   in dev) throws "reading 'dimensions'". `e2e/tests/console.spec.ts`
   covers this.

See [console.md](console.md) for how the server side of that
WebSocket is implemented.

## End-to-end tests

`crates/glidex-ui/e2e/` is a Playwright suite that drives this UI against
a scratch control plane (authentication on), for both hypervisors: login
and session handling, CSRF, console tickets, glidex-ui's Host check and
headers, the access-control pages, the reconciliation activity indicator, the
create form, a full VM lifecycle
on a real guest (console login, pause/resume, Shut down) and the console
page's teardown. See its [README](../crates/glidex-ui/e2e/README.md).

## State management

There is none beyond React's built-in hooks. Lists are fetched on
mount and refreshed after mutations by re-calling the list
endpoint, and again when the live stream reports a change (above);
without the stream, pages and the activity indicator poll while
something converges. The stream's maps (`LiveProvider`) are the only
shared state: no global store, no query cache. If that becomes painful
(polling, optimistic updates, cross-component invalidation) a
lightweight option like TanStack Query is the natural upgrade.

## Styling conventions

Tailwind utility classes inline in JSX. No CSS-in-JS, no CSS
modules. Page-level containers follow a common pattern
(`max-w-* mx-auto p-* bg-white rounded-xl shadow-md border`) but
there's no reusable "page shell" component — each page wires its
own layout.

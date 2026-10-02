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
├── hooks.ts                # useDirectory(): users and teams when listable
├── types.ts                # API types and helpers
├── index.css               # Tailwind entry
├── components/
│   ├── Header.tsx          # Nav (by capability), project selector, user, Log out, API health
│   ├── ReauthDialog.tsx    # Step-up re-login, then retry
│   ├── AddBindingForm.tsx  # Role + user/team picker for role links
│   ├── ui.tsx              # Shared bits: cards, badges, binding table
│   ├── Loading.tsx
│   ├── Modal.tsx
│   ├── CreateVmForm.tsx    # POST /vms form (boot mode, boot disk source, credential picker)
│   ├── VmActions.tsx       # Start/Shut down/Stop/Pause/Delete buttons (Shut down: power button, 60 s)
│   └── VmCard.tsx          # Dashboard VM row
└── pages/
    ├── Dashboard.tsx       # List VMs, open create modal
    ├── VmDetail.tsx        # VM details, actions, Open Console link
    ├── VmConsole.tsx       # xterm.js + console WebSocket
    ├── Credentials.tsx     # List / add / edit / delete guest logins
    ├── Images.tsx          # Catalog (Pull), downloaded images with progress
    ├── Disks.tsx           # Disks: create, resize, extend root, delete, partitions
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

`pages/Images.tsx` shows the catalog (`GET /images/catalog`) with a Pull
button per entry, and downloaded images with a progress bar. It polls
`GET /images` every 1.5 s while anything is downloading or verifying.
"Download from URL" opens a form for `{url, sha256?, name?}`.
`pages/Disks.tsx` lists disks with Resize / Extend root / Delete actions
(Delete is disabled while the disk is attached), and clicking a name shows
its partition table (`GET /disks/{id}`). A refused shrink shows
`details.min_size_bytes`. `CreateVmForm`'s firmware-boot "Boot Disk"
select offers a new disk from a ready image (with a root size), an
unattached existing disk, or a file path. It defaults to the image option
when one is ready.

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
headers, the access-control pages, the create form, a full VM lifecycle
on a real guest (console login, pause/resume, Shut down) and the console
page's teardown. See its [README](../crates/glidex-ui/e2e/README.md).

## State management

There is none beyond React's built-in hooks. Lists are fetched on
mount and refreshed after mutations by re-calling the list
endpoint. No global store, no query cache. If that becomes painful
(polling, optimistic updates, cross-component invalidation) a
lightweight option like TanStack Query is the natural upgrade.

## Styling conventions

Tailwind utility classes inline in JSX. No CSS-in-JS, no CSS
modules. Page-level containers follow a common pattern
(`max-w-* mx-auto p-* bg-white rounded-xl shadow-md border`) but
there's no reusable "page shell" component — each page wires its
own layout.

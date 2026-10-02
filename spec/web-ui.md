# Web UI

Two crates cooperate to deliver the UI:

- `crates/glidex-ui` (Rust bin) — serves the production build
  (`bun run build` → `ui/dist`, or `GLIDEX_UI_DIR`) on
  `GLIDEX_UI_LISTEN` (default `127.0.0.1:5173`), with unknown paths
  answered by `index.html` (client-side routes), and proxies `/api/*`
  to `GLIDEX_API_URL` (default `http://127.0.0.1:8841`), WebSocket
  upgrades included. `glidex-ui --dev` instead runs `bun run dev`
  inside `crates/glidex-ui/ui/` (Vite HMR). No rendering happens here.
- `crates/glidex-ui/ui` — the actual Vite + React + TypeScript app.

The installer builds the UI, copies `dist` to
`/usr/local/share/glidex/ui` and runs `glidex-ui.service` as the
`glidex` user (see [installer.md](installer.md)).

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
├── App.tsx                 # <Routes> for the pages
├── main.tsx                # ReactDOM.createRoot entrypoint
├── api.ts                  # Typed fetch wrappers around the REST API
├── types.ts                # VmResponse / CreateVmRequest / helpers
├── index.css               # Tailwind entry
├── components/
│   ├── Header.tsx          # Brand, VMs / Images / Disks / Credentials / Networking nav, API health
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
| `*` | `NotFound` |

## API client

`ui/src/api.ts` wraps `fetch` with a single `handleResponse` helper
that parses `ApiError` bodies into thrown `Error`s formatted as
`"<error>: <message>"`. The base URL is hard-coded `/api` — the
Vite proxy handles forwarding in dev.

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
2. Opens `ws(s)://<location.host>/api/vms/:id/console/ws` with
   `binaryType = "arraybuffer"`.
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
a scratch control plane, for both hypervisors: the create form, a full VM
lifecycle on a real guest (console login, pause/resume, Shut down) and
the console page's teardown. See its [README](../crates/glidex-ui/e2e/README.md).

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

# Web UI end-to-end tests

Playwright tests that drive the web UI in headless Chromium against a
**throwaway control plane**, for both hypervisors.

```bash
scripts/dev/ui-e2e.sh -i ~/images/resolute-server-cloudimg-amd64.img   # from the repo root
scripts/dev/ui-e2e.sh -H ch       # or -H qemu: the ui project plus one hypervisor
scripts/dev/ui-e2e.sh -- --headed -g "create form"   # extra Playwright arguments
```

The script installs the UI's and the suite's dependencies and
Playwright's Chromium (into `~/.cache/ms-playwright`) when missing,
refuses to start if `:8851` / `:5174` / `:5175` are taken, and passes everything
after `--` to `playwright test`. Without the script:

```bash
(cd ../ui && bun install) && bun install && bunx playwright install chromium
GLIDEX_TEST_IMAGE=~/images/resolute-server-cloudimg-amd64.img bun run test   # or test:ch / test:qemu
```

## What runs

Playwright starts three servers (`playwright.config.ts`, `webServer`):

- `scripts/start-control-plane.sh` builds and runs the branch's
  `glidex-control-plane` on `127.0.0.1:8851` (`E2E_API_PORT`) with
  `HOME=/tmp/glidex-ui-e2e` (`GLIDEX_E2E_HOME`): its own database, images,
  disks and UEFI variable stores, config (`control-plane.json`), runtime
  directory and sockets (`api.sock`, `ui.sock`), wiped at every start and
  left in place afterwards for debugging. `~/.glidex/CLOUDHV.fd` is linked
  in, so the UI's default Cloud-Hypervisor firmware path works.
  Authentication is on. The config makes the test user's primary group
  the instance's `admin_group`, so on its `api.sock` that user is the
  break-glass administrator, and allows the two UI origins below.
- the Vite dev server on `:5174` (`E2E_UI_PORT`), proxying `/api` to it
  (`GLIDEX_API_URL`).
- `scripts/start-ui-server.sh`: `bun run build`, then the production
  `glidex-ui` on `127.0.0.1:5175` (`E2E_UI_SERVER_PORT`) in front of the
  scratch control plane over TCP, for the Host allowlist, security
  headers, CSP and the console through its WebSocket proxy.

**Signing in.** The `setup` project (`tests/auth.setup.ts`) runs first:
over `api.sock` it gives the test user's glidex user `role.system-admin`
(a browser session doesn't inherit break-glass), makes `default` its
default project, and opens a session with `POST /auth/session` (the same
call `gxctl ui` makes). The cookie goes into `.auth/state.json`, the
storage state every other project starts from. The specs' `api()` helper
talks to `api.sock` too, so cleanup isn't subject to the browser user's
rights. No PAM or IdP is needed.

It never attaches to an already running control plane
(`reuseExistingServer: false`), so a real glidex on `:8841` is left alone.
It does use the host's glidex-netd and Open vSwitch: the scratch control
plane registers its own `default` network on the existing `gxbr-nat`
bridge (netd treats that as a no-op), and the boot tests create and
delete `ui-ch` / `ui-qemu` networks.

| Project | Specs | Needs |
|---|---|---|
| `setup` | `auth.setup.ts`: the browser session described above | nothing beyond the servers |
| `ui` | `console.spec.ts`: the console page opens and closes without errors (xterm teardown regression) | nothing beyond the servers |
| | `auth.spec.ts`: the login page without a session, `401` from the API, the signed-in header, the CSRF header on writes (and `403 csrf_failed` without it), logout, the console through a single-use ticket (dev server and glidex-ui) and refused without one, `421` for a foreign `Host` and the security headers from glidex-ui, the built app under its CSP | nothing beyond the servers |
| | `access.spec.ts`: projects (create, add/remove a member, quotas, delete), tokens (secret shown once, revoke), site policies (validate, simulate, save, `409` on a stale version, history, delete), the audit log | nothing beyond the servers |
| `cloudhypervisor`, `qemu` | `create-form.spec.ts`: boot mode, default firmware per hypervisor, credential and network pickers, the request the form sends | nothing beyond the servers |
| | `vm-lifecycle.spec.ts`: credential and network created on their pages, VM created with firmware boot, started (NIC address shown), console login checked against that address, pause/resume, **Shut down** (clean guest poweroff), delete | KVM, the hypervisor (and OVMF for QEMU), glidex-netd with full access, OVS, `GLIDEX_TEST_IMAGE` |

Shutting down checks the console log ends with the kernel's
`reboot: Power down` — the guest's very last output, so it also checks
that nothing printed just before the hypervisor exits is lost.

`vm-lifecycle.spec.ts` skips itself, saying what is missing, when a
prerequisite is absent. Every test fails on an uncaught page error or a
browser console error (`fixtures.ts`, `pageErrors`). Tests run one at a
time; a failure leaves a screenshot and a trace in `test-results/`
(`bunx playwright show-trace <trace.zip>`).

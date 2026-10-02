# Web UI end-to-end tests

Playwright tests that drive the web UI in headless Chromium against a
**throwaway control plane**, for both hypervisors.

```bash
cd crates/glidex-ui/ui && bun install          # the UI's own dependencies
cd ../e2e && bun install
bunx playwright install chromium               # once; into ~/.cache/ms-playwright

GLIDEX_TEST_IMAGE=~/images/resolute-server-cloudimg-amd64.img bun run test
bun run test:ch      # ui + cloudhypervisor projects only
bun run test:qemu    # ui + qemu projects only
```

## What runs

Playwright starts two servers (`playwright.config.ts`, `webServer`):

- `scripts/start-control-plane.sh` builds and runs the branch's
  `glidex-control-plane` on `127.0.0.1:8851` (`E2E_API_PORT`) with
  `HOME=/tmp/glidex-ui-e2e` (`GLIDEX_E2E_HOME`): its own database, images,
  disks and UEFI variable stores, wiped at every start and left in place
  afterwards for debugging. `~/.glidex/CLOUDHV.fd` is linked in, so the
  UI's default Cloud-Hypervisor firmware path works.
- the Vite dev server on `:5174` (`E2E_UI_PORT`), proxying `/api` to it
  (`GLIDEX_API_URL`).

It never attaches to an already running control plane
(`reuseExistingServer: false`), so a real glidex on `:8841` is left alone.
It does use the host's glidex-netd and Open vSwitch: the scratch control
plane registers its own `default` network on the existing `gxbr-nat`
bridge (netd treats that as a no-op), and the boot tests create and
delete `ui-ch` / `ui-qemu` networks.

| Project | Specs | Needs |
|---|---|---|
| `ui` | `console.spec.ts`: the console page opens and closes without errors (xterm teardown regression) | nothing beyond the servers |
| `cloudhypervisor`, `qemu` | `create-form.spec.ts`: boot mode, default firmware per hypervisor, credential and network pickers, the request the form sends | nothing beyond the servers |
| | `vm-lifecycle.spec.ts`: credential and network created on their pages, VM created with firmware boot, started (NIC address shown), console login checked against that address, pause/resume, **Shut down** (clean guest poweroff), delete | KVM, the hypervisor (and OVMF for QEMU), glidex-netd with full access, OVS, `GLIDEX_TEST_IMAGE` |

`vm-lifecycle.spec.ts` skips itself, saying what is missing, when a
prerequisite is absent. Every test fails on an uncaught page error or a
browser console error (`fixtures.ts`, `pageErrors`). Tests run one at a
time; a failure leaves a screenshot and a trace in `test-results/`
(`bunx playwright show-trace <trace.zip>`).

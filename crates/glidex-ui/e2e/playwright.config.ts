import { defineConfig } from "@playwright/test";
import { STORAGE_STATE, UI_SERVER_PORT, type Options } from "./fixtures";

const apiPort = Number(process.env.E2E_API_PORT ?? 8851);
const uiPort = Number(process.env.E2E_UI_PORT ?? 5174);

// Specs that don't depend on a hypervisor.
const uiSpecs = /(console|auth|access|credentials|activity|footer)\.spec\.ts/;

export default defineConfig<Options>({
  testDir: "./tests",
  // One scratch control plane and real VMs: run one test at a time.
  workers: 1,
  fullyParallel: false,
  timeout: 120_000,
  reporter: [["list"]],
  use: {
    baseURL: `http://localhost:${uiPort}`,
    viewport: { width: 1280, height: 1000 },
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
  },
  projects: [
    // Signs the test user in (tests/auth.setup.ts).
    { name: "setup", testMatch: /auth\.setup\.ts/ },
    { name: "ui", testMatch: uiSpecs, dependencies: ["setup"], use: { storageState: STORAGE_STATE } },
    {
      name: "cloudhypervisor",
      testIgnore: [uiSpecs, /auth\.setup\.ts/],
      dependencies: ["setup"],
      use: { hypervisor: "cloudhypervisor", storageState: STORAGE_STATE },
    },
    {
      name: "qemu",
      testIgnore: [uiSpecs, /auth\.setup\.ts/],
      dependencies: ["setup"],
      use: { hypervisor: "qemu", storageState: STORAGE_STATE },
    },
  ],
  webServer: [
    {
      command: "bash scripts/start-control-plane.sh",
      url: `http://127.0.0.1:${apiPort}/health`,
      env: { E2E_API_PORT: String(apiPort), E2E_UI_PORT: String(uiPort), E2E_UI_SERVER_PORT: String(UI_SERVER_PORT) },
      // Never attach to a running (real) control plane.
      reuseExistingServer: false,
      timeout: 600_000,
      stdout: "ignore",
      stderr: "pipe",
    },
    {
      command: `bunx vite --port ${uiPort} --strictPort`,
      cwd: "../ui",
      url: `http://localhost:${uiPort}`,
      env: { GLIDEX_API_URL: `http://127.0.0.1:${apiPort}` },
      reuseExistingServer: false,
      stdout: "ignore",
    },
    // The production server (built UI, Host allowlist, security headers).
    {
      command: "bash scripts/start-ui-server.sh",
      url: `http://localhost:${UI_SERVER_PORT}/`,
      env: { E2E_API_PORT: String(apiPort), E2E_UI_SERVER_PORT: String(UI_SERVER_PORT) },
      reuseExistingServer: false,
      timeout: 600_000,
      stdout: "ignore",
      stderr: "pipe",
    },
  ],
});

import { defineConfig } from "@playwright/test";
import type { Options } from "./fixtures";

const apiPort = Number(process.env.E2E_API_PORT ?? 8851);
const uiPort = Number(process.env.E2E_UI_PORT ?? 5174);

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
    // Pages that don't depend on a hypervisor.
    { name: "ui", testMatch: /console\.spec\.ts/ },
    { name: "cloudhypervisor", testIgnore: /console\.spec\.ts/, use: { hypervisor: "cloudhypervisor" } },
    { name: "qemu", testIgnore: /console\.spec\.ts/, use: { hypervisor: "qemu" } },
  ],
  webServer: [
    {
      command: "bash scripts/start-control-plane.sh",
      url: `http://127.0.0.1:${apiPort}/health`,
      env: { E2E_API_PORT: String(apiPort) },
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
  ],
});

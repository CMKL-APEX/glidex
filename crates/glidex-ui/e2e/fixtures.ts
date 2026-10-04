import { test as base, expect, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { request as httpRequest } from "node:http";
import { fileURLToPath } from "node:url";

export type Hypervisor = "cloudhypervisor" | "qemu";

export interface Options {
  hypervisor: Hypervisor;
  /** Console errors a test expects (e.g. a refused WebSocket). A single
   * RegExp: Playwright would read a two-element array as [value, options]. */
  ignoredConsoleErrors: RegExp | undefined;
}

/** The scratch control plane (scripts/start-control-plane.sh). */
export const API = `http://127.0.0.1:${process.env.E2E_API_PORT ?? 8851}`;
export const E2E_HOME = process.env.GLIDEX_E2E_HOME ?? "/tmp/glidex-ui-e2e";

/** The firmware catalog entry each hypervisor's VMs boot through
 * (spec/images.md §4.1). */
export const FIRMWARE_KEY: Record<Hypervisor, string> = {
  cloudhypervisor: "cloudhv-edk2",
  qemu: "ovmf",
};
export const BINARY: Record<Hypervisor, string> = {
  cloudhypervisor: "cloud-hypervisor",
  qemu: "qemu-system-x86_64",
};
/** The kernel's last line when a guest powers off cleanly. Being the very
 * last thing printed before the hypervisor exits, it also checks that the
 * console log keeps the guest's final output. */
export const CLEAN_POWEROFF = /reboot: Power down/;

export const LABEL: Record<Hypervisor, string> = {
  cloudhypervisor: "Cloud Hypervisor",
  qemu: "QEMU",
};

export const test = base.extend<Options & { pageErrors: string[] }>({
  hypervisor: ["qemu", { option: true }],
  ignoredConsoleErrors: [undefined, { option: true }],
  // Every test fails on an uncaught page error or a console error.
  pageErrors: [
    async ({ page, ignoredConsoleErrors }, use) => {
      const errors: string[] = [];
      page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
      page.on("console", (m) => {
        if (m.type() === "error" && !ignoredConsoleErrors?.test(m.text())) {
          errors.push(`console: ${m.text()}`);
        }
      });
      // Delete buttons confirm(); Playwright would dismiss the dialog.
      page.on("dialog", (d) => d.accept());
      await use(errors);
      expect(errors, "browser errors").toEqual([]);
    },
    { auto: true },
  ],
});
export { expect };

/** The input/select (or checkbox group) right after the label `text`. */
export function field(page: Page, text: string) {
  return page
    .locator(`xpath=//label[normalize-space()='${text}']/following-sibling::*[self::input or self::select or self::div][1]`)
    .first();
}

/** A VM's card on the dashboard. */
export function vmCard(page: Page, name: string) {
  return page
    .locator("div")
    .filter({ has: page.getByText(name, { exact: true }) })
    .filter({ has: page.getByRole("link", { name: "View Details" }) })
    .last();
}

/** The scratch control plane's local socket: requests on it are made as
 * the user running the tests, who is its break-glass administrator
 * (scripts/start-control-plane.sh sets `admin_group`). */
export const API_SOCKET = `${E2E_HOME}/api.sock`;
/** glidex-ui serving the built UI in front of the scratch control plane. */
export const UI_SERVER_PORT = Number(process.env.E2E_UI_SERVER_PORT ?? 5175);
/** Where the setup project leaves the browser session. */
export const STORAGE_STATE = fileURLToPath(new URL("./.auth/state.json", import.meta.url));

export interface RawResponse {
  status: number;
  headers: Record<string, string | string[] | undefined>;
  body: string;
}

/** One HTTP request over a Unix socket or TCP, with any Host header. */
export function rawRequest(
  target: { socketPath: string } | { port: number; host?: string },
  method: string,
  path: string,
  opts: { body?: unknown; headers?: Record<string, string> } = {},
): Promise<RawResponse> {
  const data = opts.body === undefined ? undefined : JSON.stringify(opts.body);
  const headers: Record<string, string> = { ...(opts.headers ?? {}) };
  if (data !== undefined) headers["content-type"] = "application/json";
  return new Promise((resolve, reject) => {
    const req = httpRequest(
      { ...("socketPath" in target ? { socketPath: target.socketPath } : { host: target.host ?? "127.0.0.1", port: target.port }), method, path, headers },
      (res) => {
        let body = "";
        res.setEncoding("utf8");
        res.on("data", (c) => (body += c));
        res.on("end", () => resolve({ status: res.statusCode ?? 0, headers: res.headers, body }));
      },
    );
    req.on("error", reject);
    if (data !== undefined) req.write(data);
    req.end();
  });
}

/** JSON (or text) from the scratch control plane's REST API, over its
 * local socket as the break-glass administrator. */
export async function api<T = any>(path: string, init?: { method?: string; body?: unknown }): Promise<T> {
  const r = await rawRequest({ socketPath: API_SOCKET }, init?.method ?? "GET", path, { body: init?.body });
  const type = String(r.headers["content-type"] ?? "");
  return (type.includes("json") ? JSON.parse(r.body) : r.body) as T;
}

export const del = (path: string) => api(path, { method: "DELETE" });

export function commandExists(cmd: string): boolean {
  try {
    execFileSync("sh", ["-c", `command -v ${cmd}`], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

/** The cloud image the boot tests copy (as for the Rust e2e tests). */
export function testImage(): string | undefined {
  const value = process.env.GLIDEX_TEST_IMAGE;
  if (!value) return undefined;
  return value.startsWith("~/") ? homedir() + value.slice(1) : value;
}

/** Why a real VM can't be booted for `hypervisor` here (empty: it can). */
export async function bootPrerequisites(hypervisor: Hypervisor): Promise<string[]> {
  const missing: string[] = [];
  const image = testImage();
  if (!image) missing.push("GLIDEX_TEST_IMAGE is not set");
  else if (!existsSync(image)) missing.push(`${image} does not exist`);
  if (!existsSync("/dev/kvm")) missing.push("/dev/kvm is missing");
  if (!commandExists(BINARY[hypervisor])) missing.push(`${BINARY[hypervisor]} is not installed`);
  try {
    await ensureFirmware(hypervisor);
  } catch (e) {
    missing.push(e instanceof Error ? e.message : String(e));
  }
  const status = await api("/ovs/status");
  if (status?.netd?.access !== "full") missing.push("glidex-netd is not usable (join the glidex group)");
  if (status?.host?.ovs_running !== true) missing.push("Open vSwitch is not running");
  return missing;
}

/** Ready firmware images for `hypervisor`, newest first: what CreateVmForm
 * offers, and the first is what it preselects. */
export async function firmwareImages(hypervisor: Hypervisor): Promise<any[]> {
  return (await api<any[]>("/images"))
    .filter((i) => i.kind === "firmware" && i.hypervisor === hypervisor && i.status.state === "ready" && !i.deleting)
    .sort((a, b) => b.created_at - a.created_at);
}

/** A ready firmware image for `hypervisor`, pulled from the firmware
 * catalog if there is none (the installer's pinned CLOUDHV.fd or the
 * host's OVMF are copied; otherwise it is downloaded). */
export async function ensureFirmware(hypervisor: Hypervisor): Promise<any> {
  const have = await firmwareImages(hypervisor);
  if (have.length > 0) return have[0];
  const key = FIRMWARE_KEY[hypervisor];
  let img = await api("/images", { method: "POST", body: { firmware: key } });
  if (!img?.id) throw new Error(`firmware ${key} can't be pulled: ${img?.message ?? JSON.stringify(img)}`);
  const deadline = Date.now() + 120_000;
  while (img.status?.state === "downloading" || img.status?.state === "verifying") {
    if (Date.now() > deadline) throw new Error(`firmware ${key} did not download in time`);
    await new Promise((r) => setTimeout(r, 1000));
    img = await api(`/images/${img.id}`);
  }
  if (img.status?.state !== "ready") throw new Error(`firmware ${key} is ${JSON.stringify(img.status)}`);
  return img;
}

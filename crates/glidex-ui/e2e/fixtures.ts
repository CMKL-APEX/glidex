import { test as base, expect, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";

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

/** What CreateVmForm fills in for each hypervisor. */
export const DEFAULT_FIRMWARE: Record<Hypervisor, string> = {
  cloudhypervisor: "~/.glidex/CLOUDHV.fd",
  qemu: "/usr/share/OVMF/OVMF_CODE_4M.fd",
};
export const FIRMWARE_HINT: Record<Hypervisor, string> = {
  cloudhypervisor: "Downloaded by glidex-install",
  qemu: "OVMF code image from the ovmf / edk2-ovmf package",
};
export const BINARY: Record<Hypervisor, string> = {
  cloudhypervisor: "cloud-hypervisor",
  qemu: "qemu-system-x86_64",
};
/** A guest that shut down cleanly logs this. (The kernel's own "reboot:
 * Power down" can be lost: Cloud-Hypervisor exits, closing its PTY, before
 * the console proxy reads the last bytes.) */
export const CLEAN_POWEROFF = /Reached target .*poweroff\.target|reboot: Power down/;

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

/** JSON (or text) from the scratch control plane's REST API. */
export async function api<T = any>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(API + path, init);
  const type = resp.headers.get("content-type") ?? "";
  return (type.includes("json") ? resp.json() : resp.text()) as Promise<T>;
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
  const firmware = DEFAULT_FIRMWARE[hypervisor].replace("~", E2E_HOME);
  if (!existsSync(firmware)) missing.push(`firmware ${firmware} is missing`);
  const status = await api("/ovs/status");
  if (status?.netd?.access !== "full") missing.push("glidex-netd is not usable (join the glidex group)");
  if (status?.host?.ovs_running !== true) missing.push("Open vSwitch is not running");
  return missing;
}

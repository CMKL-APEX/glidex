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

/** Show the create form's advanced options (boot mode, firmware, kernel, restarts). */
export async function showAdvanced(page: Page) {
  const details = page.getByTestId("advanced-options");
  if (!(await details.evaluate((d) => (d as HTMLDetailsElement).open))) {
    await details.locator("summary").click();
  }
}

/** A VM's card on the dashboard. */
export function vmCard(page: Page, name: string) {
  return page
    .locator("div")
    .filter({ has: page.getByText(name, { exact: true }) })
    .filter({ has: page.getByRole("link", { name: "View Details" }) })
    .last();
}

/** The console page's terminal. */
export function consoleTerminal(page: Page) {
  return page.getByTestId("console-terminal");
}

/** The text in the console's terminal, whichever it is: ghostty-web draws
 *  on a canvas, so it is read through the page's `screenText` hook. */
export function consoleText(page: Page): Promise<string> {
  return consoleTerminal(page).evaluate(
    (el) => (el as HTMLElement & { screenText?: () => string }).screenText?.() ?? "",
  );
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

/** Every VM, from `GET /vms` (the proto3 JSON shape of glidex.vm.v1: `{vms}`,
 * enums by their full names, spec/grpc.md §9.1). */
export async function listVms(): Promise<any[]> {
  return (await api<{ vms?: any[] }>("/vms")).vms ?? [];
}

/** Create a VM over the API; the VM from its `{vm}` answer (or the error body). */
export async function createVm(body: Record<string, unknown>): Promise<any> {
  const r = await api("/vms", { method: "POST", body });
  return r?.vm ?? r;
}

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
  if (status?.netd?.access !== "NETD_ACCESS_FULL") missing.push("glidex-netd is not usable (join the glidex group)");
  if (status?.host?.ovs_running !== true) missing.push("Open vSwitch is not running");
  return missing;
}

/** Ready firmware images for `hypervisor`, newest first: what CreateVmForm
 * offers, and the first is what it preselects. */
export async function firmwareImages(hypervisor: Hypervisor): Promise<any[]> {
  // The storage routes answer in the proto3 JSON shape (spec/grpc.md §9.1).
  return ((await api<{ images?: any[] }>("/images")).images ?? [])
    .filter((i) => i.kind === "IMAGE_KIND_FIRMWARE" && i.hypervisor === PROTO_HYPERVISOR[hypervisor] && i.status?.state === "IMAGE_STATE_READY" && !i.deleting)
    .sort((a, b) => Date.parse(b.created_at ?? "") - Date.parse(a.created_at ?? ""));
}

/** A hypervisor as the storage and VM APIs name it. */
export const PROTO_HYPERVISOR: Record<Hypervisor, string> = {
  cloudhypervisor: "HYPERVISOR_CLOUD_HYPERVISOR",
  qemu: "HYPERVISOR_QEMU",
};

/** A ready firmware image for `hypervisor`, pulled from the firmware
 * catalog if there is none (the installer's pinned CLOUDHV.fd or the
 * host's OVMF are copied; otherwise it is downloaded). */
export async function ensureFirmware(hypervisor: Hypervisor): Promise<any> {
  const have = await firmwareImages(hypervisor);
  if (have.length > 0) return have[0];
  const key = FIRMWARE_KEY[hypervisor];
  const pulled = await api("/images", { method: "POST", body: { firmware: key } });
  let img = pulled?.image;
  if (!img?.id) throw new Error(`firmware ${key} can't be pulled: ${pulled?.message ?? JSON.stringify(pulled)}`);
  const deadline = Date.now() + 120_000;
  while (img.status?.state === "IMAGE_STATE_DOWNLOADING" || img.status?.state === "IMAGE_STATE_VERIFYING") {
    if (Date.now() > deadline) throw new Error(`firmware ${key} did not download in time`);
    await new Promise((r) => setTimeout(r, 1000));
    img = (await api(`/images/${img.id}`)).image;
  }
  if (img.status?.state !== "IMAGE_STATE_READY") throw new Error(`firmware ${key} is ${JSON.stringify(img.status)}`);
  return img;
}

// ---- RPC mocks (the UI's calls are gRPC-Web, spec/grpc.md G-D8) ---------------

/** A gRPC-Web frame: flag byte, 4-byte big-endian length, payload. */
function grpcWebFrame(flag: number, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(5 + payload.length);
  out[0] = flag;
  new DataView(out.buffer).setUint32(1, payload.length);
  out.set(payload, 5);
  return out;
}

const utf8 = (s: string) => new TextEncoder().encode(s);

/** Protobuf wire encoding, just what `google.rpc.Status` needs. */
function varint(n: number): number[] {
  const out: number[] = [];
  while (n > 0x7f) {
    out.push((n & 0x7f) | 0x80);
    n >>>= 7;
  }
  out.push(n);
  return out;
}
function pbField(no: number, wire: number, bytes: number[] | Uint8Array): number[] {
  const tag = varint((no << 3) | wire);
  return wire === 2 ? [...tag, ...varint(bytes.length), ...bytes] : [...tag, ...bytes];
}
const str = (no: number, s: string) => pbField(no, 2, utf8(s));

/** gRPC status codes by name (the ones the mocks use). */
export const RpcCode = {
  INVALID_ARGUMENT: 3,
  NOT_FOUND: 5,
  ALREADY_EXISTS: 6,
  PERMISSION_DENIED: 7,
  FAILED_PRECONDITION: 9,
  ABORTED: 10,
  UNIMPLEMENTED: 12,
  INTERNAL: 13,
  UNAVAILABLE: 14,
  UNAUTHENTICATED: 16,
} as const;

/** An RPC error as the server sends it: the code, and `google.rpc.ErrorInfo`
 * with the REST code as `reason` and the REST status in metadata. */
export interface RpcErrorReply {
  code: number;
  /** The REST error code (`conflict`, `leader_changed`, …). */
  reason: string;
  message: string;
  /** What REST would have answered (409, 503, …). */
  httpStatus: number;
}

/** The trailers-only body of a gRPC-Web reply. */
function trailers(code: number, extra: Record<string, string> = {}): Uint8Array {
  const lines = [`grpc-status:${code}`, ...Object.entries(extra).map(([k, v]) => `${k}:${v}`)];
  return grpcWebFrame(0x80, utf8(lines.join("\r\n") + "\r\n"));
}

function errorTrailers(e: RpcErrorReply): Uint8Array {
  const info = [...str(1, e.reason), ...str(2, "glidex"), ...pbField(3, 2, [...str(1, "http_status"), ...str(2, String(e.httpStatus))])];
  const any = [...str(1, "type.googleapis.com/google.rpc.ErrorInfo"), ...pbField(2, 2, info)];
  const status = [...pbField(1, 0, varint(e.code)), ...str(2, e.message), ...pbField(3, 2, any)];
  return trailers(e.code, {
    "grpc-message": encodeURIComponent(e.message),
    "grpc-status-details-bin": Buffer.from(status).toString("base64").replace(/=+$/, ""),
  });
}

/** What a mocked RPC answers: a message (proto JSON), or an error. */
export type RpcReply = { message: unknown } | { error: RpcErrorReply };

/** The request message of a gRPC-Web+JSON call (proto JSON). */
export function rpcRequest(route: import("@playwright/test").Route): any {
  const body = route.request().postDataBuffer();
  if (!body || body.length < 5) return {};
  const text = body.subarray(5, 5 + body.readUInt32BE(1)).toString("utf8");
  return text ? JSON.parse(text) : {};
}

/** Answer `procedure` (`glidex.cluster.v1.ClusterService/GetClusterStatus`)
 * from `handler`, as the control plane would over gRPC-Web with JSON
 * messages; `undefined` from the handler lets the call through. */
export async function rpcRoute(
  page: Page,
  procedure: string,
  handler: (req: any, route: import("@playwright/test").Route) => RpcReply | undefined | Promise<RpcReply | undefined>,
): Promise<void> {
  await page.route(`**/api/${procedure}`, async (route) => {
    const reply = await handler(rpcRequest(route), route);
    if (!reply) return route.fallback();
    const body =
      "error" in reply
        ? errorTrailers(reply.error)
        : new Uint8Array([...grpcWebFrame(0, utf8(JSON.stringify(reply.message))), ...trailers(0)]);
    return route.fulfill({ status: 200, contentType: "application/grpc-web+json", body: Buffer.from(body) });
  });
}

/** Wait for the UI to call `procedure`; the request message (proto JSON). */
export async function waitForRpc(page: Page, procedure: string): Promise<any> {
  const req = await page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith(`/api/${procedure}`));
  const body = req.postDataBuffer();
  if (!body || body.length < 5) return {};
  return JSON.parse(body.subarray(5, 5 + body.readUInt32BE(1)).toString("utf8") || "{}");
}

/** Send a mocked call on to the control plane and read its answer message
 * (proto JSON); `null` when it failed or the page went away meanwhile. */
export async function rpcFetch(route: import("@playwright/test").Route): Promise<any | null> {
  try {
    const resp = await route.fetch();
    const body = await resp.body();
    // The first frame is the message (a trailers-only reply has none).
    if (body.length < 5 || (body[0] & 0x80) !== 0) return null;
    return JSON.parse(body.subarray(5, 5 + body.readUInt32BE(1)).toString("utf8") || "{}");
  } catch {
    return null;
  }
}

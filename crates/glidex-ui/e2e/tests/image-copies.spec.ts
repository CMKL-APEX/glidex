// Images across architectures and copies on chosen nodes in the UI
// (spec/clustering.md §9.2.1–3, clustering-ui.md §5.3). The cluster, its
// images and catalogs are mocked: an x86_64 server and an aarch64 agent.
import type { Page, Route } from "@playwright/test";
import { expect, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /Failed to load resource/ });

const S1 = "11111111-1111-4111-8111-111111111111";
const A1 = "22222222-2222-4222-8222-222222222222";

const json = (r: Route, v: unknown, status = 200) => r.fulfill({ status, contentType: "application/json", body: JSON.stringify(v) });

function node(id: string, name: string, role: string, arch: string, spare: { name: string; bytes: number; mtime: number }[] = []) {
  return {
    meta: { id, name, created_at: 1790000000, generation: 1, resource_version: 1 },
    spec: { name, role, unschedulable: false, labels: {} },
    status: {
      phase: "Active",
      ready: "True",
      versions: { glidex: "0.1.0" },
      capacity: { cpus: 8, memory_mib: 16384 },
      allocatable: { cpus: 7, memory_mib: 14336 },
      features: { kvm: true, hypervisors: ["cloudhypervisor"], arch, spare_image_files: spare },
    },
  };
}

/** An image in the storage API's proto3 JSON shape (spec/grpc.md §9.1). */
function image(id: string, name: string, arch: string, copies: unknown[]) {
  return {
    id, name, kind: "IMAGE_KIND_DISK", arch: `ARCH_${arch.toUpperCase()}`, copies,
    source: { catalog: { key: name, url: "https://example.invalid/x.img" } },
    status: { state: "IMAGE_STATE_READY" }, format: "DISK_FORMAT_QCOW2", virtual_size_bytes: "3758096384", file_size_bytes: "600000000",
    sha256: "ab".repeat(32), created_at: "2026-09-21T13:46:40Z", verified: true,
  };
}

type Seen = { method: string; path: string; body: unknown }[];

async function mock(page: Page): Promise<Seen> {
  const seen: Seen = [];
  const record = (r: Route) => {
    let body: unknown = null;
    try {
      body = r.request().postDataJSON();
    } catch {
      body = null;
    }
    const u = new URL(r.request().url());
    seen.push({ method: r.request().method(), path: u.pathname.replace(/^\/api/, "") + u.search, body });
  };
  await page.route("**/api/watch*", (r) => json(r, { error: "too_many_watchers", message: "poll" }, 503));
  await page.route("**/api/nodes", (r) => json(r, [node(S1, "s1", "server", "x86_64"), node(A1, "a1", "agent", "aarch64", [{ name: "old.qcow2", bytes: 3758096384, mtime: 1 }])]));
  await page.route(`**/api/nodes/${A1}`, (r) => json(r, node(A1, "a1", "agent", "aarch64", [{ name: "old.qcow2", bytes: 3758096384, mtime: 1 }])));
  await page.route("**/api/cluster/status", (r) => json(r, { cluster_id: "c", node_id: S1, name: "s1", role: "server", nodes: [] }));
  await page.route("**/api/images/catalog*", (r) => {
    record(r);
    const arch = new URL(r.request().url()).searchParams.get("arch") ?? "x86_64";
    return json(r, { items: [{ key: "ubuntu-26.04", distro: "Ubuntu", release: "26.04", arch: `ARCH_${arch.toUpperCase()}`, url: "https://example.invalid/x.img" }] });
  });
  await page.route("**/api/images/firmware-catalog*", (r) => json(r, { items: [] }));
  await page.route("**/api/images", (r) => {
    if (r.request().method() === "POST") {
      record(r);
      return json(r, { image: image("new", "ubuntu-26.04-aarch64", "aarch64", []), created: true }, 202);
    }
    return json(r, {
      images: [
        image("img-x", "ubuntu-26.04", "x86_64", [
          { node: S1, node_name: "s1", state: "ready", bytes: "600000000" },
          { node: A1, node_name: "a1", state: "ready", bytes: "600000000", pinned: true },
        ]),
      ],
    });
  });
  for (const pattern of ["**/api/images/*/copies", "**/api/images/*/copies/*"]) {
    await page.route(pattern, (r) => {
      record(r);
      return json(r, { image: image("img-x", "ubuntu-26.04", "x86_64", []) }, 202);
    });
  }
  await page.route("**/api/nodes/*/spare-image-files/delete", (r) => {
    record(r);
    return json(r, { node: "a1", files: 1, bytes: 3758096384 }, 202);
  });
  return seen;
}

test("the catalogs follow the chosen architecture and a pull says where copies go", async ({ page }) => {
  const seen = await mock(page);
  await page.goto("/images");
  // Two architectures in the cluster: the catalogs are for one at a time.
  const pick = page.getByLabel("Catalogs for");
  await expect(pick).toHaveValue("x86_64");
  await pick.selectOption("aarch64");
  await expect.poll(() => seen.some((s) => s.path === "/images/catalog?arch=aarch64")).toBe(true);

  await page.getByRole("button", { name: "Pull" }).first().click();
  const dialog = page.getByRole("dialog", { name: /Pull Ubuntu 26.04/ });
  // Only nodes of that architecture.
  await expect(dialog.getByRole("checkbox", { name: "a1" })).toBeVisible();
  await expect(dialog.getByRole("checkbox", { name: "s1" })).toHaveCount(0);
  await expect(dialog.getByRole("checkbox", { name: /Keep the server's copy/ })).toBeDisabled();
  await dialog.getByRole("checkbox", { name: "a1" }).check();
  await dialog.getByRole("checkbox", { name: /Keep the server's copy/ }).uncheck();
  await dialog.getByRole("button", { name: "Pull" }).click();
  await expect.poll(() => seen.find((s) => s.method === "POST" && s.path === "/images")?.body).toEqual({
    catalog: "ubuntu-26.04",
    arch: "ARCH_AARCH64",
    nodes: [A1],
    keep_server_copy: false,
  });
});

test("an image is copied to a node of its architecture, and a copy let go", async ({ page }) => {
  const seen = await mock(page);
  await page.goto("/images");
  const row = page.getByRole("row", { name: /ubuntu-26.04/ });
  await expect(row).toContainText("x86_64");

  // Copy to node: only x86_64 nodes without a copy; a1 is aarch64 and s1 has one.
  await row.getByRole("button", { name: "Copy to node…" }).click();
  await expect(page.getByRole("dialog")).toContainText("Every x86_64 node has a copy already");
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();

  // Copies: a1's is kept there; s1's can go.
  await row.getByText(/on 2 nodes/).click();
  await expect(row).toContainText("(kept here)");
  await row.getByRole("listitem").filter({ hasText: "s1" }).getByRole("button", { name: "Remove" }).click();
  await expect.poll(() => seen.find((s) => s.method === "DELETE")?.path).toBe(`/images/img-x/copies/${S1}`);
});

test("a node's unrecorded image files are listed and deleted on request", async ({ page }) => {
  const seen = await mock(page);
  await page.goto(`/cluster/nodes/${A1}`);
  await expect(page.getByText("aarch64", { exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: /Unrecorded image files \(1/ })).toBeVisible();
  await expect(page.getByText("old.qcow2")).toBeVisible();
  await page.getByRole("button", { name: "Delete unrecorded files" }).click();
  await expect.poll(() => seen.find((s) => s.path === `/nodes/${A1}/spare-image-files/delete`)?.method).toBe("POST");
});

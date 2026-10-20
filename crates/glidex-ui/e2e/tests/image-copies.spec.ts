// Images across architectures and copies on chosen nodes in the UI
// (spec/clustering.md §9.2.1–3, clustering-ui.md §5.3). The cluster, its
// images and catalogs are mocked: an x86_64 server and an aarch64 agent.
import type { Page } from "@playwright/test";
import { expect, RpcCode, rpcRoute, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /Failed to load resource/ });

const S1 = "11111111-1111-4111-8111-111111111111";
const A1 = "22222222-2222-4222-8222-222222222222";

const ok = (message: unknown) => ({ message });

/** A node as glidex.cluster.v1 sends it (proto3 JSON: 64-bit numbers as strings, RFC 3339 times). */
function node(id: string, name: string, role: "server" | "agent", arch: string, spare: { name: string; bytes: string; mtime: string }[] = []) {
  return {
    meta: { id, name, created_at: "2026-09-21T14:13:20Z", generation: "1", resource_version: "1" },
    spec: { name, role: role === "server" ? "NODE_ROLE_SERVER" : "NODE_ROLE_AGENT" },
    status: {
      phase: "NODE_PHASE_ACTIVE",
      ready: "CONDITION_STATUS_TRUE",
      versions: { glidex: "0.1.0" },
      capacity: { cpus: 8, memory_mib: "16384" },
      allocatable: { cpus: 7, memory_mib: "14336" },
      features: { kvm: true, hypervisors: ["cloudhypervisor"], arch, ...(spare.length ? { spare_image_files: spare } : {}) },
    },
  };
}

const SPARE = [{ name: "old.qcow2", bytes: "3758096384", mtime: "1970-01-01T00:00:01Z" }];

/** An image in the storage API's proto3 JSON shape (spec/grpc.md §9.1). */
function image(id: string, name: string, arch: string, copies: unknown[]) {
  return {
    id, name, kind: "IMAGE_KIND_DISK", arch: `ARCH_${arch.toUpperCase()}`, copies,
    source: { catalog: { key: name, url: "https://example.invalid/x.img" } },
    status: { state: "IMAGE_STATE_READY" }, format: "DISK_FORMAT_QCOW2", virtual_size_bytes: "3758096384", file_size_bytes: "600000000",
    sha256: "ab".repeat(32), created_at: "2026-09-21T13:46:40Z", verified: true,
  };
}

type Seen = { procedure: string; body: any }[];

const IMAGES = "glidex.storage.v1.ImageService";

async function mock(page: Page): Promise<Seen> {
  const seen: Seen = [];
  const record = (procedure: string, body: unknown) => seen.push({ procedure, body });
  // No live stream: the pages poll, so the mocks are what they see.
  await rpcRoute(page, "glidex.watch.v1.WatchService/Watch", () => ({
    error: { code: RpcCode.UNAVAILABLE, reason: "too_many_watchers", message: "poll", httpStatus: 503 },
  }));
  await rpcRoute(page, "glidex.cluster.v1.NodeService/ListNodes", () => ok({ nodes: [node(S1, "s1", "server", "x86_64"), node(A1, "a1", "agent", "aarch64", SPARE)] }));
  await rpcRoute(page, "glidex.cluster.v1.NodeService/GetNode", (req) => (req.id === A1 ? ok({ node: node(A1, "a1", "agent", "aarch64", SPARE) }) : undefined));
  await rpcRoute(page, "glidex.cluster.v1.ClusterService/GetClusterStatus", () => ok({ clustered: true, cluster_id: "c", node_id: S1, name: "s1", role: "NODE_ROLE_SERVER" }));
  await rpcRoute(page, `${IMAGES}/GetImageCatalog`, (req) => {
    record("GetImageCatalog", req);
    const arch = req.arch ?? "ARCH_X86_64";
    return ok({ items: [{ key: "ubuntu-26.04", distro: "Ubuntu", release: "26.04", arch, url: "https://example.invalid/x.img" }] });
  });
  await rpcRoute(page, `${IMAGES}/GetFirmwareCatalog`, () => ok({ items: [] }));
  await rpcRoute(page, `${IMAGES}/PullImage`, (req) => {
    record("PullImage", req);
    return ok({ image: image("new", "ubuntu-26.04-aarch64", "aarch64", []), created: true });
  });
  await rpcRoute(page, `${IMAGES}/ListImages`, () =>
    ok({
      images: [
        image("img-x", "ubuntu-26.04", "x86_64", [
          { node: S1, node_name: "s1", state: "ready", bytes: "600000000" },
          { node: A1, node_name: "a1", state: "ready", bytes: "600000000", pinned: true },
        ]),
      ],
    }),
  );
  for (const m of ["AddImageCopy", "RemoveImageCopy"]) {
    await rpcRoute(page, `${IMAGES}/${m}`, (req) => {
      record(m, req);
      return ok({ image: image("img-x", "ubuntu-26.04", "x86_64", []) });
    });
  }
  await rpcRoute(page, "glidex.cluster.v1.NodeService/DeleteSpareImageFiles", (req) => {
    record("DeleteSpareImageFiles", req);
    return ok({ node: "a1", files: 1, bytes: "3758096384" });
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
  await expect.poll(() => seen.some((s) => s.procedure === "GetImageCatalog" && s.body.arch === "ARCH_AARCH64")).toBe(true);

  await page.getByRole("button", { name: "Pull" }).first().click();
  const dialog = page.getByRole("dialog", { name: /Pull Ubuntu 26.04/ });
  // Only nodes of that architecture.
  await expect(dialog.getByRole("checkbox", { name: "a1" })).toBeVisible();
  await expect(dialog.getByRole("checkbox", { name: "s1" })).toHaveCount(0);
  await expect(dialog.getByRole("checkbox", { name: /Keep the server's copy/ })).toBeDisabled();
  await dialog.getByRole("checkbox", { name: "a1" }).check();
  await dialog.getByRole("checkbox", { name: /Keep the server's copy/ }).uncheck();
  await dialog.getByRole("button", { name: "Pull" }).click();
  await expect.poll(() => seen.find((s) => s.procedure === "PullImage")?.body).toEqual({
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
  await expect.poll(() => seen.find((s) => s.procedure === "RemoveImageCopy")?.body).toEqual({ id: "img-x", node: S1 });
});

test("a node's unrecorded image files are listed and deleted on request", async ({ page }) => {
  const seen = await mock(page);
  await page.goto(`/cluster/nodes/${A1}`);
  await expect(page.getByText("aarch64", { exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: /Unrecorded image files \(1/ })).toBeVisible();
  await expect(page.getByText("old.qcow2")).toBeVisible();
  await page.getByRole("button", { name: "Delete unrecorded files" }).click();
  await expect.poll(() => seen.find((s) => s.procedure === "DeleteSpareImageFiles")?.body).toEqual({ id: A1 });
});

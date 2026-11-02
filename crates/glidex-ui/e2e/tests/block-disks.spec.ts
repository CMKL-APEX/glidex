// Disks on a block datastore, and which disks a VM's delete takes, in the
// UI (spec/24-storage.md S19; user-guide/vms.md, Delete). The datastores,
// disks and VM are mocked: a hyperstor datastore, the default for disks.
import type { Page } from "@playwright/test";
import { expect, RpcCode, rpcRoute, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /Failed to load resource/ });

const ok = (message: unknown) => ({ message });
const STORAGE = "glidex.storage.v1";
const VM = "11111111-1111-4111-8111-111111111111";

type Seen = { procedure: string; body: any }[];

function disk(id: string, name: string, extra: Record<string, unknown> = {}) {
  return { id, name, size_bytes: "10737418240", format: "DISK_FORMAT_RAW", status: "ready", phase: "DISK_PHASE_READY", ...extra };
}

async function mock(page: Page): Promise<Seen> {
  const seen: Seen = [];
  // No live stream: the pages poll, so the mocks are what they see.
  await rpcRoute(page, "glidex.watch.v1.WatchService/Watch", () => ({
    error: { code: RpcCode.UNAVAILABLE, reason: "too_many_watchers", message: "poll", httpStatus: 503 },
  }));
  await rpcRoute(page, `${STORAGE}.DatastoreService/ListDatastores`, () =>
    ok({
      datastores: [
        { id: "local", name: "local", driver: "DATASTORE_DRIVER_DIR" },
        { id: "hs", name: "hs", driver: "DATASTORE_DRIVER_HYPERSTOR", shared: true, default_for_disks: true },
      ],
    }),
  );
  await rpcRoute(page, `${STORAGE}.ImageService/ListImages`, () =>
    ok({
      images: [
        {
          id: "img", name: "ubuntu", kind: "IMAGE_KIND_DISK", arch: "ARCH_X86_64", status: { state: "IMAGE_STATE_READY" },
          format: "DISK_FORMAT_QCOW2", virtual_size_bytes: "3758096384", created_at: "2026-09-21T13:46:40Z",
        },
      ],
    }),
  );
  await rpcRoute(page, `${STORAGE}.DiskService/ListDisks`, () =>
    ok({
      disks: [
        disk("d-root", "vm1-root", { attached_to: VM, owner: VM, datastore: "hs" }),
        disk("d-data", "data1", { attached_to: VM, datastore: "hs" }),
        // The root disk of a VM since deleted with keep_disk.
        disk("d-kept", "kept1", { owner: "22222222-2222-4222-8222-222222222222", datastore: "hs" }),
      ],
    }),
  );
  await rpcRoute(page, `${STORAGE}.DiskService/CreateDisk`, (req) => {
    seen.push({ procedure: "CreateDisk", body: req });
    return ok({ disk: disk("d-new", req.name, { datastore: "hs" }), settled: true });
  });
  await rpcRoute(page, `${STORAGE}.DiskService/DeleteDisk`, (req) => {
    seen.push({ procedure: "DeleteDisk", body: req });
    return ok({});
  });
  await rpcRoute(page, "glidex.vm.v1.VmService/ListVms", () =>
    ok({ vms: [{ id: VM, name: "vm1", project: "p", state: "VM_STATE_STOPPED", root_disk: "d-root", data_disks: ["d-data"] }] }),
  );
  await rpcRoute(page, "glidex.vm.v1.VmService/DeleteVm", (req) => {
    seen.push({ procedure: "DeleteVm", body: req });
    return ok({});
  });
  return seen;
}

test("a disk on a hyperstor datastore is raw, and can be linked", async ({ page }) => {
  const seen = await mock(page);
  await page.goto("/disks");
  await page.getByRole("button", { name: "Create Disk" }).click();
  const form = page.getByRole("dialog");
  await expect(form.getByText("raw (block volume)")).toBeVisible();
  await expect(form.getByRole("option", { name: /^Linked/ })).toBeEnabled();
  await form.locator("input").first().fill("thin1");
  await form.getByRole("button", { name: "Create disk" }).click();
  await expect.poll(() => seen.find((s) => s.procedure === "CreateDisk")?.body).toMatchObject({
    name: "thin1",
    datastore: "hs",
    clone: "CLONE_MODE_LINKED",
  });
  expect(seen.find((s) => s.procedure === "CreateDisk")?.body.format).toBeUndefined();
});

test("deleting a VM names the disks it takes, and can keep its root disk", async ({ page }) => {
  const seen = await mock(page);
  await page.goto("/");
  await page.getByRole("button", { name: "Delete" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("vm1-root was made for this VM and is deleted with it");
  await expect(dialog).toContainText("Only detached, and kept: data1");
  await dialog.getByRole("checkbox", { name: "Keep disk vm1-root" }).check();
  await dialog.getByRole("button", { name: "Delete VM", exact: true }).click();
  await expect.poll(() => seen.find((s) => s.procedure === "DeleteVm")?.body).toMatchObject({ id: VM, keep_disk: true });
});

test("a disk no VM claims can be deleted, whoever it was made for", async ({ page }) => {
  const seen = await mock(page);
  await page.goto("/disks");
  const row = (name: string) => page.getByRole("row").filter({ hasText: name });
  await expect(row("vm1-root").getByRole("button", { name: "Delete" })).toBeDisabled();
  await row("kept1").getByRole("button", { name: "Delete" }).click();
  await expect.poll(() => seen.find((s) => s.procedure === "DeleteDisk")?.body).toMatchObject({ id: "d-kept" });
});

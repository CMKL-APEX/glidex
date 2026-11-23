// Live migration on the VM page (spec/32-live-migration.md decision 20): the
// Migrate dialog shows the check's answer per node and sends the chosen node
// and downtime; the page then shows the migration's phase with Cancel, and a
// failed migration's reason. The cluster, the VM's running state and the
// migration RPCs are stand-ins in front of the scratch (standalone) control
// plane.
import type { Page } from "@playwright/test";
import { createVm, del, expect, listVms, RpcCode, rpcFetch, rpcRoute, test } from "../fixtures";

// The mocked stream refusal may be logged as a failed load.
test.use({ ignoredConsoleErrors: /Failed to load resource/ });

const name = (hv: string) => `migrate-${hv}`;

test.afterEach(async ({ hypervisor }) => {
  const vm = (await listVms()).find((v) => v.name === name(hypervisor));
  if (vm) await del(`/vms/${vm.id}?wait=60`);
});

const S1 = "11111111-1111-4111-8111-111111111111";
const A1 = "22222222-2222-4222-8222-222222222222";
const A2 = "33333333-3333-4333-8333-333333333333";

function node(id: string, name: string) {
  return {
    meta: { id, name, created_at: "2026-09-21T14:13:20Z", generation: "1", resource_version: "1" },
    spec: { name, role: "NODE_ROLE_AGENT" },
    status: { phase: "NODE_PHASE_ACTIVE", ready: "CONDITION_STATUS_TRUE", features: { kvm: true, hypervisors: ["cloudhypervisor"] } },
  };
}

/** Three ready nodes, and no live stream, so the page polls. */
async function mockCluster(page: Page) {
  await rpcRoute(page, "glidex.watch.v1.WatchService/Watch", () => ({
    error: { code: RpcCode.UNAVAILABLE, reason: "too_many_watchers", message: "poll", httpStatus: 503 },
  }));
  await rpcRoute(page, "glidex.cluster.v1.NodeService/ListNodes", () => ({
    message: { nodes: [node(S1, "s1"), node(A1, "a1"), node(A2, "a2")] },
  }));
}

test("a running VM migrates to a node the check lets through, with Cancel while it copies", async ({ page, hypervisor }) => {
  const vm = await createVm({
    name: name(hypervisor),
    vcpu_count: 1,
    mem_size_mib: 256,
    kernel_image_path: "/nonexistent/vmlinux",
    rootfs_path: "/nonexistent/disk.img",
  });
  expect(vm.id, JSON.stringify(vm)).toBeTruthy();
  await mockCluster(page);

  // The VM as the page sees it: running on s1, then each migration phase.
  let migration: Record<string, unknown> | undefined;
  await rpcRoute(page, "glidex.vm.v1.VmService/GetVm", async (_, route) => {
    const real = await rpcFetch(route);
    if (!real?.vm) return undefined;
    return {
      message: {
        vm: {
          ...real.vm,
          state: "VM_STATE_RUNNING",
          desired_state: "POWER_STATE_RUNNING",
          node: S1,
          node_name: "s1",
          migration,
        },
      },
    };
  });
  // What the page asked, as the stand-ins received it.
  const asked: { method: string; req: any }[] = [];
  await rpcRoute(page, "glidex.vm.v1.VmService/MigrateVm", (req: any) => {
    asked.push({ method: "MigrateVm", req });
    return req.check
      ? {
          message: {
            check: {
              targets: [
                { node: A1, name: "a1" },
                { node: A2, name: "a2", reasons: ["its CPU lacks leaf 0x7 EDX bit 26"] },
              ],
            },
          },
        }
      : { message: { vm: { id: vm.id, name: vm.name } } };
  });
  await rpcRoute(page, "glidex.vm.v1.VmService/CancelMigration", (req: any) => {
    asked.push({ method: "CancelMigration", req });
    return { message: { vm: { id: vm.id, name: vm.name } } };
  });
  const sent = (method: string) => asked.filter((a) => a.method === method && !a.req.check).map((a) => a.req);

  await page.goto(`/vms/${vm.id}`);
  await page.getByRole("button", { name: "Migrate…" }).click();
  const dialog = page.getByRole("dialog");
  const select = dialog.getByLabel("Node");
  // The refused node is listed with its reason, and can't be chosen.
  await expect(select.locator("option", { hasText: "a2" })).toHaveText(/leaf 0x7 EDX bit 26/);
  await expect(select.locator("option", { hasText: "a2" })).toBeDisabled();
  await select.selectOption(A1);
  await dialog.getByLabel("Longest pause at the switch (ms)").fill("500");

  migration = { target: A1, phase: "MIGRATION_PHASE_COPYING", started_at: "1795356305", downtime_ms: 500 };
  await dialog.getByRole("button", { name: "Migrate", exact: true }).click();
  await expect.poll(() => sent("MigrateVm")).toEqual([expect.objectContaining({ id: vm.id, node: A1, downtime_ms: 500 })]);

  // In progress: its phase and target, and Cancel.
  const status = page.getByTestId("migration-status");
  await expect(status).toContainText("copying memory");
  await expect(status).toContainText("a1");
  await status.getByRole("button", { name: "Cancel migration" }).click();
  await expect.poll(() => sent("CancelMigration")).toEqual([expect.objectContaining({ id: vm.id })]);

  // The cancel ends in Failed, with its reason; the VM can migrate again.
  migration = { target: A1, phase: "MIGRATION_PHASE_FAILED", reason: "cancelled by api:someone" };
  await page.reload();
  await expect(page.getByTestId("migration-failed")).toContainText("cancelled by api:someone");
  await expect(page.getByRole("button", { name: "Migrate…" })).toBeVisible();
});

test("a VM that can't migrate says why, and Migrate stays off", async ({ page, hypervisor }) => {
  const vm = await createVm({
    name: name(hypervisor),
    vcpu_count: 1,
    mem_size_mib: 256,
    kernel_image_path: "/nonexistent/vmlinux",
    rootfs_path: "/nonexistent/disk.img",
  });
  await mockCluster(page);
  await rpcRoute(page, "glidex.vm.v1.VmService/GetVm", async (_, route) => {
    const real = await rpcFetch(route);
    return real?.vm
      ? { message: { vm: { ...real.vm, state: "VM_STATE_RUNNING", desired_state: "POWER_STATE_RUNNING", node: S1, node_name: "s1" } } }
      : undefined;
  });
  await rpcRoute(page, "glidex.vm.v1.VmService/MigrateVm", () => ({
    message: {
      check: {
        reasons: ["its root is a host path, /nonexistent/disk.img, on its node"],
        targets: [{ node: A1, name: "a1" }],
      },
    },
  }));

  await page.goto(`/vms/${vm.id}`);
  await page.getByRole("button", { name: "Migrate…" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByTestId("migrate-vm-reasons")).toContainText("its root is a host path");
  await expect(dialog.getByRole("button", { name: "Migrate", exact: true })).toBeDisabled();
});

// A Pylon VM that pylond doesn't manage says so on its page
// (operator-guide/installation.md, Pylon; spec/30-pylon.md P16). Against the
// real control plane with a VM that is never started; the PylonManaged
// condition, which only a refused registration on a Pylon host produces, is
// added to the real GetVm answer.
import { createVm, del, expect, listVms, rpcRoute, test } from "../fixtures";

const vmName = (hv: string) => `pylon-managed-${hv}`;
const message =
  "pylond has no registration for the VM: it refused it (the VMM says why in the console log, `gxctl log`) or restarted since; restart the VM";

test.afterEach(async ({ hypervisor }) => {
  const vm = (await listVms()).find((v) => v.name === vmName(hypervisor));
  if (vm) await del(`/vms/${vm.id}?wait=60`);
});

/** The real GetVm answer, with `PylonManaged` set to `status` when given. */
async function withPylonManaged(page: import("@playwright/test").Page, status?: string) {
  await rpcRoute(page, "glidex.vm.v1.VmService/GetVm", async (_req, route) => {
    const body = await (await route.fetch()).body();
    const reply = JSON.parse(body.subarray(5, 5 + body.readUInt32BE(1)).toString("utf8"));
    if (status) {
      reply.vm.conditions = [
        ...(reply.vm.conditions ?? []),
        { kind: "PylonManaged", status, reason: "NotRegistered", message },
      ];
    }
    return { message: reply };
  });
}

test("the VM page says when pylond doesn't manage the VM", async ({ page, hypervisor }) => {
  const vm = await createVm({
    name: vmName(hypervisor),
    vcpu_count: 1,
    mem_size_mib: 256,
    kernel_image_path: "/nonexistent/vmlinux",
    rootfs_path: "/nonexistent/disk.img",
  });
  expect(vm.id, JSON.stringify(vm)).toBeTruthy();

  await test.step("no warning without the condition, or while it is True", async () => {
    await withPylonManaged(page, "CONDITION_STATUS_TRUE");
    await page.goto(`/vms/${vm.id}`);
    await expect(page.getByText("256 MiB")).toBeVisible();
    await expect(page.getByTestId("pylon-unmanaged")).toHaveCount(0);
    await page.unroute("**/api/glidex.vm.v1.VmService/GetVm");
  });

  await test.step("the condition's message when it is False", async () => {
    await withPylonManaged(page, "CONDITION_STATUS_FALSE");
    await page.reload();
    const warning = page.getByTestId("pylon-unmanaged");
    await expect(warning).toBeVisible();
    await expect(warning).toContainText("Not managed by pylond");
    await expect(warning).toContainText("restart the VM");
  });
});

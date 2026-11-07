// Renaming a VM from its page (user-guide/vms.md, Rename;
// spec/12-reconciliation.md §7.7), against the real control plane: a VM
// that is never started, and another VM whose name is taken.
import { api, createVm, del, expect, listVms, test, vmCard } from "../fixtures";

const names = (hv: string) => [`rename-${hv}`, `renamed-${hv}`, `rename-taken-${hv}`];

test.afterEach(async ({ hypervisor }) => {
  for (const vm of await listVms()) {
    if (names(hypervisor).includes(vm.name)) await del(`/vms/${vm.id}?wait=60`);
  }
});

test("the VM page renames a VM, and refuses a name taken in its project", async ({ page, hypervisor }) => {
  const [from, to, taken] = names(hypervisor);
  const spec = { vcpu_count: 1, mem_size_mib: 256, kernel_image_path: "/nonexistent/vmlinux", rootfs_path: "/nonexistent/disk.img" };
  const vm = await createVm({ name: from, ...spec });
  expect(vm.id, JSON.stringify(vm)).toBeTruthy();
  const other = await createVm({ name: taken, ...spec, project: vm.project });
  expect(other.id, JSON.stringify(other)).toBeTruthy();

  await page.goto(`/vms/${vm.id}`);
  await expect(page.getByRole("heading", { name: from })).toBeVisible();

  await test.step("a taken name is refused and nothing changes", async () => {
    await page.getByRole("button", { name: "Rename" }).click();
    await page.getByLabel("New name").fill(taken);
    await page.getByRole("button", { name: "Rename" }).click();
    await expect(page.getByText(/already exists/)).toBeVisible();
    expect((await api(`/vms/${vm.id}`)).vm.name).toBe(from);
  });

  await test.step("a free name is taken, and the list shows it", async () => {
    await page.getByLabel("New name").fill(to);
    await page.getByRole("button", { name: "Rename" }).click();
    await expect(page.getByRole("heading", { name: to })).toBeVisible();
    expect((await api(`/vms/${vm.id}`)).vm.name).toBe(to);
    await page.goto("/");
    await expect(vmCard(page, to)).toBeVisible();
    await expect(page.getByText(from, { exact: true })).toHaveCount(0);
  });
});

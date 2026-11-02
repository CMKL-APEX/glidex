// Changing a VM's memory and its data disks from its page (user-guide/vms.md,
// Change a VM and Data disks), against the real control plane: a VM that is
// never started, and a blank disk of its project.
import { api, createVm, del, expect, listVms, test } from "../fixtures";

const vmName = (hv: string) => `memdisk-${hv}`;
const diskName = (hv: string) => `memdisk-data-${hv}`;

test.afterEach(async ({ hypervisor }) => {
  const vm = (await listVms()).find((v) => v.name === vmName(hypervisor));
  if (vm) await del(`/vms/${vm.id}?wait=60`);
  await del(`/disks/${diskName(hypervisor)}`);
});

test("the VM page changes the memory and attaches and detaches a disk", async ({ page, hypervisor }) => {
  const vm = await createVm({
    name: vmName(hypervisor),
    vcpu_count: 1,
    mem_size_mib: 256,
    kernel_image_path: "/nonexistent/vmlinux",
    rootfs_path: "/nonexistent/disk.img",
  });
  expect(vm.id, JSON.stringify(vm)).toBeTruthy();
  const made = await api("/disks", { method: "POST", body: { name: diskName(hypervisor), size_bytes: 64 * 1024 * 1024, project: vm.project } });
  expect(made?.disk?.id, JSON.stringify(made)).toBeTruthy();
  const diskId: string = made.disk.id;
  await expect
    .poll(async () => (await api(`/disks/${diskId}`)).disk?.status, { timeout: 30_000 })
    .toBe("ready");
  const current = async () => (await api(`/vms/${vm.id}`)).vm;

  await page.goto(`/vms/${vm.id}`);
  await expect(page.getByText("256 MiB")).toBeVisible();

  await test.step("change the memory", async () => {
    await page.getByRole("button", { name: "Change memory" }).click();
    await page.getByLabel("Memory (MiB)").fill("1024");
    await expect(page.getByText("1 GiB. Applies when the VM starts.")).toBeVisible();
    await page.getByRole("button", { name: "Apply" }).click();
    await expect(page.getByText("1024 MiB")).toBeVisible();
    expect((await current()).mem_size_mib).toBe(1024);
  });

  const disks = page.getByTestId("vm-disks");
  await test.step("attach the free disk", async () => {
    await disks.getByLabel("Disk to attach").selectOption({ label: diskName(hypervisor) });
    await disks.getByRole("button", { name: "Attach", exact: true }).click();
    await expect(disks.getByRole("button", { name: "Detach" })).toBeVisible();
    expect((await current()).data_disks).toEqual([diskId]);
    expect((await api(`/disks/${diskId}`)).disk.attached_to).toBe(vm.id);
    // Attached, it's no longer offered.
    await expect(disks.getByLabel("Disk to attach")).toHaveCount(0);
  });

  await test.step("detach it", async () => {
    // Its confirm() is accepted by the fixture.
    await disks.getByRole("button", { name: "Detach" }).click();
    await expect(disks.getByRole("button", { name: "Detach" })).toHaveCount(0);
    expect((await current()).data_disks ?? []).toEqual([]);
    await expect(disks.getByLabel("Disk to attach")).toBeVisible();
  });
});

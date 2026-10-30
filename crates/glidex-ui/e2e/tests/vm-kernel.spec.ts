// The VM page's kernel panel (spec/23-kernel-boot.md §7): no kernel image is
// needed, the VM is shown as booting one.
import { createVm, del, expect, listVms, rpcFetch, rpcRoute, test } from "../fixtures";

const name = (hv: string) => `kernel-${hv}`;

test.afterEach(async ({ hypervisor }) => {
  const vm = (await listVms()).find((v) => v.name === name(hypervisor));
  if (vm) await del(`/vms/${vm.id}?wait=60`);
});

test("following the guest's kernel sends a JSON field mask", async ({ page, hypervisor }) => {
  const vm = await createVm({
    name: name(hypervisor),
    vcpu_count: 1,
    mem_size_mib: 256,
    kernel_image_path: "/nonexistent/vmlinux",
    rootfs_path: "/nonexistent/disk.img",
  });
  expect(vm.id, JSON.stringify(vm)).toBeTruthy();
  // As if it booted a kernel image from a managed disk.
  await rpcRoute(page, "glidex.vm.v1.VmService/GetVm", async (_, route) => {
    const real = await rpcFetch(route);
    return real?.vm ? { message: { vm: { ...real.vm, kernel: "kernel-image", root_disk: "root-disk" } } } : undefined;
  });
  await rpcRoute(page, "glidex.vm.v1.VmService/UpdateVm", () => ({ message: {} }));

  await page.goto(`/vms/${vm.id}`);
  const panel = page.getByTestId("vm-kernel");
  await expect(panel).toBeVisible();
  const [request] = await Promise.all([
    page.waitForRequest((r) => r.url().endsWith("/api/glidex.vm.v1.VmService/UpdateVm")),
    // Click, not check(): the mocked VM still reads as not following.
    panel.getByLabel("Follow the guest's kernel").click(),
  ]);
  const body = request.postDataBuffer()!;
  const sent = JSON.parse(body.subarray(5, 5 + body.readUInt32BE(1)).toString("utf8"));
  // FieldMask JSON paths are lowerCamelCase.
  expect(sent).toMatchObject({ id: vm.id, change: { kernel_from_disk: true }, update_mask: "kernelFromDisk" });
});

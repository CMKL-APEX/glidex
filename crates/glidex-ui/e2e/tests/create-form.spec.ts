// CreateVmForm per hypervisor: no VM is booted, so no KVM or image needed.
import { api, createVm, del, ensureFirmware, expect, field, firmwareImages, FIRMWARE_KEY, LABEL, listVms, PROTO_HYPERVISOR, rpcRoute, test, vmCard, waitForRpc, type Hypervisor, showAdvanced } from "../fixtures";

const other = (hv: Hypervisor): Hypervisor => (hv === "qemu" ? "cloudhypervisor" : "qemu");

// The form offers firmware images; make sure there is one to pick.
test.beforeAll(async ({ hypervisor }) => {
  await ensureFirmware(hypervisor);
});

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "+ Create VM" }).click();
  await showAdvanced(page);
});

test("offers firmware boot through the hypervisor's firmware image", async ({ page, hypervisor }) => {
  const [newest] = await firmwareImages(hypervisor);
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  await expect(field(page, "Boot Mode")).toBeVisible();
  await expect(field(page, "Boot Mode")).toHaveValue("firmware");
  await expect(field(page, "UEFI Firmware")).toHaveValue(newest.id);
  await expect(page.getByTestId("firmware-hint")).toContainText("built for this hypervisor");
  await expect(page.getByText("Login Credential")).toBeVisible();
  if (((await api<{ networks?: unknown[] }>("/networks")).networks ?? []).length > 0) {
    await expect(page.getByText("Networks", { exact: true })).toBeVisible();
  }

  await field(page, "Boot Mode").selectOption("kernel");
  await expect(page.getByTestId("kernel-image")).toBeVisible();
  await expect(page.getByText("UEFI Firmware", { exact: true })).toBeHidden();
  // A kernel image boots a seeded cloud image: still asks for a login.
  await expect(page.getByText("Login Credential")).toBeVisible();

  // Kernel files on the host and a disk path: nothing to seed.
  await field(page, "Kernel").selectOption("path");
  await expect(page.getByText("Kernel Image Path")).toBeVisible();
  await expect(page.getByText("Initramfs Path (optional)")).toBeVisible();
  await field(page, "Boot Disk").selectOption("path");
  await expect(page.getByText("Login Credential")).toBeHidden();
});

test("keeps the boot options under Advanced options until one needs a choice", async ({ page, hypervisor }) => {
  const details = page.getByTestId("advanced-options");
  const isOpen = () => details.evaluate((d) => (d as HTMLDetailsElement).open);
  await page.reload();
  await page.getByRole("button", { name: "+ Create VM" }).click();
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  const [newest] = await firmwareImages(hypervisor);
  // Closed, saying what it holds.
  await expect(details.locator("summary")).toContainText(`UEFI firmware (${newest.name})`);
  await expect.poll(isOpen).toBe(false);
  await expect(page.getByText("Boot Mode", { exact: true })).toBeHidden();

  // No firmware image to boot through: opened, so the missing choice shows.
  await rpcRoute(page, "glidex.storage.v1.ImageService/ListImages", () => ({ message: { images: [] } }));
  await page.reload();
  await page.getByRole("button", { name: "+ Create VM" }).click();
  await expect.poll(isOpen).toBe(true);
  await expect(page.getByTestId("firmware-hint")).toContainText("Pull");
});

test("preselects only the chosen image's own kernel", async ({ page }) => {
  // Two disk images, and the kernel taken from one of them.
  const ready = { state: "IMAGE_STATE_READY" };
  const disk = (id: string) => ({ id, name: id, kind: "IMAGE_KIND_DISK", arch: "ARCH_X86_64", status: ready, virtual_size_bytes: "1073741824" });
  await rpcRoute(page, "glidex.storage.v1.ImageService/ListImages", () => ({
    message: {
      images: [
        disk("beta"),
        disk("alpha"),
        { id: "alpha-kernel", name: "alpha-kernel", kind: "IMAGE_KIND_KERNEL", arch: "ARCH_X86_64", status: ready, for_image: "alpha", kernel: { release: "6.1.0", cmdline: "root=LABEL=alpha ro console=ttyS0" } },
      ],
    },
  }));
  await page.reload();
  await page.getByRole("button", { name: "+ Create VM" }).click();
  await showAdvanced(page);
  await field(page, "Boot Mode").selectOption("kernel");
  const kernel = page.getByTestId("kernel-image");

  // beta has no kernel of its own: none is chosen for it.
  await expect(field(page, "Image")).toHaveValue("beta");
  await expect(kernel).toHaveValue("");
  await expect(page.getByTestId("kernel-hint")).toContainText("No kernel from beta yet");
  await expect(kernel.locator("option", { hasText: "alpha-kernel" })).toContainText("from alpha");

  // alpha's own kernel is.
  await field(page, "Image").selectOption("alpha");
  await expect(kernel).toHaveValue("alpha-kernel");
  await expect(page.getByTestId("kernel-mismatch")).toBeHidden();

  // Picking another image's kernel on purpose warns.
  await field(page, "Image").selectOption("beta");
  await expect(kernel).toHaveValue("");
  await kernel.selectOption("alpha-kernel");
  await expect(page.getByTestId("kernel-mismatch")).toContainText("taken from alpha");
});

test("offers kernels and firmware of the image's architecture", async ({ page, hypervisor }) => {
  const ready = { state: "IMAGE_STATE_READY" };
  const [X86, ARM] = ["ARCH_X86_64", "ARCH_AARCH64"];
  const disk = (id: string, arch: string) => ({ id, name: id, kind: "IMAGE_KIND_DISK", arch, status: ready, virtual_size_bytes: "1073741824" });
  const kernel = (id: string, arch: string, forImage: string) => ({ id, name: id, kind: "IMAGE_KIND_KERNEL", arch, status: ready, for_image: forImage, kernel: { release: "7.0.0" } });
  const fw = (id: string, arch: string) => ({ id, name: id, kind: "IMAGE_KIND_FIRMWARE", arch, hypervisor: PROTO_HYPERVISOR[hypervisor], status: ready, created_at: "2026-10-01T00:00:00Z" });
  await rpcRoute(page, "glidex.storage.v1.ImageService/ListImages", () => ({
    message: {
      images: [
        disk("intel", X86),
        disk("arm", ARM),
        kernel("intel-kernel", X86, "intel"),
        kernel("arm-kernel", ARM, "arm"),
        fw("fw-x86", X86),
        fw("fw-arm", ARM),
      ],
    },
  }));
  await page.reload();
  await page.getByRole("button", { name: "+ Create VM" }).click();
  await showAdvanced(page);
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  const firmware = field(page, "UEFI Firmware");
  const offered = (select: typeof firmware) =>
    select.locator("option").evaluateAll((os) => os.map((o) => (o as HTMLOptionElement).value));

  // An x86_64 image: x86_64 firmware only.
  await expect(field(page, "Image")).toHaveValue("intel");
  await expect(firmware).toHaveValue("fw-x86");
  expect(await offered(firmware)).toEqual(["fw-x86"]);

  // An aarch64 one: its firmware, chosen for it.
  await field(page, "Image").selectOption("arm");
  await expect(firmware).toHaveValue("fw-arm");
  expect(await offered(firmware)).toEqual(["fw-arm"]);
  await expect(page.getByTestId("firmware-hint")).toContainText("aarch64");

  // Its kernels too.
  await field(page, "Boot Mode").selectOption("kernel");
  const kernels = page.getByTestId("kernel-image");
  await expect(kernels).toHaveValue("arm-kernel");
  expect(await offered(kernels)).toEqual(["arm-kernel"]);
  await field(page, "Image").selectOption("intel");
  await expect(kernels).toHaveValue("intel-kernel");
  expect(await offered(kernels)).toEqual(["intel-kernel"]);
});

test("offers only firmware built for the chosen hypervisor", async ({ page, hypervisor }) => {
  const hv = field(page, "Hypervisor Backend");
  const firmware = field(page, "UEFI Firmware");
  const others = await firmwareImages(other(hypervisor));
  await hv.selectOption(other(hypervisor));
  if (others.length > 0) {
    await expect(firmware).toHaveValue(others[0].id);
  } else {
    // None: the form says which catalog entry to pull, and won't submit.
    await expect(firmware).toHaveValue("");
    await expect(page.getByTestId("firmware-hint")).toContainText(FIRMWARE_KEY[other(hypervisor)]);
  }
  const mine = (await firmwareImages(hypervisor)).map((i) => i.id);
  await hv.selectOption(hypervisor);
  await expect(firmware).toHaveValue(mine[0]);
  const offered = await firmware.locator("option").evaluateAll((os) => os.map((o) => (o as HTMLOptionElement).value));
  expect(offered).toEqual(mine);
});

test("sizes an elastic Pylon VM at its max vCPUs", async ({ page }) => {
  const vcpus = field(page, "vCPU Count");
  const elastic = page.getByLabel("Elastic CPU scaling (min/max vCPUs, weight)");
  await vcpus.fill("2");
  await elastic.check();
  // Not Pylon: the guest boots with vcpu_count, which stays the user's.
  await expect(vcpus).toHaveValue("2");
  await expect(vcpus).toBeEnabled();

  // Pylon boots all max_vcpus (spec/30-pylon.md P2): the count follows max.
  await field(page, "Hypervisor Backend").selectOption("pylon");
  await expect(field(page, "Max vCPUs")).toHaveValue("4");
  await expect(vcpus).toHaveValue("4");
  await expect(vcpus).toBeDisabled();
  await field(page, "Max vCPUs").fill("6");
  await expect(vcpus).toHaveValue("6");

  // Checked with Pylon already chosen.
  await elastic.uncheck();
  await expect(vcpus).toBeEnabled();
  await elastic.check();
  await expect(vcpus).toHaveValue("6");
});

test.afterEach(async ({ hypervisor }) => {
  const vm = (await listVms()).find((v) => v.name === `form-${hypervisor}`);
  // Wait: a VM that was started takes a moment to tear down, and the next
  // test reuses its name.
  if (vm) await del(`/vms/${vm.id}?wait=60`);
});

test("submits hypervisor, firmware and networks", async ({ page, hypervisor }) => {
  const networks = (await api<{ networks?: { id: string; name: string }[] }>("/networks")).networks ?? [];
  await field(page, "VM Name").fill(`form-${hypervisor}`);
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  await field(page, "Boot Disk").selectOption("path");
  await field(page, "Disk Image Path").fill("/nonexistent/disk.img");

  const [body] = await Promise.all([
    waitForRpc(page, "glidex.vm.v1.VmService/CreateVm"),
    page.getByRole("button", { name: "Create VM", exact: true }).click(),
  ]);
  expect(body.firmware_path).toBeUndefined();
  // proto JSON leaves an empty string out.
  expect(body.kernel_image_path ?? "").toBe("");
  expect(body).toMatchObject({
    name: `form-${hypervisor}`,
    hypervisor: PROTO_HYPERVISOR[hypervisor],
    firmware: (await firmwareImages(hypervisor))[0].id,
    rootfs_path: "/nonexistent/disk.img",
    // The form's default size.
    mem_size_mib: 1024,
    restart_policy: "RESTART_POLICY_ON_FAILURE",
    on_host_boot: "HOST_BOOT_POLICY_RESUME",
  });
  // Created stopped unless asked to start.
  expect(body.power).toBeUndefined();
  // The `default` network, if there is one, is preselected.
  // It is sent by id (a project network and a host network may share a name).
  const defaults = networks.filter((n) => n.name === "default").map((n) => n.id);
  if (defaults.length) {
    expect(body.networks).toHaveLength(1);
    expect(defaults).toContain(body.networks[0].network);
  }

  // The control plane accepted it; the dashboard lists it with its hypervisor.
  await expect(vmCard(page, `form-${hypervisor}`)).toContainText(LABEL[hypervisor]);
  const vm = (await listVms()).find((v) => v.name === `form-${hypervisor}`);
  expect(vm?.hypervisor).toBe(PROTO_HYPERVISOR[hypervisor]);
});

test("submits kernel files on the host", async ({ page, hypervisor }) => {
  await field(page, "VM Name").fill(`form-${hypervisor}`);
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  await field(page, "Boot Mode").selectOption("kernel");
  await field(page, "Kernel").selectOption("path");
  await field(page, "Kernel Image Path").fill("/nonexistent/vmlinuz");
  await field(page, "Initramfs Path (optional)").fill("/nonexistent/initrd.img");
  await field(page, "Boot Disk").selectOption("path");
  await field(page, "Disk Image Path").fill("/nonexistent/disk.img");
  await page.getByTestId("kernel-args").fill("console=ttyS0 root=/dev/vda");

  const [body] = await Promise.all([
    waitForRpc(page, "glidex.vm.v1.VmService/CreateVm"),
    page.getByRole("button", { name: "Create VM", exact: true }).click(),
  ]);
  expect(body).toMatchObject({
    kernel_image_path: "/nonexistent/vmlinuz",
    initrd_path: "/nonexistent/initrd.img",
    rootfs_path: "/nonexistent/disk.img",
    kernel_args: "console=ttyS0 root=/dev/vda",
  });
  expect(body.firmware).toBeUndefined();
  expect(body.kernel).toBeUndefined();
  expect(body.kernel_from_disk).toBeUndefined();
  await expect(vmCard(page, `form-${hypervisor}`)).toBeVisible();
});

test("starts it now with the chosen restart behaviour", async ({ page, hypervisor }) => {
  await field(page, "VM Name").fill(`form-${hypervisor}`);
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  await field(page, "Boot Disk").selectOption("path");
  await field(page, "Disk Image Path").fill("/nonexistent/disk.img");
  await field(page, "If It Crashes").selectOption("never");
  await field(page, "After a Host Reboot").selectOption("stop");
  await page.getByLabel("Start it now").check();

  const [request] = await Promise.all([
    waitForRpc(page, "glidex.vm.v1.VmService/CreateVm"),
    page.getByRole("button", { name: "Create VM", exact: true }).click(),
  ]);
  expect(request).toMatchObject({ power: "POWER_STATE_RUNNING", restart_policy: "RESTART_POLICY_NEVER", on_host_boot: "HOST_BOOT_POLICY_STOP" });
  await expect(vmCard(page, `form-${hypervisor}`)).toBeVisible();
  const vm = (await listVms()).find((v) => v.name === `form-${hypervisor}`);
  expect(vm).toMatchObject({ desired_state: "POWER_STATE_RUNNING", restart_policy: "RESTART_POLICY_NEVER", on_host_boot: "HOST_BOOT_POLICY_STOP" });
});

test.describe("refused", () => {
  // The browser logs the 409 the test asks for.
  test.use({ ignoredConsoleErrors: /status of 409/ });

  test("a refused create keeps the form open with the reason", async ({ page, hypervisor }) => {
    // Taken: the second create is refused (409).
    const taken = await createVm({ name: `form-${hypervisor}`, vcpu_count: 1, mem_size_mib: 256, kernel_image_path: "/nonexistent/vmlinux", rootfs_path: "/nonexistent/disk.img" });
    expect(taken.id, JSON.stringify(taken)).toBeTruthy();
    await field(page, "VM Name").fill(`form-${hypervisor}`);
    await field(page, "Boot Disk").selectOption("path");
    await field(page, "Disk Image Path").fill("/nonexistent/disk.img");
    const create = page.getByRole("button", { name: "Create VM", exact: true });
    await create.click();
    await expect(page.getByText(/already exists/)).toBeVisible();
    // Still there, and usable again.
    await expect(field(page, "VM Name")).toHaveValue(`form-${hypervisor}`);
    await expect(create).toBeEnabled();
  });
});

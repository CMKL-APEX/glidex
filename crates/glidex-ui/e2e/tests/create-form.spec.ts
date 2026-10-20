// CreateVmForm per hypervisor: no VM is booted, so no KVM or image needed.
import { api, createVm, del, ensureFirmware, expect, field, firmwareImages, FIRMWARE_KEY, LABEL, listVms, PROTO_HYPERVISOR, test, vmCard, waitForRpc, type Hypervisor } from "../fixtures";

const other = (hv: Hypervisor): Hypervisor => (hv === "qemu" ? "cloudhypervisor" : "qemu");

// The form offers firmware images; make sure there is one to pick.
test.beforeAll(async ({ hypervisor }) => {
  await ensureFirmware(hypervisor);
});

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "+ Create VM" }).click();
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

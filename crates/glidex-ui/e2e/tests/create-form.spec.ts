// CreateVmForm per hypervisor: no VM is booted, so no KVM or image needed.
import { api, DEFAULT_FIRMWARE, del, expect, field, FIRMWARE_HINT, LABEL, test, vmCard, type Hypervisor } from "../fixtures";

const other = (hv: Hypervisor): Hypervisor => (hv === "qemu" ? "cloudhypervisor" : "qemu");

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "+ Create VM" }).click();
});

test("offers firmware boot with the hypervisor's own firmware", async ({ page, hypervisor }) => {
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  await expect(field(page, "Boot Mode")).toBeVisible();
  await expect(field(page, "Boot Mode")).toHaveValue("firmware");
  await expect(field(page, "UEFI Firmware Path")).toHaveValue(DEFAULT_FIRMWARE[hypervisor]);
  await expect(page.getByText(FIRMWARE_HINT[hypervisor])).toBeVisible();
  await expect(page.getByText("Login Credential")).toBeVisible();
  if ((await api<unknown[]>("/networks")).length > 0) {
    await expect(page.getByText("Networks", { exact: true })).toBeVisible();
  }

  await field(page, "Boot Mode").selectOption("kernel");
  await expect(page.getByText("Kernel Image Path")).toBeVisible();
  await expect(page.getByText("UEFI Firmware Path")).toBeHidden();
  await expect(page.getByText("Login Credential")).toBeHidden();
});

test("follows the hypervisor's default firmware unless one was typed", async ({ page, hypervisor }) => {
  const hv = field(page, "Hypervisor Backend");
  const firmware = field(page, "UEFI Firmware Path");
  await hv.selectOption(other(hypervisor));
  await hv.selectOption(hypervisor);
  await expect(firmware).toHaveValue(DEFAULT_FIRMWARE[hypervisor]);
  await hv.selectOption(other(hypervisor));
  await expect(firmware).toHaveValue(DEFAULT_FIRMWARE[other(hypervisor)]);

  await firmware.fill("/custom/fw.fd");
  await hv.selectOption(hypervisor);
  await expect(firmware).toHaveValue("/custom/fw.fd");
});

test.afterEach(async ({ hypervisor }) => {
  const vm = (await api<any[]>("/vms")).find((v) => v.name === `form-${hypervisor}`);
  // Wait: a VM that was started takes a moment to tear down, and the next
  // test reuses its name.
  if (vm) await del(`/vms/${vm.id}?wait=60`);
});

test("submits hypervisor, firmware and networks", async ({ page, hypervisor }) => {
  const networks = await api<{ name: string }[]>("/networks");
  await field(page, "VM Name").fill(`form-${hypervisor}`);
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  await field(page, "Boot Disk").selectOption("path");
  await field(page, "Disk Image Path").fill("/nonexistent/disk.img");

  const [request] = await Promise.all([
    page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/vms")),
    page.getByRole("button", { name: "Create VM", exact: true }).click(),
  ]);
  const body = request.postDataJSON();
  expect(body).toMatchObject({
    name: `form-${hypervisor}`,
    hypervisor,
    firmware_path: DEFAULT_FIRMWARE[hypervisor],
    kernel_image_path: "",
    rootfs_path: "/nonexistent/disk.img",
    restart_policy: "on_failure",
    on_host_boot: "resume",
  });
  // Created stopped unless asked to start.
  expect(body.power).toBeUndefined();
  // The `default` network, if there is one, is preselected.
  if (networks.some((n) => n.name === "default")) {
    expect(body.networks).toEqual([{ network: "default" }]);
  }

  // The control plane accepted it; the dashboard lists it with its hypervisor.
  await expect(vmCard(page, `form-${hypervisor}`)).toContainText(LABEL[hypervisor]);
  const vm = (await api<any[]>("/vms")).find((v) => v.name === `form-${hypervisor}`);
  expect(vm?.hypervisor).toBe(hypervisor);
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
    page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/vms")),
    page.getByRole("button", { name: "Create VM", exact: true }).click(),
  ]);
  expect(request.postDataJSON()).toMatchObject({ power: "running", restart_policy: "never", on_host_boot: "stop" });
  await expect(vmCard(page, `form-${hypervisor}`)).toBeVisible();
  const vm = (await api<any[]>("/vms")).find((v) => v.name === `form-${hypervisor}`);
  expect(vm).toMatchObject({ desired_state: "running", restart_policy: "never", on_host_boot: "stop" });
});

test.describe("refused", () => {
  // The browser logs the 409 the test asks for.
  test.use({ ignoredConsoleErrors: /status of 409/ });

  test("a refused create keeps the form open with the reason", async ({ page, hypervisor }) => {
    // Taken: the second create is refused (409).
    const taken = await api("/vms", {
      method: "POST",
      body: { name: `form-${hypervisor}`, vcpu_count: 1, mem_size_mib: 256, kernel_image_path: "/nonexistent/vmlinux", rootfs_path: "/nonexistent/disk.img" },
    });
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

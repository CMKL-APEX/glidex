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
  await expect(page.getByText("Login Credential (optional)")).toBeVisible();
  if ((await api<unknown[]>("/networks")).length > 0) {
    await expect(page.getByText("Networks", { exact: true })).toBeVisible();
  }

  await field(page, "Boot Mode").selectOption("kernel");
  await expect(page.getByText("Kernel Image Path")).toBeVisible();
  await expect(page.getByText("UEFI Firmware Path")).toBeHidden();
  await expect(page.getByText("Login Credential (optional)")).toBeHidden();
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
  if (vm) await del(`/vms/${vm.id}`);
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
  });
  // The `default` network, if there is one, is preselected.
  if (networks.some((n) => n.name === "default")) {
    expect(body.networks).toEqual([{ network: "default" }]);
  }

  // The control plane accepted it; the dashboard lists it with its hypervisor.
  await expect(vmCard(page, `form-${hypervisor}`)).toContainText(LABEL[hypervisor]);
  const vm = (await api<any[]>("/vms")).find((v) => v.name === `form-${hypervisor}`);
  expect(vm?.hypervisor).toBe(hypervisor);
});

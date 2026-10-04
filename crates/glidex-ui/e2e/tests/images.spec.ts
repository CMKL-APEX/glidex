// The Images page: the cloud image and firmware catalogs, and the
// downloaded images split by kind. Nothing is booted.
import { api, del, ensureFirmware, expect, field, FIRMWARE_KEY, LABEL, test } from "../fixtures";

test("lists the firmware catalog and the firmware images", async ({ page, hypervisor }) => {
  const fw = await ensureFirmware(hypervisor);
  await page.goto("/images");
  await expect(page.getByRole("heading", { name: "Cloud image catalog" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Firmware catalog" })).toBeVisible();

  const catalog = page.getByTestId("firmware-catalog");
  for (const entry of await api<any[]>("/images/firmware-catalog")) {
    await expect(catalog).toContainText(entry.key);
  }
  // The pulled entry reads as downloaded.
  // By the exact key: "ovmf" is also a prefix of "ovmf-debian".
  const card = catalog.locator("> div").filter({ has: page.locator("span.font-mono", { hasText: new RegExp(`^${FIRMWARE_KEY[hypervisor]}$`) }) });
  if (fw.source?.key === FIRMWARE_KEY[hypervisor]) await expect(card).toContainText(/Downloaded|Imported/);

  // Firmware sits in its own table, with its hypervisor, not among the cloud images.
  const row = page.getByTestId("firmware-images").locator("tr").filter({ hasText: fw.name });
  await expect(row).toContainText(LABEL[hypervisor]);
  await expect(row).toContainText("Ready");
  await expect(page.getByTestId("disk-images").getByText(fw.name, { exact: true })).toHaveCount(0);
});

test("the disk form never offers firmware as a source", async ({ page, hypervisor }) => {
  const fw = await ensureFirmware(hypervisor);
  await page.goto("/disks");
  await page.getByRole("button", { name: "Create Disk" }).click();
  const contents = field(page, "Contents");
  const offered = await contents.locator("option").allTextContents();
  expect(offered.join("|")).not.toContain(fw.name);
});

test("the VM page names its firmware image", async ({ page, hypervisor }) => {
  const fw = await ensureFirmware(hypervisor);
  const vm = await api("/vms", {
    method: "POST",
    body: { name: `fw-${hypervisor}`, project: "default", vcpu_count: 1, mem_size_mib: 256, hypervisor, kernel_image_path: "",
      firmware: fw.id, rootfs_path: "/nonexistent/disk.img" },
  });
  expect(vm.firmware, JSON.stringify(vm)).toBe(fw.id);
  try {
    await page.goto(`/vms/${vm.id}`);
    await expect(page.getByTestId("vm-firmware")).toHaveText(fw.name);
    if (process.env.UI_SHOTS) {
      await page.screenshot({ path: `${process.env.UI_SHOTS}/vm-${hypervisor}.png`, fullPage: true });
      await page.goto("/images");
      await expect(page.getByTestId("firmware-images")).toBeVisible();
      await page.screenshot({ path: `${process.env.UI_SHOTS}/images-${hypervisor}.png`, fullPage: true });
      await page.goto("/");
      await page.getByRole("button", { name: "+ Create VM" }).click();
      await field(page, "Hypervisor Backend").selectOption(hypervisor);
      await page.screenshot({ path: `${process.env.UI_SHOTS}/create-${hypervisor}.png`, fullPage: true });
    }
  } finally {
    await del(`/vms/${vm.id}?wait=60`);
  }
});

test("Download from URL asks for a variable store for QEMU firmware only", async ({ page }) => {
  await page.goto("/images");
  await expect(page.getByTestId("firmware-catalog")).toContainText("Debian package");
  await page.getByRole("button", { name: "Download from URL" }).click();
  await expect(page.getByText("Variable store URL (optional)")).toBeHidden();
  await field(page, "Type").selectOption("firmware");
  await field(page, "For hypervisor").selectOption("qemu");
  await expect(page.getByText("Variable store URL (optional)")).toBeVisible();
  // The digest field appears once there is a URL.
  await expect(page.getByText("Variable store sha256 (optional)")).toBeHidden();
  await field(page, "Variable store URL (optional)").fill("https://example.com/OVMF_VARS.fd");
  await expect(page.getByText("Variable store sha256 (optional)")).toBeVisible();
  if (process.env.UI_SHOTS) await page.screenshot({ path: `${process.env.UI_SHOTS}/url-form.png` });
  await field(page, "For hypervisor").selectOption("cloudhypervisor");
  await expect(page.getByText("Variable store URL (optional)")).toBeHidden();
});

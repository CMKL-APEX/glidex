// The Images page: the cloud image and firmware catalogs, and the
// downloaded images split by kind. Nothing is booted.
import { api, ensureFirmware, expect, FIRMWARE_KEY, LABEL, test } from "../fixtures";

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
  const card = catalog.locator("> div").filter({ hasText: FIRMWARE_KEY[hypervisor] });
  if (fw.source?.key === FIRMWARE_KEY[hypervisor]) await expect(card).toContainText("Downloaded");

  // Firmware sits in its own table, with its hypervisor, not among the cloud images.
  const row = page.getByTestId("firmware-images").locator("tr").filter({ hasText: fw.name });
  await expect(row).toContainText(LABEL[hypervisor]);
  await expect(row).toContainText("Ready");
  await expect(page.getByTestId("disk-images").getByText(fw.name, { exact: true })).toHaveCount(0);
});

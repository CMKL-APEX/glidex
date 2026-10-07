// Usage page (spec/metering.md §12): tabs, filters and the CSV export.
import { api, expect, test } from "../fixtures";

test("the Usage page shows each tab and exports CSV", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("link", { name: "Usage", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Usage" })).toBeVisible();
  // Totals for the month, then bandwidth and disk I/O with their p95 columns.
  await expect(page.getByText(/Complete through|No hour closed yet/)).toBeVisible();
  await page.getByRole("tab", { name: "Bandwidth" }).click();
  await expect(page.getByText(/Month to date|Final|Nothing recorded/).first()).toBeVisible();
  await page.getByRole("tab", { name: "Disk I/O" }).click();
  await expect(page.getByRole("option", { name: "disk" })).toHaveCount(1);
  await expect(page.getByRole("option", { name: "nic" })).toHaveCount(0);

  // The CSV link downloads with the session cookie.
  const [download] = await Promise.all([page.waitForEvent("download"), page.getByRole("link", { name: "Download CSV" }).click()]);
  expect(download.suggestedFilename()).toMatch(/^glidex-disk-io-\d{4}-\d{2}\.csv$/);
});

test("the usage API answers for the e2e user", async () => {
  const u = await api<{ rows: unknown[]; timezone: string }>("/usage?granularity=month");
  expect(Array.isArray(u.rows)).toBe(true);
  expect(u.timezone).toBeTruthy();
});

// Self-service tenants (spec/19-tenancy.md §11.1, T9) as a directory
// administrator: the directory's settings and its label for a tenant (D81).
import { api, expect, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /status of 4\d\d/ });

const DEFAULTS = { settings: {} };

async function cleanup() {
  await api("/directory/settings", { method: "PUT", body: DEFAULTS });
}

test.beforeAll(cleanup);
test.afterAll(cleanup);

test("directory settings: the label renames tenants in the UI, and the self-service settings are kept", async ({ page }) => {
  await api("/directory/settings", { method: "PUT", body: DEFAULTS });
  await page.goto("/directory/settings");
  const form = page.getByTestId("directory-settings");
  await expect(form.getByLabel("One", { exact: true })).toHaveValue("Tenant");
  await expect(form.getByLabel("Per user", { exact: true })).toHaveValue("3");

  await form.getByLabel("One", { exact: true }).fill("Lab");
  await form.getByLabel("Many", { exact: true }).fill("Labs");
  await page.locator("#self-service-enabled").check();
  await form.getByLabel("Per user", { exact: true }).fill("2");
  await form.getByLabel("In all (empty: no limit)").fill("10");
  await form.getByLabel("Slug prefix").fill("lab-");
  await page.locator("#self-service-approval").check();
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByText("Saved.")).toBeVisible();

  const { settings } = await api("/directory/settings");
  expect(settings.tenant_label).toEqual({ singular: "Lab", plural: "Labs" });
  expect(settings.self_service).toMatchObject({
    enabled: true,
    max_per_user: 2,
    max_total: 10,
    slug_prefix: "lab-",
    approval_required: true,
    admins_may_delete: true,
  });
  expect((await api("/auth/whoami")).tenant_label).toBe("Lab");

  // The directory's pages use the label.
  await expect(page.getByRole("navigation", { name: "Directory" }).getByRole("link", { name: "Labs" })).toBeVisible();
  await page.goto("/directory/tenants");
  await expect(page.getByRole("heading", { name: "Labs" })).toBeVisible();
  await expect(page.getByRole("button", { name: "+ Create Lab" })).toBeVisible();

  // The server refuses a bad value, and nothing changes.
  const bad = await api("/directory/settings", {
    method: "PUT",
    body: { settings: { ...settings, self_service: { ...settings.self_service, slug_prefix: "Lab" } } },
  });
  expect(bad.error).toBe("invalid");
  expect((await api("/directory/settings")).settings.self_service.slug_prefix).toBe("lab-");
});


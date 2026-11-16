// The Audit page shows the newest entries first, a hundred to a page, and
// sorts by a column header (spec/11-security.md decisions 22, 23). "All"
// says what it covers. No VM is booted.
import { api, del, expect, test } from "../fixtures";

const PREFIX = "e2e-audit-page";

async function cleanup() {
  for (const p of ((await api<{ projects?: any[] }>("/projects")).projects ?? []).filter((x) => x.name.startsWith(PREFIX)))
    await del(`/projects/${p.id}`);
}

test.beforeAll(cleanup);
test.afterAll(cleanup);

test("the audit: newest first, paged, sorted by a column", async ({ page }) => {
  // 55 projects made and deleted: 110 entries, more than a page.
  for (let i = 0; i < 55; i++) {
    const p = (await api("/projects", { method: "POST", body: { name: `${PREFIX}-${i}` } })).project;
    await del(`/projects/${p.id}`);
  }

  await page.goto("/audit");
  await expect(page.getByRole("heading", { name: "Audit log" })).toBeVisible();
  // The e2e admin is of the host's own directory: All is the cluster's.
  await expect(page.locator("#audit-project")).toHaveValue("");
  await expect(page.locator("#audit-project option").first()).toHaveText("All (cluster)");

  // Newest first: the last write, the last project's delete, heads the page.
  const rows = page.locator("tbody tr");
  await expect(rows.first().locator("td").nth(2)).toHaveText("deleteProject");
  await expect(rows).toHaveCount(100);
  await expect(page.getByRole("button", { name: "Newer" })).toBeDisabled();
  await page.getByRole("button", { name: "Older" }).click();
  await expect(page.getByText("Page 2")).toBeVisible();
  await expect(page.getByRole("button", { name: "Newer" })).toBeEnabled();
  await page.getByRole("button", { name: "Newer" }).click();
  await expect(page.getByText("Page 1")).toBeVisible();

  // A header sorts A–Z, and again Z–A.
  const action = page.getByRole("columnheader", { name: /Action/ });
  await action.getByRole("button").click();
  await expect(action).toHaveAttribute("aria-sort", "ascending");
  const actions = async () => rows.locator("td:nth-child(3)").allTextContents();
  await expect.poll(async () => {
    const a = await actions();
    return a.length > 1 && a.every((x, i) => i === 0 || a[i - 1] <= x);
  }).toBe(true);
  await action.getByRole("button").click();
  await expect(action).toHaveAttribute("aria-sort", "descending");
  await expect.poll(async () => {
    const a = await actions();
    return a.length > 1 && a.every((x, i) => i === 0 || a[i - 1] >= x);
  }).toBe(true);
});

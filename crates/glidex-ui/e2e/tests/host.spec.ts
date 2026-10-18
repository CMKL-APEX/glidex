// A standalone host is one node (`local`): the Cluster page says so and
// leads to its node page, which a cluster node has too.
import { expect, test } from "../fixtures";

test("a standalone host shows its node and its page", async ({ page }) => {
  await page.goto("/cluster");
  await expect(page.getByText("This host is standalone.")).toBeVisible();
  const card = page.getByTestId("this-host");
  await expect(card).toContainText(/CPUs/);
  await card.getByRole("link", { name: "Open this host's page" }).click();
  await expect(page).toHaveURL(/\/cluster\/nodes\/[^/]+$/);
  await expect(page.getByRole("heading", { name: "Capacity" })).toBeVisible();
  // A standalone host isn't removed or detached from a cluster it isn't in.
  await expect(page.getByRole("button", { name: /^Remove/ })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /^Detach/ })).toHaveCount(0);
});

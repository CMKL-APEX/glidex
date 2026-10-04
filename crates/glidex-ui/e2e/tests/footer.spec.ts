// The footer shows which build of the UI this is.
import { expect, test } from "../fixtures";

test("shows the build in the footer", async ({ page }) => {
  await page.goto("/");
  const footer = page.getByTestId("build-info");
  await expect(footer).toBeVisible();
  await expect(footer).toContainText(/Commit [0-9a-f]{7,}/);
  await expect(footer).toContainText("Branch");
});

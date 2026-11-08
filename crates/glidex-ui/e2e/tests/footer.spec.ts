// The footer shows which build of the UI this is, linked to its code.
import { expect, test } from "../fixtures";

test("shows the build in the footer, linked to its code on GitHub", async ({ page }) => {
  await page.goto("/");
  const footer = page.getByTestId("build-info");
  await expect(footer).toBeVisible();
  await expect(footer).toContainText(/Commit [0-9a-f]{7,}/);
  await expect(footer).toContainText("Branch");
  // The commit opens on GitHub, in a new tab.
  const commit = footer.getByRole("link", { name: /^[0-9a-f]{7,}$/ });
  await expect(commit).toHaveAttribute("href", /^https:\/\/github\.com\/[^/]+\/[^/]+\/commit\/[0-9a-f]{7,40}$/);
  await expect(commit).toHaveAttribute("target", "_blank");
  // The release: its tag, or the release before and the commits since.
  const version = footer.getByRole("link", { name: /^v\d+\.\d+\.\d+(\+\d+)?$/ });
  await expect(version).toHaveAttribute("href", /\/(releases\/tag\/v[\d.]+|compare\/v[\d.]+\.\.\.[0-9a-f]{7,40})$/);
});

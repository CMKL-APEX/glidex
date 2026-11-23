// A page loaded before an upgrade asks for the previous build's files; a
// lazily loaded page (or the console's terminal) whose file is gone
// reloads the page once, which loads the new build (src/staleBuild.ts).
import { expect, test, UI_SERVER_PORT } from "../fixtures";

// The built UI, served as installed: the dev server has no build files.
const UI_SERVER = `http://localhost:${UI_SERVER_PORT}`;

test.use({ ignoredConsoleErrors: /status of 404|dynamically imported module|Tokens-/ });

/** The page's own errors from the missing file, removed once checked: a
 * lazy page that can't load throws while the page reloads. */
function takeMissingFileErrors(errors: string[]) {
  for (const e of errors) expect(e).toMatch(/dynamically imported module|Tokens-|lazy/);
  errors.splice(0);
}

test("a page's missing file reloads the page once, and the page then loads", async ({ page, pageErrors }) => {
  let missing = 1;
  let loads = 0;
  page.on("load", () => loads++);
  await page.route("**/assets/Tokens-*.js", (route) =>
    missing-- > 0 ? route.fulfill({ status: 404, body: "" }) : route.continue(),
  );
  await page.goto(`${UI_SERVER}/tokens`);
  await expect(page.getByRole("heading", { name: "Access Tokens" })).toBeVisible();
  expect(missing).toBeLessThan(0);
  expect(loads).toBe(2);
  takeMissingFileErrors(pageErrors);
});

test("a file missing after the reload too doesn't reload the page again", async ({ page, pageErrors }) => {
  let asked = 0;
  let loads = 0;
  page.on("load", () => loads++);
  await page.route("**/assets/Tokens-*.js", (route) => {
    asked++;
    return route.fulfill({ status: 404, body: "" });
  });
  await page.goto(`${UI_SERVER}/tokens`);
  await expect.poll(() => loads).toBe(2);
  await expect.poll(() => asked).toBeGreaterThanOrEqual(2);
  // Time for another reload, were there one.
  await page.waitForTimeout(3000);
  expect(loads).toBe(2);
  await expect(page.getByRole("heading", { name: "Access Tokens" })).toHaveCount(0);
  takeMissingFileErrors(pageErrors);
});

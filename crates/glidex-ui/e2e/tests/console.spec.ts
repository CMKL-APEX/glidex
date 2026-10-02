// VmConsole lifecycle, independent of any hypervisor: xterm 5.5 threw
// "reading 'dimensions'" when a terminal was disposed right after open()
// (React StrictMode's double mount in dev, or leaving the page at once).
// The `pageErrors` fixture fails the test on any such error.
import { expect, test } from "../fixtures";

// There is no such VM: the console WebSocket is refused and its detail
// page gets 404s.
test.use({ ignoredConsoleErrors: /WebSocket|status of 404/ });

test("opening the console page leaves no errors behind", async ({ page }) => {
  await page.goto("/vms/no-such-vm/console");
  await expect(page.locator(".xterm")).toHaveCount(1);
  // Let xterm's queued timers run.
  await page.waitForTimeout(1_000);
});

test("leaving the console page at once leaves no errors behind", async ({ page }) => {
  for (let i = 0; i < 3; i++) {
    await page.goto("/vms/no-such-vm/console");
    await page.getByRole("link", { name: "Back to VM" }).click();
  }
  await page.waitForTimeout(1_000);
});

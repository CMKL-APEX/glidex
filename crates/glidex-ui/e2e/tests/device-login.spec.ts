// A directory's device login (spec/19-tenancy.md D54, D57): gxctl asks the
// directory for a code, the signed-in user approves it on the web UI's
// /device page, and gxctl's next poll gets its tokens for this cluster.
import { api, expect, test } from "../fixtures";

test("a device login is approved on the device page", async ({ page }) => {
  const start = await api("/d/local/device_authorization", { method: "POST", body: { client_id: "gxctl" } });
  expect(start.user_code, JSON.stringify(start)).toMatch(/^[A-Z]{4}-[A-Z]{4}$/);
  expect(start.verification_uri_complete).toContain(`/device?directory=local&code=${start.user_code}`);
  const poll = () =>
    api("/d/local/token", {
      method: "POST",
      body: { grant_type: "urn:ietf:params:oauth:grant-type:device_code", client_id: "gxctl", device_code: start.device_code },
    });
  expect((await poll()).error).toBe("authorization_pending");

  await page.goto(`/device?directory=local&code=${start.user_code}`);
  await expect(page.getByText(/gxctl \(command line\) asks to sign in as/)).toBeVisible();
  await page.getByRole("button", { name: "Approve" }).click();
  await expect(page.getByTestId("device-done")).toContainText("Approved");

  await expect.poll(async () => (await poll()).refresh_token ?? "", { timeout: 15_000, intervals: [6_000] }).toMatch(/^gxr_/);
});

test("a code typed by hand, and a denied login", async ({ page }) => {
  const start = await api("/d/local/device_authorization", { method: "POST", body: { client_id: "gxctl" } });
  await page.goto("/device");
  await page.getByLabel("The code the device shows").fill(start.user_code.toLowerCase().replace("-", ""));
  await page.getByRole("button", { name: "Continue" }).click();
  await page.getByRole("button", { name: "Deny" }).click();
  await expect(page.getByTestId("device-done")).toContainText("Denied");
  const r = await api("/d/local/token", {
    method: "POST",
    body: { grant_type: "urn:ietf:params:oauth:grant-type:device_code", client_id: "gxctl", device_code: start.device_code },
  });
  expect(r.error).toBe("access_denied");
});

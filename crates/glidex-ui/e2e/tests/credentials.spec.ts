// Login credentials: the VM form's "no login" default, and prefilling a
// new credential with the user's own public keys (GET /users/me/ssh-keys).
import { api, del, expect, field, test } from "../fixtures";

const KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl me@laptop";
const USER = "e2e-keys";

async function defaultProject(): Promise<string> {
  const projects = await api<{ id: string; name: string }[]>("/projects");
  return projects.find((p) => p.name === "default")!.id;
}

async function removeCredential(name: string) {
  await del(`/credentials/${name}?project=${await defaultProject()}`);
}

test.afterEach(async () => {
  await removeCredential(USER);
});

test("the VM form says there is no login without credentials", async ({ page }) => {
  const project = await defaultProject();
  for (const c of await api<{ username: string }[]>(`/credentials?project=${project}`)) {
    await removeCredential(c.username);
  }
  await page.goto("/");
  await page.getByRole("button", { name: "+ Create VM" }).click();
  const select = field(page, "Login Credential");
  await expect(select.locator("option").first()).toHaveText("No login available");
  await expect(page.getByTestId("credential-hint")).toContainText("no way to log in");

  // With a credential in the project the default reads "None (no login)".
  await api("/credentials", {
    method: "POST",
    body: { username: USER, project, ssh_authorized_keys: [KEY] },
  });
  await page.reload();
  await page.getByRole("button", { name: "+ Create VM" }).click();
  await expect(select.locator("option").first()).toHaveText("None (no login)");
  await expect(select.locator("option", { hasText: USER })).toHaveCount(1);
  await select.selectOption(USER);
  await expect(page.getByTestId("credential-hint")).toContainText("Provisioned by cloud-init");
});

test("Add Credential prefills my own public keys", async ({ page }) => {
  // The server reads ~/.ssh through glidex-authd, which only serves the
  // glidex service user; the scratch control plane can't use it, so the
  // answer is stubbed here (the server side has its own tests).
  await page.route("**/api/users/me/ssh-keys", (route) =>
    route.fulfill({ json: { available: true, username: USER, keys: [KEY] } }),
  );
  await page.goto("/credentials");
  await page.getByRole("button", { name: "Add Credential" }).click();
  const keys = page.getByPlaceholder("ssh-ed25519 AAAA... user@host");
  await expect(keys).toHaveValue(KEY);
  await expect(page.getByPlaceholder("alice")).toHaveValue(USER);
  await expect(page.getByTestId("my-keys-note")).toContainText("1 key");
  // "Add my keys again" doesn't duplicate them.
  await page.getByRole("button", { name: "Add my keys again" }).click();
  await expect(keys).toHaveValue(KEY);

  const [request] = await Promise.all([
    page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/credentials")),
    page.getByRole("button", { name: "Add", exact: true }).click(),
  ]);
  expect(request.postDataJSON()).toMatchObject({ username: USER, ssh_authorized_keys: [KEY] });
  await expect(page.getByText(USER, { exact: true })).toBeVisible();
});

test("Add Credential keeps what was typed and explains missing keys", async ({ page }) => {
  // Real server: the scratch control plane can't reach glidex-authd.
  await page.goto("/credentials");
  await page.getByRole("button", { name: "Add Credential" }).click();
  await expect(page.getByTestId("my-keys-note")).toContainText("can't be filled in automatically");
  await expect(page.getByPlaceholder("ssh-ed25519 AAAA... user@host")).toHaveValue("");
});

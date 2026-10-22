// Login credentials: the VM form's "no login" default, and prefilling a
// new credential with the user's own public keys (GET /users/me/ssh-keys).
import { api, del, expect, field, rpcRoute, test, waitForRpc } from "../fixtures";

const KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl me@laptop";
const USER = "e2e-keys";

async function defaultProject(): Promise<string> {
  const projects = (await api<{ projects?: { id: string; name: string }[] }>("/projects")).projects ?? [];
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
  for (const c of (await api<{ credentials?: { username: string }[] }>(`/credentials?project=${project}`)).credentials ?? []) {
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

test("the VM form defaults to my own credential when the project has one", async ({ page }) => {
  const project = await defaultProject();
  // api() is the same Unix user as the browser session.
  const me = (await api<{ user: { display_name: string } }>("/auth/whoami")).user.display_name;
  await api("/credentials", { method: "POST", body: { username: USER, project, ssh_authorized_keys: [KEY] } });
  await page.goto("/");
  await page.getByRole("button", { name: "+ Create VM" }).click();
  const select = field(page, "Login Credential");
  // Only someone else's credential: no default.
  await expect(select.locator("option", { hasText: USER })).toHaveCount(1);
  await expect(select).toHaveValue("");

  await api("/credentials", { method: "POST", body: { username: me, project, ssh_authorized_keys: [KEY] } });
  try {
    await page.reload();
    await page.getByRole("button", { name: "+ Create VM" }).click();
    await expect(select).toHaveValue(me);
    // It's only a default: "no login" can still be chosen.
    await select.selectOption("");
    await expect(select).toHaveValue("");
  } finally {
    await removeCredential(me);
  }
});

test("the VM form picks the credential I last gave a VM", async ({ page }) => {
  const project = await defaultProject();
  const me = (await api<{ user: { display_name: string } }>("/auth/whoami")).user.display_name;
  const OTHER = "e2e-keys-2";
  for (const u of [USER, OTHER, me]) {
    await api("/credentials", { method: "POST", body: { username: u, project, ssh_authorized_keys: [KEY] } });
  }
  try {
    // A firmware image to boot through, and a create that isn't made.
    await rpcRoute(page, "glidex.storage.v1.ImageService/ListImages", () => ({
      message: {
        images: [
          { id: "fw", name: "fw", kind: "IMAGE_KIND_FIRMWARE", arch: "ARCH_X86_64", hypervisor: "HYPERVISOR_CLOUD_HYPERVISOR", status: { state: "IMAGE_STATE_READY" } },
        ],
      },
    }));
    let sent: { credential?: string } | undefined;
    await rpcRoute(page, "glidex.vm.v1.VmService/CreateVm", (body) => {
      sent = body as { credential?: string };
      return { message: { vm: { id: "v1", name: "cred-vm" } } };
    });
    await page.goto("/");
    await page.getByRole("button", { name: "+ Create VM" }).click();
    const select = field(page, "Login Credential");
    // Nothing given yet: my own.
    await expect(select).toHaveValue(me);

    await field(page, "VM Name").fill("cred-vm");
    await field(page, "Hypervisor Backend").selectOption("cloudhypervisor");
    await field(page, "Boot Disk").selectOption("path");
    await field(page, "Disk Image Path").fill("/nonexistent/disk.img");
    await select.selectOption(OTHER);
    await page.getByRole("button", { name: "Create VM", exact: true }).click();
    await expect.poll(() => sent?.credential).toBe(OTHER);

    // The next VM starts from it, here and after a reload.
    await page.getByRole("button", { name: "+ Create VM" }).click();
    await expect(select).toHaveValue(OTHER);
    await page.reload();
    await page.getByRole("button", { name: "+ Create VM" }).click();
    await expect(select).toHaveValue(OTHER);

    // Gone from the project: back to my own.
    await removeCredential(OTHER);
    await page.reload();
    await page.getByRole("button", { name: "+ Create VM" }).click();
    await expect(select).toHaveValue(me);
  } finally {
    await removeCredential(OTHER);
    await removeCredential(me);
  }
});

test("Add Credential prefills my own public keys", async ({ page }) => {
  // The server reads ~/.ssh through glidex-authd, which only serves the
  // glidex service user; the scratch control plane can't use it, so the
  // answer is stubbed here (the server side has its own tests).
  await rpcRoute(page, "glidex.access.v1.UserService/GetMySshKeys", () => ({ message: { available: true, username: USER, keys: [KEY] } }));
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
    waitForRpc(page, "glidex.storage.v1.CredentialService/CreateCredential"),
    page.getByRole("button", { name: "Add", exact: true }).click(),
  ]);
  expect(request).toMatchObject({ username: USER, ssh_authorized_keys: [KEY] });
  await expect(page.getByText(USER, { exact: true })).toBeVisible();
});

test("Add Credential keeps what was typed and explains missing keys", async ({ page }) => {
  // Real server: the scratch control plane can't reach glidex-authd.
  await page.goto("/credentials");
  await page.getByRole("button", { name: "Add Credential" }).click();
  await expect(page.getByTestId("my-keys-note")).toContainText("can't be filled in automatically");
  await expect(page.getByPlaceholder("ssh-ed25519 AAAA... user@host")).toHaveValue("");
});

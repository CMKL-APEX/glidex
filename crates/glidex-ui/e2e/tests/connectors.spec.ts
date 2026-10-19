// The connector form's presets (spec/tenancy.md §5.1): Google (Workspace or
// gated consumer accounts, the device client) and JumpCloud, each with the
// redirect URI to register. Nothing here logs in through them: the
// connector tests do that against a mock IdP.
import { API_SOCKET, api, del, expect, rawRequest, test } from "../fixtures";

const GOOGLE = "e2e-google";
const JUMPCLOUD = "e2e-jumpcloud";

test("Google and JumpCloud connectors are made with their own fields", async ({ page }) => {
  try {
    await page.goto("/connectors");
    await page.getByRole("button", { name: "+ Add Connector" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.locator("#connector-kind").selectOption("google");
    await dialog.locator("#connector-id").fill(GOOGLE);
    await expect(dialog.getByTestId("redirect-uri")).toContainText(`/api/auth/c/${GOOGLE}/callback`);
    // Workspace by default: domains are required, 2-step trust offered.
    await expect(dialog.locator("#connector-domains")).toBeVisible();
    await expect(dialog.locator("#connector-trust-2sv")).toBeVisible();
    // Any account: the gate. Creating users with no allowlist is refused.
    await dialog.locator("#connector-google-accounts").selectOption("any");
    await expect(dialog.getByTestId("consumer-warning")).toBeVisible();
    await dialog.locator("#connector-client_id").fill("web-client");
    await dialog.getByLabel("Create users at their first login").check();
    await dialog.getByRole("button", { name: "Create Connector" }).click();
    await expect(dialog.getByText(/anyone with a Google account/)).toBeVisible();
    await dialog.locator("#connector-allowed_emails").fill("bob@gmail.com");
    // The device client brings its own secret field.
    await expect(dialog.locator("#connector-device-secret")).toHaveCount(0);
    await dialog.locator("#connector-device_client_id").fill("tv-client");
    await expect(dialog.locator("#connector-device-secret")).toBeVisible();
    await dialog.getByRole("button", { name: "Create Connector" }).click();
    await expect(dialog).toBeHidden();

    const row = page.getByRole("row", { name: new RegExp(GOOGLE) });
    await expect(row).toContainText("Google");
    await expect(row).toContainText("any Google account");
    await expect(row.getByRole("button", { name: "Set secret" })).toBeVisible();
    await expect(row.getByRole("button", { name: "Set device secret" })).toBeVisible();
    const made = (await api(`/connectors/${GOOGLE}`)).connector;
    expect(made.google).toMatchObject({ accounts: "GOOGLE_ACCOUNTS_ANY", client_id: "web-client", allowed_emails: ["bob@gmail.com"], device_client_id: "tv-client" });
    // Saved with the redirect URI it showed: this page's address.
    expect(made.google.redirect_uri).toBe(`${new URL(page.url()).origin}/api/auth/c/${GOOGLE}/callback`);

    // Edited in the same form.
    await row.getByRole("button", { name: "Edit" }).click();
    await expect(dialog.locator("#connector-google-accounts")).toHaveValue("any");
    await expect(dialog.locator("#connector-allowed_emails")).toHaveValue("bob@gmail.com");
    await dialog.locator("#connector-allowed_emails").fill("bob@gmail.com, carol@gmail.com");
    await dialog.getByRole("button", { name: "Save" }).click();
    await expect(dialog).toBeHidden();
    expect((await api(`/connectors/${GOOGLE}`)).connector.google.allowed_emails).toEqual(["bob@gmail.com", "carol@gmail.com"]);

    // JumpCloud: its groups claim.
    await page.getByRole("button", { name: "+ Add Connector" }).click();
    await dialog.locator("#connector-kind").selectOption("jumpcloud");
    await dialog.locator("#connector-id").fill(JUMPCLOUD);
    await expect(dialog.locator("#connector-issuer")).toHaveCount(0);
    await dialog.locator("#connector-client_id").fill("jc-client");
    await dialog.locator("#connector-groups_claim").fill("groups");
    await dialog.getByRole("button", { name: "Create Connector" }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByRole("row", { name: new RegExp(JUMPCLOUD) })).toContainText("groups in groups");
    expect((await api(`/connectors/${JUMPCLOUD}`)).connector.jumpcloud).toMatchObject({ client_id: "jc-client", groups_claim: "groups" });
  } finally {
    await del(`/connectors/${GOOGLE}`);
    await del(`/connectors/${JUMPCLOUD}`);
  }
});

test("users show how they sign in", async ({ page }) => {
  // spec/tenancy.md D17: one person through two connectors is two users;
  // the lists say which way each signs in.
  await page.goto("/access");
  const users = page.locator("section", { has: page.getByRole("heading", { name: "Users" }) });
  await expect(users.getByText(/^(Local password|Host socket)$/).first()).toBeVisible();
  await expect(users.getByText(/^(unix|pam):/)).toHaveCount(0);
});

test("your account lists your sign-in methods and offers more", async ({ page }) => {
  // spec/tenancy.md §6.2: a user may have several; the only one isn't removable.
  const made = await api("/connectors", {
    method: "POST",
    body: { connector: { id: "e2e-add-sso", display_name: "E2E Add SSO", oidc: { issuer: "https://idp.example", client_id: "glidex" } } },
  });
  expect(made.connector?.id, JSON.stringify(made)).toBe("e2e-add-sso");
  try {
    // The header's name leads there.
    await page.goto("/");
    await page.locator('a[href="/account"]').click();
    await expect(page).toHaveURL(/\/account$/);
    await expect(page.getByRole("heading", { name: "Your account" })).toBeVisible();
    const list = page.getByTestId("sign-in-methods-list");
    await expect(list.getByText(/^(Local password|Host socket)$/).first()).toBeVisible();
    await expect(list.getByRole("button", { name: "Remove" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "E2E Add SSO" })).toBeVisible();
  } finally {
    await del("/connectors/e2e-add-sso");
  }
});

test("an administrator removes one of a user's sign-in methods", async ({ page }) => {
  const made = await api("/users", {
    method: "POST",
    body: { display_name: "Two Ways", identities: [{ provider: "pam", subject: "e2e-twoways" }, { provider: "oidc:https://idp.example", subject: "tw-1" }] },
  });
  const id = made.user?.id as string;
  expect(id, JSON.stringify(made)).toBeTruthy();
  try {
    await page.goto("/access");
    const row = page.getByRole("row", { name: /Two Ways/ });
    await expect(row.getByRole("button", { name: "remove" })).toHaveCount(2);
    await row.getByRole("button", { name: "remove" }).first().click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByText("Also sign them out everywhere")).toBeVisible();
    await dialog.getByRole("button", { name: "Remove" }).click();
    await expect(dialog).toBeHidden();
    // One left, and the last one isn't offered for removal.
    await expect(row.getByRole("button", { name: "remove" })).toHaveCount(0);
    const user = ((await api("/users")).users ?? []).find((u: { id: string }) => u.id === id);
    expect(user?.identities?.length, JSON.stringify(user)).toBe(1);
    // Disabled, its last one can go too (to be added to the right user).
    await api(`/users/${id}`, { method: "PATCH", body: { disabled: true } });
    await page.reload();
    await row.getByRole("button", { name: "remove" }).click();
    await dialog.getByRole("button", { name: "Remove" }).click();
    await expect(dialog).toBeHidden();
    const gone = ((await api("/users")).users ?? []).find((u: { id: string }) => u.id === id);
    expect(gone?.identities ?? [], JSON.stringify(gone)).toHaveLength(0);

    // Then deleted, typing its name to confirm.
    await row.getByRole("button", { name: "Delete" }).click();
    await dialog.getByRole("textbox").fill("Two Ways");
    await dialog.getByRole("button", { name: "Delete user" }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByRole("status")).toContainText("Deleted Two Ways");
    await expect(page.getByRole("row", { name: /Two Ways/ })).toHaveCount(0);
    expect(((await api("/users")).users ?? []).some((u: { id: string }) => u.id === id)).toBe(false);
  } finally {
    await del(`/users/${id}`);
  }
});

test("a SCIM connector gets its endpoint and a token shown once", async ({ page }) => {
  const ID = "e2e-scim";
  try {
    await page.goto("/connectors");
    await page.getByRole("button", { name: "+ Add Connector" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.locator("#connector-kind").selectOption("jumpcloud");
    await dialog.locator("#connector-id").fill(ID);
    await dialog.locator("#connector-client_id").fill("jc-client");
    await dialog.getByLabel("Create users at their first login").check();
    // Provisioned: only the IdP's people log in, so JIT goes off.
    await dialog.locator("#connector-provisioning").selectOption("scim");
    await expect(dialog.getByLabel("Create users at their first login")).toBeDisabled();
    await dialog.locator("#connector-group-mode").selectOption("auto");
    await dialog.getByRole("button", { name: "Create Connector" }).click();
    await expect(dialog).toBeHidden();
    const made = (await api(`/connectors/${ID}`)).connector;
    expect(made).toMatchObject({ provisioning: "scim", group_mode: "auto", jit: false });

    const row = page.getByRole("row", { name: new RegExp(ID) });
    await expect(row).toContainText("SCIM-provisioned, a team per group");
    await row.getByRole("button", { name: "SCIM" }).click();
    const scim = page.getByRole("dialog");
    await expect(scim.getByTestId("scim-url")).toHaveText(`${new URL(page.url()).origin}/api/scim/v2/c/${ID}`);
    await expect(scim.getByText("No tokens")).toBeVisible();
    await scim.locator("#scim-name").fill("jumpcloud");
    await scim.locator("#scim-days").fill("30");
    await scim.getByRole("button", { name: "Create token" }).click();
    const secret = (await scim.getByTestId("scim-secret").textContent()) ?? "";
    expect(secret).toMatch(/^gxs_[0-9A-Za-z]+$/);
    await expect(scim.getByRole("cell", { name: "jumpcloud" })).toBeVisible();
    // Never listed again.
    const listed = await api(`/connectors/${ID}/scim-tokens`);
    expect(JSON.stringify(listed)).not.toContain(secret);
    await scim.getByRole("button", { name: "Done" }).click();
    await expect(scim.getByTestId("scim-secret")).toHaveCount(0);
    await scim.getByRole("button", { name: "Revoke" }).click();
    await expect(scim.getByText("No tokens")).toBeVisible();
  } finally {
    await del(`/connectors/${ID}`);
  }
});

test("users and groups the IdP provisions show as its own", async ({ page }) => {
  const ID = "e2e-scim-users";
  const scim = async (method: string, path: string, token: string, body?: unknown) => {
    const r = await rawRequest({ socketPath: API_SOCKET }, method, `/scim/v2/c/${ID}${path}`, {
      body,
      headers: { authorization: `Bearer ${token}` },
    });
    return { status: r.status, body: r.body ? JSON.parse(r.body) : null };
  };
  try {
    await api("/connectors", {
      method: "POST",
      body: { connector: { id: ID, display_name: "E2E JumpCloud", provisioning: "scim", group_mode: "auto", jumpcloud: { client_id: "jc" } } },
    });
    const { secret } = await api(`/connectors/${ID}/scim-tokens`, { method: "POST", body: { name: "e2e" } });
    expect(secret).toMatch(/^gxs_/);
    expect((await scim("GET", "/Users", "gxs_wrong")).status).toBe(401);
    const made = await scim("POST", "/Users", secret, {
      userName: "grace@e2e.example",
      name: { givenName: "Grace", familyName: "Hopper" },
      emails: [{ value: "grace@e2e.example", primary: true }],
      active: true,
    });
    expect(made.status).toBe(201);
    const grace = made.body.id;
    expect((await scim("POST", "/Groups", secret, { displayName: "e2e-navy", members: [{ value: grace }] })).status).toBe(201);

    await page.goto("/directory/access");
    const row = page.getByRole("row", { name: /Grace Hopper/ });
    await expect(row).toContainText("managed by E2E JumpCloud");
    // Its delete is the IdP's while it is enabled; a disable isn't.
    await expect(row.getByRole("button", { name: "Delete" })).toHaveCount(0);
    await expect(row.getByRole("button", { name: "Disable" })).toBeVisible();
    await expect(page.getByText("from E2E JumpCloud")).toBeVisible();

    // The IdP deactivates it: disabled by its IdP, and only the IdP enables it.
    const off = await scim("PATCH", `/Users/${grace}`, secret, { Operations: [{ op: "replace", path: "active", value: false }] });
    expect(off.status).toBe(200);
    await page.reload();
    await expect(row).toContainText("disabled by its IdP");
    await expect(row.getByRole("button", { name: "Enable" })).toHaveCount(0);
    await expect(row.getByRole("button", { name: "Delete" })).toBeVisible();
  } finally {
    await del(`/connectors/${ID}`);
    const users = (await api<{ users?: any[] }>("/users")).users ?? [];
    for (const u of users.filter((u) => u.display_name === "Grace Hopper")) await del(`/users/${u.id}`);
    const teams = (await api<{ teams?: any[] }>("/teams")).teams ?? [];
    for (const t of teams.filter((t) => t.name === "e2e-navy")) await del(`/teams/${t.id}`);
  }
});

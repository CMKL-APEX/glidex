// The connector form's presets (spec/tenancy.md §5.1): Google (Workspace or
// gated consumer accounts, the device client) and JumpCloud, each with the
// redirect URI to register. Nothing here logs in through them: the
// connector tests do that against a mock IdP.
import { api, del, expect, test } from "../fixtures";

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

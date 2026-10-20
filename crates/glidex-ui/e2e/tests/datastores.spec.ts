// The Datastores page (spec/storage.md S2): the built-in `local`, adding a
// directory datastore, what the node finds there, making it the default,
// and the disk form offering it. Nothing is booted.
import { mkdirSync } from "node:fs";
import { api, del, E2E_HOME, expect, field, test } from "../fixtures";

test("adds a datastore, sees it checked, and makes it the default for disks", async ({ page }) => {
  const name = `e2e-fast-${Date.now().toString(36)}`;
  // The control plane's datastore root (scripts/start-control-plane.sh).
  mkdirSync(`${E2E_HOME}/datastores/${name}`, { recursive: true });
  try {
    await page.goto("/datastores");
    await expect(page.getByRole("heading", { name: "Datastores" })).toBeVisible();
    const local = page.locator("tr").filter({ has: page.getByRole("button", { name: "local", exact: true }) });
    await expect(local).toContainText("built in");
    await expect(local).toContainText("default for disks");

    await page.getByRole("button", { name: "Add Datastore", exact: true }).click();
    await field(page, "Name").fill(name);
    // The path defaults to <root>/<name>; the form shows /srv/glidex, so
    // give the scratch root's.
    await field(page, "Path").fill(`${E2E_HOME}/datastores/${name}`);
    await page.getByRole("button", { name: "Add datastore", exact: true }).click();

    const row = page.locator("tr").filter({ has: page.getByRole("button", { name, exact: true }) });
    // The node looks at it and finds it.
    await expect(row).toContainText("1 of 1", { timeout: 15_000 });
    await row.getByRole("button", { name: "Make default" }).click();
    await expect(row).toContainText("default for disks");
    await expect(local).not.toContainText("default for disks");

    // The disk form offers it, chosen.
    await page.goto("/disks");
    await page.getByRole("button", { name: "Create Disk" }).click();
    await expect(field(page, "Datastore").locator("option:checked")).toHaveText(`${name} (default)`);
  } finally {
    const ds = (await api<{ datastores?: any[] }>("/datastores")).datastores ?? [];
    const mine = ds.find((d) => d.name === name);
    if (mine) await del(`/datastores/${mine.id}`);
  }
  // Deleting it gives `local` the default back.
  const back = (await api<{ datastore: any }>("/datastores/local")).datastore;
  expect(back.default_for_disks).toBe(true);
});

// Two organizations (spec/tenancy.md D63–D73), on the two control planes
// of scripts/start-peers.sh: `north`, a cluster, and `acme`, another
// organization's directory. North's operator makes an offer on the
// Admissions page; acme's tenant administrator accepts it on the tenant's
// page; once north has pulled acme's feed, acme's administrator signs in
// to north with "Sign in with Acme"; logging out there ends that sign-in's
// chain at acme (D72), and north's report shows on acme's tenant page.
import type { Browser, BrowserContext } from "@playwright/test";
import { expect, rawRequest, test } from "../fixtures";

const PEERS = process.env.GLIDEX_E2E_PEERS_HOME ?? "/tmp/glidex-ui-e2e-peers";
const NORTH = {
  sock: `${PEERS}/north/api.sock`,
  ui: `https://localhost:${process.env.E2E_NORTH_UI_PORT ?? 5186}`,
  host: "localhost",
};
const ACME = {
  sock: `${PEERS}/acme/api.sock`,
  ui: `https://127.0.0.1:${process.env.E2E_ACME_UI_PORT ?? 5187}`,
  host: "127.0.0.1",
};
type Peer = typeof NORTH;

/** JSON from a peer's REST API over its api.sock, as its break-glass admin. */
async function peerApi<T = any>(p: Peer, path: string, init?: { method?: string; body?: unknown }): Promise<T> {
  const r = await rawRequest({ socketPath: p.sock }, init?.method ?? "GET", path, { body: init?.body });
  const text = r.body;
  try {
    return JSON.parse(text) as T;
  } catch {
    return text as T;
  }
}

/** A browser context signed in to `p`'s web UI as the test user, who holds
 * system-admin there (a browser session doesn't carry break-glass). */
async function signedIn(browser: Browser, p: Peer): Promise<BrowserContext> {
  const me = await peerApi(p, "/auth/whoami");
  const principal = { type: "User", id: me.user.id };
  const bindings: any[] = (await peerApi(p, "/system/bindings")).bindings ?? [];
  if (!bindings.some((b) => b.role === "role.system-admin" && b.principal.id === me.user.id)) {
    const tenants: any[] = (await peerApi(p, "/tenants")).tenants ?? [];
    const operator = tenants.find((t) => t.operates_here);
    await peerApi(p, `/tenants/${operator.id}/members`, { method: "POST", body: { principal } });
    await peerApi(p, "/system/bindings", { method: "POST", body: { role: "role.system-admin", principal } });
  }
  const s = await peerApi(p, "/auth/session", { method: "POST", body: {} });
  expect(s.cookie_name, JSON.stringify(s)).toBe("gx_session");
  const context = await browser.newContext({ ignoreHTTPSErrors: true, storageState: { cookies: [], origins: [] } });
  await context.addCookies([
    { name: s.cookie_name, value: s.cookie, domain: p.host, path: "/", httpOnly: true, secure: true, sameSite: "Lax" },
  ]);
  return context;
}

test.use({ ignoredConsoleErrors: /status of (401|403)/ });

test("a tenant of another organization's directory signs in to the cluster and out again", async ({ browser }) => {
  test.setTimeout(240_000);

  // At acme: the tenant, its administrator the test user.
  const acmeMe = await peerApi(ACME, "/auth/whoami");
  expect(acmeMe.user?.id, JSON.stringify(acmeMe)).toBeTruthy();
  const created = await peerApi(ACME, "/tenants", { method: "POST", body: { slug: "acme", display_name: "Acme" } });
  expect(created.tenant?.id, JSON.stringify(created)).toBeTruthy();
  const principal = { type: "User", id: acmeMe.user.id };
  await peerApi(ACME, "/tenants/acme/members", { method: "POST", body: { principal } });
  const bound = await peerApi(ACME, "/tenants/acme/bindings", { method: "POST", body: { role: "role.tenant-admin", principal } });
  expect(bound.binding?.id, JSON.stringify(bound)).toBeTruthy();

  // North's operator makes the offer.
  const north = await signedIn(browser, NORTH);
  const np = await north.newPage();
  await np.goto(`${NORTH.ui}/admissions`);
  await np.getByLabel("Tenant (its slug there)").fill("acme");
  await np.getByLabel("Its directory's issuer").fill(`${ACME.ui}/d/local`);
  await np.getByRole("button", { name: "Make offer" }).click();
  const shown = np.getByTestId("offer-token");
  await expect(shown).toBeVisible();
  const offer = ((await shown.locator(".font-mono").textContent()) ?? "").trim();
  expect(offer).toMatch(/^gxo_/);

  // Acme's tenant administrator accepts it.
  const acme = await signedIn(browser, ACME);
  const ap = await acme.newPage();
  // By id, as the app links to it: its checks are on the tenant's id.
  await ap.goto(`${ACME.ui}/tenants/${created.tenant.id}`);
  await ap.getByLabel("Use a cluster: the offer its operator sent you").fill(offer);
  await ap.getByRole("button", { name: "Accept offer" }).click();
  await expect(ap.getByTestId("tenant-clusters")).toBeVisible();

  // North lists acme's directory, and pulls its feed (every 30 s).
  await np.reload();
  await expect(np.getByText(`${ACME.ui}/d/local`).first()).toBeVisible();
  await expect
    .poll(async () => ((await peerApi(NORTH, "/admissions/directories")).directories ?? []).some((d: any) => d.last_sync), {
      timeout: 90_000,
      message: "north pulled acme's feed",
    })
    .toBe(true);

  // Sign in to north at acme: the browser already has its acme session.
  const page = await acme.newPage();
  await page.goto(`${NORTH.ui}/login`);
  await page.getByRole("button", { name: /Sign in with Acme/ }).or(page.getByRole("link", { name: /Sign in with Acme/ })).click();
  await page.waitForURL(`${NORTH.ui}/**`);
  await expect(page.getByRole("heading", { name: "Sign in to the VM Control Panel" })).toHaveCount(0);
  const me = await page.evaluate(async () => (await fetch("/api/auth/whoami")).json());
  expect(me.method, JSON.stringify(me)).toBe("directory");
  expect(me.user?.id).toBe(acmeMe.user.id);

  // Logging out at north ends the sign-in's chain at acme (D72).
  await page.getByRole("button", { name: "Log out" }).click();
  await expect(page.getByRole("heading", { name: "Sign in to the VM Control Panel" })).toBeVisible();
  await expect
    .poll(
      async () => ((await peerApi(ACME, "/audit")).entries ?? []).some((e: any) => e.action === "endClusterSession"),
      { timeout: 30_000, message: "acme ended the chain" },
    )
    .toBe(true);

  // North reports back what it holds of acme's tenant (D70).
  await expect
    .poll(
      async () => {
        await ap.reload();
        const table = ap.getByTestId("tenant-clusters");
        await expect(table).toBeVisible();
        return table.getByText("admitted", { exact: true }).count();
      },
      { timeout: 90_000, message: "north's report on acme's tenant page" },
    )
    .toBeGreaterThan(0);

  await north.close();
  await acme.close();
});

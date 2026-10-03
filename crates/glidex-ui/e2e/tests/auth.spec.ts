// Login, session and browser protections (spec/security.md §5.5, §5.6,
// §14 "UI e2e"): the login page, CSRF on writes, logout, console tickets,
// and glidex-ui's Host allowlist and security headers.
import { api, del, expect, rawRequest, test, UI_SERVER_PORT } from "../fixtures";

const UI_SERVER = `http://localhost:${UI_SERVER_PORT}`;

test.describe("signed out", () => {
  test.use({ storageState: { cookies: [], origins: [] }, ignoredConsoleErrors: /status of 401/ });

  test("shows the login page", async ({ page }) => {
    await page.goto("/disks");
    await expect(page.getByText("Sign in to the VM Control Panel")).toBeVisible();
    // PAM is on and OIDC off in the scratch config.
    await expect(page.getByLabel("Username")).toBeVisible();
    await expect(page.getByLabel("Password")).toBeVisible();
    await expect(page.getByRole("button", { name: "Sign in", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Sign in with SSO" })).toHaveCount(0);
    // Nothing of the app itself.
    await expect(page.getByRole("link", { name: "Projects" })).toHaveCount(0);
  });

  test("the API refuses requests without a session", async ({ page }) => {
    await page.goto("/");
    const status = await page.evaluate(() => fetch("/api/vms").then((r) => r.status));
    expect(status).toBe(401);
  });
});

test.describe("signed in", () => {
  test.use({ ignoredConsoleErrors: /status of 403|WebSocket/ });

  test("shows who is signed in and the selected project", async ({ page }) => {
    const me = await api("/auth/whoami");
    const projects: any[] = await api("/projects");
    const dflt = projects.find((p) => p.name === "default");
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Virtual Machines" })).toBeVisible();
    await expect(page.getByText(me.user.display_name, { exact: true })).toBeVisible();
    await expect(page.getByLabel("Project")).toHaveValue(dflt.id);
    await expect(page.getByRole("link", { name: "Policies" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Audit" })).toBeVisible();
  });

  test("writes carry the CSRF header; without it they are refused", async ({ page }) => {
    await page.goto("/access");
    const csrf = await page.evaluate(() => fetch("/api/auth/whoami").then((r) => r.json()).then((w) => w.csrf));
    expect(csrf).toBeTruthy();

    await page.getByLabel("New team name").fill("e2e-csrf");
    const [request] = await Promise.all([
      page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith("/api/teams")),
      page.getByRole("button", { name: "Create team" }).click(),
    ]);
    expect(request.headers()["x-glidex-csrf"]).toBe(csrf);
    await expect(page.getByText("e2e-csrf", { exact: true })).toBeVisible();

    // The same request without the header.
    const refused = await page.evaluate(() =>
      fetch("/api/teams", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: "e2e-no-csrf" }),
      }).then(async (r) => ({ status: r.status, body: await r.json() })),
    );
    expect(refused.status).toBe(403);
    expect(refused.body.error).toBe("csrf_failed");

    for (const t of (await api<any[]>("/teams")).filter((t) => t.name.startsWith("e2e-"))) {
      await del(`/teams/${t.id}`);
    }
  });

  test("logging out ends the session", async ({ browser }) => {
    // A session of its own, so the shared one survives.
    const s = await api("/auth/session", { method: "POST", body: {} });
    const context = await browser.newContext({
      storageState: {
        cookies: [{ name: s.cookie_name, value: s.cookie, domain: "localhost", path: "/", expires: -1, httpOnly: true, secure: false, sameSite: "Strict" }],
        origins: [],
      },
    });
    const page = await context.newPage();
    await page.goto("/");
    await page.getByRole("button", { name: "Log out" }).click();
    await expect(page.getByText("Sign in to the VM Control Panel")).toBeVisible();
    await page.reload();
    await expect(page.getByText("Sign in to the VM Control Panel")).toBeVisible();
    await context.close();
  });
});

test.describe("console", () => {
  test.use({ ignoredConsoleErrors: /WebSocket|status of 40[34]/ });
  let vmId = "";

  test.beforeAll(async () => {
    const vm = await api("/vms", {
      method: "POST",
      body: {
        name: "e2e-console",
        project: "default",
        vcpu_count: 1,
        mem_size_mib: 256,
        hypervisor: "cloudhypervisor",
        kernel_image_path: "",
        firmware_path: "~/.glidex/CLOUDHV.fd",
        rootfs_path: "/nonexistent/disk.img",
      },
    });
    expect(vm.id, JSON.stringify(vm)).toBeTruthy();
    vmId = vm.id;
  });

  test.afterAll(async () => {
    if (vmId) await del(`/vms/${vmId}`);
  });

  for (const [server, base] of [
    ["the dev server", ""],
    ["glidex-ui", UI_SERVER],
  ] as const) {
    test(`opens through a single-use ticket (${server})`, async ({ page }) => {
      const ticket = page.waitForRequest((r) => r.method() === "POST" && r.url().endsWith(`/api/vms/${vmId}/console/ticket`));
      // (The dev server has a WebSocket of its own, for HMR.)
      const socket = page.waitForEvent("websocket", (ws) => ws.url().includes("/console/ws"));
      await page.goto(`${base}/vms/${vmId}/console`);
      await ticket;
      const ws = await socket;
      expect(ws.url()).toMatch(/\/api\/vms\/[^/]+\/console\/ws\?ticket=.+/);
      // The VM isn't running: the server accepted the upgrade and says so.
      await expect(page.locator(".xterm-rows")).toContainText("Failed to connect to the VM console", { timeout: 15_000 });
    });
  }

  test("a WebSocket without a ticket is refused", async ({ page }) => {
    await page.goto("/");
    const opened = await page.evaluate(
      (id) =>
        new Promise<boolean>((resolve) => {
          const ws = new WebSocket(`ws://${location.host}/api/vms/${id}/console/ws`);
          ws.onopen = () => resolve(true);
          ws.onerror = () => resolve(false);
        }),
      vmId,
    );
    expect(opened).toBe(false);
  });
});

test.describe("glidex-ui server", () => {
  test("refuses a foreign Host with 421", async () => {
    for (const path of ["/", "/projects", "/api/health"]) {
      for (const host of ["evil.example", `evil.example:${UI_SERVER_PORT}`, "localhost.evil.example"]) {
        const r = await rawRequest({ port: UI_SERVER_PORT }, "GET", path, { headers: { Host: host } });
        expect(r.status, `${host}${path}`).toBe(421);
      }
    }
    const ok = await rawRequest({ port: UI_SERVER_PORT }, "GET", "/api/health", { headers: { Host: `localhost:${UI_SERVER_PORT}` } });
    expect(ok.status).toBe(200);
    expect(JSON.parse(ok.body).status).toBe("ok");
  });

  test("sends the security headers", async () => {
    for (const path of ["/", "/api/health"]) {
      const r = await rawRequest({ port: UI_SERVER_PORT }, "GET", path, { headers: { Host: `127.0.0.1:${UI_SERVER_PORT}` } });
      expect(r.status).toBe(200);
      expect(r.headers["content-security-policy"]).toContain("default-src 'self'");
      expect(r.headers["content-security-policy"]).toContain("frame-ancestors 'none'");
      expect(r.headers["x-content-type-options"]).toBe("nosniff");
      expect(r.headers["referrer-policy"]).toBe("no-referrer");
      expect(r.headers["x-frame-options"]).toBe("DENY");
      // Plain HTTP on loopback: no HSTS.
      expect(r.headers["strict-transport-security"]).toBeUndefined();
    }
  });

  test("the built app runs under its CSP", async ({ page }) => {
    // The fixture fails the test on any console error, CSP violations included.
    await page.goto(`${UI_SERVER}/projects`);
    await expect(page.getByRole("heading", { name: "Projects" })).toBeVisible();
    await expect(page.getByRole("link", { name: "default" })).toBeVisible();
  });
});

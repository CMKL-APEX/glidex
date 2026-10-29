// A browser session for the specs (spec/11-security.md §5.5). The scratch
// control plane has no PAM here, so the user running the tests gets one
// the way `gxctl ui` does: over the local socket (peer uid), as its
// break-glass administrator, it
//   1. checks its own glidex user holds the system-admin role (a browser
//      session doesn't carry break-glass over). The scratch instance's
//      bootstrap gives it that and the rest of a first administrator's
//      roles, since admin_group is the tester's group; if it hasn't, it is
//      granted here, after joining the operator tenant, as the API needs;
//   2. makes the `default` project its default,
//   3. opens a session (POST /auth/session) and hands the cookie to the
//      browser through Playwright's storage state.
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { test as setup, expect } from "@playwright/test";
import { api, STORAGE_STATE } from "../fixtures";

setup("open a browser session", async () => {
  const me = await api("/auth/whoami");
  expect(me.user?.id, `whoami over the socket: ${JSON.stringify(me)}`).toBeTruthy();
  expect(me.break_glass, "the test user is the scratch control plane's admin").toBe(true);

  const bindings: any[] = (await api("/system/bindings")).bindings ?? [];
  const has = bindings.some(
    (b) => b.role === "role.system-admin" && b.principal.type === "User" && b.principal.id === me.user.id,
  );
  if (!has) {
    const principal = { type: "User", id: me.user.id };
    const tenants: any[] = (await api("/tenants")).tenants ?? [];
    const operator = tenants.find((t) => t.operates_here);
    expect(operator, `the operator tenant: ${JSON.stringify(tenants)}`).toBeTruthy();
    const joined = await api(`/tenants/${operator.id}/members`, { method: "POST", body: { principal } });
    expect(joined.member, JSON.stringify(joined)).toBeTruthy();
    const r = await api("/system/bindings", { method: "POST", body: { role: "role.system-admin", principal } });
    expect(r.binding?.id, JSON.stringify(r)).toBeTruthy();
  }
  const updated = await api("/users/me", { method: "PATCH", body: { default_project: "default" } });
  expect(updated.user?.default_project, JSON.stringify(updated)).toBeTruthy();

  const s = await api("/auth/session", { method: "POST", body: {} });
  expect(s.cookie_name, JSON.stringify(s)).toBe("gx_session");
  mkdirSync(dirname(STORAGE_STATE), { recursive: true });
  writeFileSync(
    STORAGE_STATE,
    JSON.stringify({
      cookies: [
        {
          name: s.cookie_name,
          value: s.cookie,
          // Cookies aren't per port: this covers the Vite server and glidex-ui.
          domain: "localhost",
          path: "/",
          expires: -1,
          httpOnly: true,
          secure: false,
          sameSite: "Strict",
        },
      ],
      origins: [],
    }),
  );
});

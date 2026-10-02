// A browser session for the specs (spec/security.md §5.5). The scratch
// control plane has no PAM here, so the user running the tests gets one
// the way `gxctl ui` does: over the local socket (peer uid), as its
// break-glass administrator, it
//   1. gives its own glidex user the system-admin role (a browser session
//      doesn't carry break-glass over),
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

  const bindings: any[] = await api("/system/bindings");
  const has = bindings.some(
    (b) => b.template === "role.system-admin" && b.principal.type === "User" && b.principal.id === me.user.id,
  );
  if (!has) {
    const b = await api("/system/bindings", {
      method: "POST",
      body: { role: "role.system-admin", principal: { type: "User", id: me.user.id } },
    });
    expect(b.id, JSON.stringify(b)).toBeTruthy();
  }
  const updated = await api("/users/me", { method: "PATCH", body: { default_project: "default" } });
  expect(updated.default_project, JSON.stringify(updated)).toBeTruthy();

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

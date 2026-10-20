// Workload federation (spec/tenancy.md §9): a federation made on the
// tenant's page, against a small OIDC issuer this test serves on the
// loopback, then a job's JWT from it exchanged for a token acting as a
// service account, and used.
import { createServer, type Server } from "node:http";
import { createSign, generateKeyPairSync, randomUUID } from "node:crypto";
import type { AddressInfo } from "node:net";
import { API_SOCKET, api, del, expect, rawRequest, test } from "../fixtures";

const ID = "e2e-ci";
const b64 = (v: string | Buffer) => Buffer.from(v).toString("base64url");

let server: Server;
let issuer = "";
const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: "jwk" }), kid: "e2e", alg: "RS256", use: "sig" };

function jwt(claims: Record<string, unknown>): string {
  const head = b64(JSON.stringify({ alg: "RS256", typ: "JWT", kid: "e2e" }));
  const now = Math.floor(Date.now() / 1000);
  const body = b64(JSON.stringify({ iss: issuer, aud: "glidex-e2e", iat: now, nbf: now, exp: now + 300, jti: randomUUID(), ...claims }));
  const sig = createSign("RSA-SHA256").update(`${head}.${body}`).sign(privateKey);
  return `${head}.${body}.${b64(sig)}`;
}

test.beforeAll(async () => {
  server = createServer((req, res) => {
    res.setHeader("content-type", "application/json");
    if (req.url === "/.well-known/openid-configuration") {
      res.end(JSON.stringify({ issuer, jwks_uri: `${issuer}/jwks` }));
    } else if (req.url === "/jwks") {
      res.end(JSON.stringify({ keys: [jwk] }));
    } else {
      res.statusCode = 404;
      res.end("{}");
    }
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  issuer = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

test.afterAll(async () => {
  await del(`/tenants/default/federations/${ID}`);
  server.close();
});

test("a CI job's token is exchanged for one acting as a service account", async ({ page }) => {
  const project = ((await api<{ projects?: any[] }>("/projects")).projects ?? []).find((p) => p.name === "default");
  const made = await api("/tokens", { method: "POST", body: { name: "e2e-ci-deployer", kind: "TOKEN_KIND_SERVICE_ACCOUNT", project: project.id } });
  const sa = made.record?.id;
  expect(sa).toBeTruthy();
  try {
    await page.goto("/tenants/default");
    const card = page.locator("section", { has: page.getByRole("heading", { name: "Workload federation" }) });
    await card.getByRole("button", { name: "+ Add federation" }).click();
    await card.locator("#fed-preset").selectOption({ label: "Other (any OIDC workload issuer)" });
    await card.locator("#fed-issuer").fill(issuer);
    await card.locator("#fed-id").fill(ID);
    await card.locator("#fed-name").fill("E2E CI");
    await card.locator("#fed-audience").fill("glidex-e2e");
    await card.locator("#fed-claims-0").fill("sub=job:e2e:main");
    await card.locator("#fed-sa-0").selectOption(sa);
    await card.getByRole("button", { name: "Add federation" }).click();
    const row = card.getByTestId("federations").locator("li", { hasText: "E2E CI" });
    await expect(row).toContainText("sub=job:e2e:main");
    await expect(row).toContainText("enabled");

    // The job's side: its JWT for a glidex token.
    const exchange = (token: string) =>
      rawRequest({ socketPath: API_SOCKET }, "POST", "/auth/exchange", {
        body: {
          grant_type: "urn:ietf:params:oauth:grant-type:token-exchange",
          subject_token_type: "urn:ietf:params:oauth:token-type:jwt",
          federation: `default/${ID}`,
          subject_token: token,
        },
      });
    const r = await exchange(jwt({ sub: "job:e2e:main" }));
    expect(r.status).toBe(200);
    const granted = JSON.parse(r.body);
    expect(granted.access_token).toMatch(/^gxt_/);
    expect(granted.expires_in).toBe(900);
    const who = await page.request.get("/api/auth/whoami", { headers: { authorization: `Bearer ${granted.access_token}` } });
    expect(who.status()).toBe(200);
    expect(JSON.stringify(await who.json())).toContain(sa);
    // Another job isn't let in.
    const other = await exchange(jwt({ sub: "job:e2e:dev" }));
    expect(other.status).toBe(400);
    expect(JSON.parse(other.body).error).toBe("invalid_grant");

    // Disabled on the page: the token it gave stops working.
    await row.getByRole("button", { name: "Disable" }).click();
    await expect(row).toContainText("disabled");
    const after = await page.request.get("/api/auth/whoami", { headers: { authorization: `Bearer ${granted.access_token}` } });
    expect(after.status()).toBe(401);
  } finally {
    await del(`/tokens/${sa}`);
  }
});

// VmConsole lifecycle, independent of any hypervisor: xterm 5.5 threw
// "reading 'dimensions'" when a terminal was disposed right after open()
// (React StrictMode's double mount in dev, or leaving the page at once).
// The `pageErrors` fixture fails the test on any such error.
import { expect, test } from "../fixtures";

// There is no such VM: the console WebSocket is refused and its detail
// page gets 404s.
test.use({ ignoredConsoleErrors: /WebSocket|status of 404/ });

test("opening the console page leaves no errors behind", async ({ page }) => {
  await page.goto("/vms/no-such-vm/console");
  await expect(page.locator(".xterm")).toHaveCount(1);
  // Let xterm's queued timers run.
  await page.waitForTimeout(1_000);
});

test("leaving the console page at once leaves no errors behind", async ({ page }) => {
  for (let i = 0; i < 3; i++) {
    await page.goto("/vms/no-such-vm/console");
    await page.getByRole("link", { name: "Back to VM" }).click();
  }
  await page.waitForTimeout(1_000);
});

/** A gRPC-web answer: the message frame, then the trailers frame. */
function grpcWeb(message: object): Buffer {
  const frame = (flag: number, body: Buffer) => {
    const head = Buffer.alloc(5);
    head.writeUInt8(flag, 0);
    head.writeUInt32BE(body.length, 1);
    return Buffer.concat([head, body]);
  };
  return Buffer.concat([
    frame(0x00, Buffer.from(JSON.stringify(message))),
    frame(0x80, Buffer.from("grpc-status: 0\r\n")),
  ]);
}

// Ctrl+Shift+C / Ctrl+Shift+V copy and paste; Ctrl+C stays ^C. Against a
// stand-in console: the ticket RPC and the WebSocket are mocked.
test("Ctrl+Shift+C and Ctrl+Shift+V copy and paste, Ctrl+C is ^C", async ({ page, context }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.route("**/CreateConsoleTicket", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "application/grpc-web+json" },
      body: grpcWeb({ ticket: "t", expiresIn: 60 }),
    }),
  );
  const typed: string[] = [];
  await page.routeWebSocket(/\/console\/ws/, (ws) => {
    ws.send(Buffer.from("guest-banner ready\r\n"));
    ws.onMessage((m) => typed.push(typeof m === "string" ? m : m.toString()));
  });

  await page.goto("/vms/mock-vm/console");
  await expect(page.getByText("connected", { exact: true })).toBeVisible();
  await expect(page.locator(".xterm-rows")).toContainText("guest-banner");

  // Select the word, copy it.
  await page.locator(".xterm-rows").getByText("guest-banner ready").dblclick({ position: { x: 5, y: 5 } });
  await page.keyboard.press("Control+Shift+KeyC");
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe("guest-banner");
  expect(typed).toEqual([]);

  // Paste reaches the guest.
  await page.evaluate(() => navigator.clipboard.writeText("pasted text"));
  await page.keyboard.press("Control+Shift+KeyV");
  await expect.poll(() => typed.join("")).toBe("pasted text");

  // Ctrl+C is still the guest's interrupt.
  await page.keyboard.press("Control+KeyC");
  await expect.poll(() => typed.join("")).toBe("pasted text\x03");
});

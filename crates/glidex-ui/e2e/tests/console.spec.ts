// VmConsole lifecycle, independent of any hypervisor: xterm 5.5 threw
// "reading 'dimensions'" when a terminal was disposed right after open()
// (React StrictMode's double mount in dev, or leaving the page at once).
// The `pageErrors` fixture fails the test on any such error.
import type { Page } from "@playwright/test";
import { expect, test } from "../fixtures";

// There is no such VM: the console WebSocket is refused and its detail
// page gets 404s.
test.use({ ignoredConsoleErrors: /WebSocket|status of 404/ });

for (const kind of ["xterm", "ghostty"]) {
  test.describe(kind, () => {
    test.beforeEach(({ page }) =>
      page.addInitScript((k) => localStorage.setItem("glidex.console.terminal", k), kind),
    );

    test("opening the console page leaves no errors behind", async ({ page }) => {
      await page.goto("/vms/no-such-vm/console");
      await expect(page.getByTestId("console-terminal").locator(kind === "xterm" ? ".xterm" : "canvas")).toHaveCount(1);
      // Let the terminal's queued timers run.
      await page.waitForTimeout(1_000);
    });

    test("leaving the console page at once leaves no errors behind", async ({ page }) => {
      for (let i = 0; i < 3; i++) {
        await page.goto("/vms/no-such-vm/console");
        await page.getByRole("link", { name: "Back to VM" }).click();
      }
      await page.waitForTimeout(1_000);
    });
  });
}

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

/** The console page against a stand-in console: the ticket RPC and the
 *  WebSocket are mocked. The guest prints `banner`; returns what is typed. */
async function mockConsole(page: Page, banner: string): Promise<string[]> {
  await page.route("**/CreateConsoleTicket", (route) =>
    route.fulfill({
      status: 200,
      headers: { "content-type": "application/grpc-web+json" },
      body: grpcWeb({ ticket: "t", expiresIn: 60 }),
    }),
  );
  const typed: string[] = [];
  await page.routeWebSocket(/\/console\/ws/, (ws) => {
    ws.send(Buffer.from(banner));
    ws.onMessage((m) => typed.push(typeof m === "string" ? m : m.toString()));
  });
  return typed;
}

// Both terminals (the console page's picker): Ctrl+Shift+C / Ctrl+Shift+V
// copy and paste; Ctrl+C stays ^C.
for (const kind of ["xterm", "ghostty"]) {
  test(`${kind}: Ctrl+Shift+C and Ctrl+Shift+V copy and paste, Ctrl+C is ^C`, async ({ page, context }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.addInitScript((k) => localStorage.setItem("glidex.console.terminal", k), kind);
    const typed = await mockConsole(page, "hello world\r\n");
    const clipboard = () => page.evaluate(() => navigator.clipboard.readText());

    await page.goto("/vms/mock-vm/console");
    await expect(page.getByText("connected", { exact: true })).toBeVisible();
    await expect(page.getByLabel("Terminal:")).toHaveValue(kind);

    // Select the first word (ghostty-web copies a selection as it is made:
    // clear the clipboard after), then copy it.
    const terminal = page.getByTestId("console-terminal");
    await terminal.dblclick({ position: { x: 12, y: 8 } });
    await page.waitForTimeout(300);
    await page.evaluate(() => navigator.clipboard.writeText(""));
    await page.keyboard.press("Control+Shift+KeyC");
    await expect.poll(clipboard).toBe("hello");
    expect(typed).toEqual([]);

    // Paste reaches the guest.
    await page.evaluate(() => navigator.clipboard.writeText("pasted text"));
    await page.keyboard.press("Control+Shift+KeyV");
    await expect.poll(() => typed.join("")).toBe("pasted text");

    // Ctrl+C is still the guest's interrupt.
    await page.keyboard.press("Control+KeyC");
    await expect.poll(() => typed.join("")).toBe("pasted text\x03");
  });
}

test("the terminal picker switches terminals and is remembered", async ({ page }) => {
  await mockConsole(page, "hello\r\n");
  await page.goto("/vms/mock-vm/console");
  const terminal = page.getByTestId("console-terminal");
  await expect(terminal.locator(".xterm")).toHaveCount(1);

  await page.getByLabel("Terminal:").selectOption("ghostty");
  await expect(page.getByText("connected", { exact: true })).toBeVisible();
  await expect(terminal.locator("canvas")).toHaveCount(1);
  await expect(terminal.locator(".xterm")).toHaveCount(0);

  await page.reload();
  await expect(page.getByLabel("Terminal:")).toHaveValue("ghostty");
  await expect(terminal.locator("canvas")).toHaveCount(1);

  await page.getByLabel("Terminal:").selectOption("xterm");
  await expect(terminal.locator(".xterm")).toHaveCount(1);
  await expect(terminal.locator("canvas")).toHaveCount(0);
});

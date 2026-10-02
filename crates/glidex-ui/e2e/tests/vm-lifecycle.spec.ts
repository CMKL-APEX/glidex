// A real VM driven entirely from the UI: credential and network, create
// with firmware boot, start, console login, pause/resume, graceful shut
// down, delete. Needs KVM, the hypervisor, glidex-netd + OVS and
// GLIDEX_TEST_IMAGE (a UEFI-bootable cloud image, copied first).
import { existsSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { api, bootPrerequisites, CLEAN_POWEROFF, del, E2E_HOME, expect, field, LABEL, test, testImage, vmCard } from "../fixtures";

test("boots a cloud image and runs it from the UI", async ({ page, hypervisor }) => {
  test.setTimeout(10 * 60_000);
  const missing = await bootPrerequisites(hypervisor);
  test.skip(missing.length > 0, missing.join("; "));

  const short = hypervisor === "qemu" ? "qemu" : "ch";
  const vmName = `ui-${short}`;
  const network = `ui-${short}`;
  const username = `ui${short}`;
  const password = `Gx-${Math.random().toString(36).slice(2, 12)}`;
  const disk = `${E2E_HOME}/images/${vmName}.img`;
  execFileSync("cp", ["--sparse=always", testImage()!, disk]);
  const vm = async () => (await api<any[]>("/vms")).find((v) => v.name === vmName);

  try {
    await test.step("add a credential", async () => {
      await page.goto("/credentials");
      await page.getByRole("button", { name: "Add Credential" }).click();
      await page.getByPlaceholder("alice").fill(username);
      const pw = page.locator('input[type="password"]');
      await pw.nth(0).fill(password);
      await pw.nth(1).fill(password);
      await page.getByRole("button", { name: "Add", exact: true }).click();
      await expect(page.getByText(username, { exact: true })).toBeVisible();
    });

    await test.step("add a NAT network", async () => {
      await page.goto("/networking");
      await page.getByRole("button", { name: "Add Network" }).click();
      await page.getByPlaceholder("lab").fill(network);
      await page.getByRole("button", { name: "Create", exact: true }).click();
      await expect(page.getByText(`gxbr-${network}`).first()).toBeVisible({ timeout: 30_000 });
    });

    await test.step("create the VM", async () => {
      await page.goto("/");
      await page.getByRole("button", { name: "+ Create VM" }).click();
      await field(page, "VM Name").fill(vmName);
      await field(page, "Hypervisor Backend").selectOption(hypervisor);
      await field(page, "vCPU Count").fill("2");
      await field(page, "Memory (MiB)").fill("2048");
      // Firmware boot with the form's default firmware path.
      await expect(field(page, "Boot Mode")).toHaveValue("firmware");
      await field(page, "Boot Disk").selectOption("path");
      await field(page, "Disk Image Path").fill(disk);
      await field(page, "Login Credential (optional)").selectOption(username);
      const box = (name: string) => page.locator("label", { hasText: name }).locator('input[type="checkbox"]');
      if (await box("default").count()) await box("default").uncheck();
      await box(network).check();
      await page.getByRole("button", { name: "Create VM", exact: true }).click();
      await expect(page.getByText(vmName, { exact: true })).toBeVisible();

      const created = await vm();
      expect(created.hypervisor).toBe(hypervisor);
      const full = await api(`/vms/${created.id}`);
      expect(full.nics.map((n: any) => n.network)).toEqual([network]);
    });

    let ip = "";
    await test.step("start it; the NIC gets an address", async () => {
      await expect(vmCard(page, vmName)).toContainText(LABEL[hypervisor]);
      await vmCard(page, vmName).getByRole("link", { name: "View Details" }).click();
      await expect(page.getByText(LABEL[hypervisor], { exact: true })).toBeVisible();
      await page.getByRole("button", { name: "Start" }).click();
      await expect(page.getByRole("button", { name: "Shut down" })).toBeVisible({ timeout: 60_000 });
      await expect(page.getByRole("button", { name: "Stop" })).toBeVisible();
      const addr = page.getByText(/^10\.88\.\d+\.\d+$/);
      await expect(addr).toBeVisible({ timeout: 30_000 });
      ip = (await addr.textContent())!.trim();
      // QEMU keeps a private UEFI variable store per VM; CH has none.
      const vars = `${E2E_HOME}/.glidex/firmware-vars/${(await vm()).id}.fd`;
      expect(existsSync(vars)).toBe(hypervisor === "qemu");
    });

    await test.step("log in on the web console", async () => {
      await page.getByText("Open Console").click();
      await expect(page).toHaveURL(/\/console$/);
      const screen = page.locator(".xterm-rows");
      await expect(page.getByText("connected", { exact: true })).toBeVisible();
      // Nudge getty until the login prompt is on screen.
      await expect
        .poll(async () => {
          if ((await screen.innerText()).includes(`${vmName} login:`)) return true;
          await page.locator(".xterm").click();
          await page.keyboard.press("Enter");
          return false;
        }, { timeout: 300_000, intervals: [2_000] })
        .toBe(true);
      await page.locator(".xterm").click();
      await page.keyboard.type(`${username}\n`, { delay: 20 });
      await expect(screen).toContainText("Password:", { timeout: 30_000 });
      await page.keyboard.type(`${password}\n`, { delay: 20 });
      await expect(screen).toContainText(`${username}@${vmName}:~$`, { timeout: 60_000 });
      // The guest has the address the UI showed.
      await page.keyboard.type(`ip -4 -o addr | grep -q ' ${ip}/' && echo GX_UI_$((40+2))\n`, { delay: 10 });
      await expect(screen).toContainText("GX_UI_42", { timeout: 30_000 });
    });

    await test.step("pause and resume", async () => {
      await page.goBack();
      await page.getByRole("button", { name: "Pause" }).click();
      await expect(page.getByRole("button", { name: "Resume" })).toBeVisible();
      // The power button can't reach a paused guest.
      await expect(page.getByRole("button", { name: "Shut down" })).toBeHidden();
      await expect(page.getByRole("button", { name: "Stop" })).toBeVisible();
      await page.getByRole("button", { name: "Resume" }).click();
      await expect(page.getByRole("button", { name: "Shut down" })).toBeVisible();
    });

    await test.step("shut down through the power button", async () => {
      await page.getByRole("button", { name: "Shut down" }).click();
      await expect(page.getByRole("button", { name: "Start" })).toBeVisible({ timeout: 90_000 });
      const stopped = await vm();
      expect(stopped.state).toBe("stopped");
      const log = await api<string>(`/vms/${stopped.id}/console/log`);
      expect(log, "guest powered off cleanly").toMatch(CLEAN_POWEROFF);
    });

    await test.step("delete it", async () => {
      await page.getByRole("button", { name: "Delete" }).click();
      await expect(page).toHaveURL(/\/$/);
      await expect.poll(vm).toBeUndefined();
    });
  } finally {
    const left = await vm();
    if (left) await del(`/vms/${left.id}`);
    await del(`/networks/${network}`);
    await del(`/credentials/${username}`);
    rmSync(disk, { force: true });
  }
});

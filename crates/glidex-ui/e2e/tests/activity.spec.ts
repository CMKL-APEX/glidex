// Reconciliation in progress (spec/reconciliation.md): writes are carried
// out by the controllers, and the UI says what they are still doing.
import { api, del, expect, test, vmCard } from "../fixtures";

const NAME = "e2e-activity";

async function cleanup() {
  for (const v of await api<{ id: string; name: string }[]>("/vms")) {
    if (v.name === NAME) await del(`/vms/${v.id}?wait=30`);
  }
}
test.beforeEach(cleanup);
test.afterEach(cleanup);

test("the header and the VM show what the controller is still doing", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByText("Up to date")).toBeVisible();

  // A VM that can't launch: the controller keeps trying, with backoff.
  const vm = await api<{ id: string }>("/vms", {
    method: "POST",
    body: { name: NAME, vcpu_count: 1, mem_size_mib: 256, kernel_image_path: "/nonexistent/vmlinux", rootfs_path: "/nonexistent/disk.img" },
  });
  // Written outside this browser: it arrives over the live stream
  // (GET /watch), with no reload and well before the 15 s idle poll.
  await api(`/vms/${vm.id}/start`, { method: "POST" });

  const indicator = page.getByRole("button", { name: "Reconciling" });
  await expect(indicator).toContainText("1 in progress");
  await expect(vmCard(page, NAME)).toContainText("Starting");
  await indicator.click();
  await expect(page.getByRole("link", { name: new RegExp(NAME) })).toContainText("Starting");
  await page.getByRole("link", { name: new RegExp(NAME) }).click();
  await expect(page.getByText("In progress:")).toBeVisible();

  // Stopped: nothing left to do.
  await api(`/vms/${vm.id}/stop?wait=30`, { method: "POST" });
  await expect(page.getByText("Up to date")).toBeVisible({ timeout: 20_000 });
  await expect(page.getByText("In progress:")).toHaveCount(0);
});

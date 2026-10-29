// A VM's service account (spec/29-workload-identity.md W5): the create form
// and the VM page. The project's service accounts are mocked; what the UI
// sends is what is checked.
import { api, createVm, del, expect, field, listVms, RpcCode, rpcFetch, rpcRoute, test, waitForRpc } from "../fixtures";

const name = (hv: string) => `identity-${hv}`;

test.afterEach(async ({ hypervisor }) => {
  const vm = (await listVms()).find((v) => v.name === name(hypervisor));
  if (vm) await del(`/vms/${vm.id}?wait=60`);
});

/** A service account "ci" in each project the caller sees (`sa-<project>`),
 * so the session's project has one, whichever it is. */
async function mockServiceAccounts(page: import("@playwright/test").Page) {
  const projects = (await api<{ projects?: { id: string }[] }>("/projects")).projects ?? [];
  await rpcRoute(page, "glidex.access.v1.TokenService/ListTokens", () => ({
    message: {
      tokens: projects.map((p) => ({ id: `sa-${p.id}`, name: "ci", kind: "TOKEN_KIND_SERVICE_ACCOUNT", project: p.id })),
    },
  }));
}

test("the create form gives a VM a service account", async ({ page, hypervisor }) => {
  await mockServiceAccounts(page);
  await page.goto("/");
  await page.getByRole("button", { name: "+ Create VM" }).click();
  const picker = page.getByTestId("service-account");
  await expect(picker).toBeVisible();
  await picker.selectOption({ label: "ci" });
  await field(page, "VM Name").fill(name(hypervisor));
  await field(page, "Hypervisor Backend").selectOption(hypervisor);
  await field(page, "Boot Disk").selectOption("path");
  await field(page, "Disk Image Path").fill("/nonexistent/disk.img");
  // The control plane would refuse the mocked id: only the request matters.
  await rpcRoute(page, "glidex.vm.v1.VmService/CreateVm", () => ({
    error: { code: RpcCode.INVALID_ARGUMENT, reason: "invalid_config", message: "mocked", httpStatus: 400 },
  }));
  const [body] = await Promise.all([
    waitForRpc(page, "glidex.vm.v1.VmService/CreateVm"),
    page.getByRole("button", { name: "Create VM", exact: true }).click(),
  ]);
  expect(body.service_account).toMatch(/^sa-/);
});

test("the VM page changes the service account with a JSON field mask", async ({ page, hypervisor }) => {
  const vm = await createVm({
    name: name(hypervisor),
    vcpu_count: 1,
    mem_size_mib: 256,
    kernel_image_path: "/nonexistent/vmlinux",
    rootfs_path: "/nonexistent/disk.img",
  });
  expect(vm.id, JSON.stringify(vm)).toBeTruthy();
  await mockServiceAccounts(page);
  await rpcRoute(page, "glidex.vm.v1.VmService/GetVm", async (_, route) => {
    const real = await rpcFetch(route);
    return real?.vm ? { message: { vm: { ...real.vm, service_account: `sa-${real.vm.project}` } } } : undefined;
  });
  await rpcRoute(page, "glidex.vm.v1.VmService/UpdateVm", () => ({ message: {} }));

  await page.goto(`/vms/${vm.id}`);
  const panel = page.getByTestId("vm-service-account");
  await expect(panel).toContainText("ci");
  await panel.getByLabel("Service account").selectOption("");
  const [request] = await Promise.all([
    page.waitForRequest((r) => r.url().endsWith("/api/glidex.vm.v1.VmService/UpdateVm")),
    panel.getByRole("button", { name: "Use" }).click(),
  ]);
  const raw = request.postDataBuffer()!;
  const sent = JSON.parse(raw.subarray(5, 5 + raw.readUInt32BE(1)).toString("utf8"));
  // Taken away: the field is absent, its path in the mask (lowerCamelCase).
  expect(sent).toMatchObject({ id: vm.id, update_mask: "serviceAccount" });
  expect(sent.change?.service_account).toBeUndefined();
});

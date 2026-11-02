// Attaching and detaching a VM's networks from its page (user-guide/vms.md,
// Networks), against the real control plane: a VM that is never started,
// and an isolated network every project may use.
import { api, createVm, del, expect, listVms, test } from "../fixtures";

const vmName = (hv: string) => `vmnet-${hv}`;
const netName = (hv: string) => `vmnet-e2e-${hv}`;

test.afterEach(async ({ hypervisor }) => {
  const vm = (await listVms()).find((v) => v.name === vmName(hypervisor));
  if (vm) await del(`/vms/${vm.id}?wait=60`);
  const nets = (await api<{ networks?: any[] }>("/networks")).networks ?? [];
  for (const n of nets.filter((x) => x.name === netName(hypervisor))) await del(`/networks/${n.id}?wait=30`);
});

test("the VM page attaches and detaches networks", async ({ page, hypervisor }) => {
  const status = await api("/ovs/status");
  test.skip(status?.netd?.access !== "NETD_ACCESS_FULL" || status?.host?.ovs_running !== true, "needs glidex-netd and a running Open vSwitch");
  const made = await api("/networks", {
    method: "POST",
    body: { name: netName(hypervisor), mode: "NETWORK_MODE_ISOLATED", all_projects: true },
  });
  expect(made?.network?.id, JSON.stringify(made)).toBeTruthy();
  const netId: string = made.network.id;
  const vm = await createVm({
    name: vmName(hypervisor),
    vcpu_count: 1,
    mem_size_mib: 256,
    kernel_image_path: "/nonexistent/vmlinux",
    rootfs_path: "/nonexistent/disk.img",
  });
  expect(vm.id, JSON.stringify(vm)).toBeTruthy();
  const nics = async (): Promise<{ network: string; mac: string }[]> => (await api(`/vms/${vm.id}`)).vm.nics ?? [];

  await page.goto(`/vms/${vm.id}`);
  const section = page.getByTestId("vm-networks");
  const attach = async () => {
    await section.getByLabel("Network to attach").selectOption(netId);
    await section.getByRole("button", { name: "Attach", exact: true }).click();
  };

  await test.step("attach the network twice", async () => {
    await attach();
    await expect(section.getByRole("button", { name: "Detach" })).toHaveCount(1);
    await attach();
    await expect(section.getByRole("button", { name: "Detach" })).toHaveCount(2);
    const got = await nics();
    expect(got.map((n) => n.network)).toEqual([netId, netId]);
    expect(new Set(got.map((n) => n.mac)).size).toBe(2);
  });

  await test.step("detach the first NIC; the other keeps its MAC", async () => {
    const [, kept] = await nics();
    // Its confirm() is accepted by the fixture.
    await section.getByRole("button", { name: "Detach" }).first().click();
    await expect(section.getByRole("button", { name: "Detach" })).toHaveCount(1);
    expect((await nics()).map((n) => n.mac)).toEqual([kept.mac]);
    await expect(section.getByText(kept.mac)).toBeVisible();
  });
});

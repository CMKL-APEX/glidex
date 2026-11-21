// The Host page's topology shows each device's ATS, the Direct Translated
// P2P of the ports above it, and the switch-local pairs with where
// translated requests go (spec/22-host-topology.md H31, §5.2a), with what
// glidex set for a trusted pool (H33, §5.3b). The topology is mocked: it
// needs a switch with devices on vfio-pci.
import { api, expect, rpcRoute, test } from "../fixtures";

const port = (address: string, parent: string, kind: string, dt: string) => ({
  address,
  parent,
  kind,
  class_id: "0x060400",
  direct_translated: dt,
});

const device = (address: string, parent: string, cls: string, ats: string) => ({
  address,
  parent,
  kind: "PCI_KIND_ENDPOINT",
  class_id: cls,
  vendor_id: "0x10de",
  device_id: "0x20b0",
  driver: "vfio-pci",
  numa_node: 0,
  switch: "0000:81:00.0",
  assignable: true,
  ready: true,
  ats,
});

test("the topology shows ATS and the switch-local P2P pairs", async ({ page }) => {
  const nodes = (await api<{ nodes?: any[] }>("/nodes")).nodes ?? [];
  const id = nodes[0]?.meta?.id;
  expect(id).toBeTruthy();
  await rpcRoute(page, "glidex.cluster.v1.NodeService/GetNodeTopology", () => ({
    message: {
      topology: {
        summary: { numa_nodes: [{ cores: [{ threads: [0, 1] }], memory_mib: "16384" }] },
        devices: [
          port("0000:80:01.1", "pci0000:80", "PCI_KIND_ROOT_PORT", "unsupported"),
          { address: "0000:81:00.0", parent: "0000:80:01.1", kind: "PCI_KIND_SWITCH_UPSTREAM", class_id: "0x060400" },
          { ...port("0000:82:00.0", "0000:81:00.0", "PCI_KIND_SWITCH_DOWNSTREAM", "on"), direct_translated_set: true },
          port("0000:82:01.0", "0000:81:00.0", "PCI_KIND_SWITCH_DOWNSTREAM", "off"),
          device("0000:87:00.0", "0000:82:00.0", "0x030200", "on"),
          device("0000:88:00.0", "0000:82:01.0", "0x020000", "on"),
        ],
        p2p: [
          {
            a: "0000:87:00.0",
            b: "0000:88:00.0",
            switch: "0000:81:00.0",
            a_to_b: { route: "direct", set_for: "train-1" },
            b_to_a: { route: "via_root", reason: "0000:82:01.0 has Direct Translated P2P off" },
          },
        ],
      },
    },
  }));
  await page.goto(`/hosts/${id}`);
  const gpu = page.locator("li").filter({ hasText: "0000:87:00.0" }).filter({ hasText: "ATS on" });
  await expect(gpu).toBeVisible();
  await expect(gpu).toContainText("Direct Translated P2P: 0000:82:00.0 on (set by glidex), 0000:80:01.1 unsupported");
  const pairs = page.getByTestId("switch-local-p2p");
  await expect(pairs.getByRole("listitem")).toHaveCount(2);
  const there = pairs.getByRole("listitem").filter({ hasText: "0000:87:00.0 → 0000:88:00.0" });
  await expect(there).toContainText("direct");
  await expect(there).toContainText("set by glidex for train-1");
  const back = pairs.getByRole("listitem").filter({ hasText: "0000:88:00.0 → 0000:87:00.0" });
  await expect(back).toContainText("via root complex");
  await expect(back).toContainText("0000:82:01.0 has Direct Translated P2P off");
  await expect(back).not.toContainText("set by glidex");
});

test("a topology without pairs shows no switch-local P2P", async ({ page }) => {
  const nodes = (await api<{ nodes?: any[] }>("/nodes")).nodes ?? [];
  const id = nodes[0]?.meta?.id;
  // Mocked too: a test host may well have pairs of its own.
  await rpcRoute(page, "glidex.cluster.v1.NodeService/GetNodeTopology", () => ({
    message: {
      topology: {
        summary: { numa_nodes: [{ cores: [{ threads: [0, 1] }], memory_mib: "16384" }] },
        devices: [device("0000:87:00.0", "0000:82:00.0", "0x030200", "unsupported")],
      },
    },
  }));
  await page.goto(`/hosts/${id}`);
  await expect(page.getByText(/^Topology \(/)).toBeVisible();
  await expect(page.getByTestId("switch-local-p2p")).toHaveCount(0);
});

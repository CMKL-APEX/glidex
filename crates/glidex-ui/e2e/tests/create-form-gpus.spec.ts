// The create form's GPUs section (spec/27-gpu-vms.md §3): pools and the
// node's own GPUs, from mocked pools and topology, so any host will do.
import { expect, field, rpcRoute, test } from "../fixtures";

const NODE_SVC = "glidex.cluster.v1.NodeService";

const gpu = (address: string, vendor: string, extra: object) => ({
  address,
  vendor_id: vendor,
  device_id: "0x7551",
  class_id: "0x030000",
  numa_node: 0,
  assignable: true,
  ...extra,
});

test.use({ ignoredConsoleErrors: /status of 409/ });

test.beforeEach(async ({ page }) => {
  await rpcRoute(page, `${NODE_SVC}/ListPciPools`, () => ({
    message: {
      pools: [
        { name: "r9700", class: "gpu", free: 2, per_device_vcpus: 6, per_device_memory_mib: 10240 },
        { name: "cx7-vfs", class: "nic", free: 4 },
      ],
    },
  }));
  await rpcRoute(page, `${NODE_SVC}/GetNodeTopology`, () => ({
    message: {
      topology: {
        devices: [
          gpu("0000:03:00.0", "0x1002", { driver: "vfio-pci", ready: true }),
          gpu("0000:43:00.0", "0x1002", { driver: "vfio-pci", ready: true, taken: "PCI device 0000:43:00.0 is used by VM gpu-a" }),
          gpu("0000:c6:00.0", "0x1a03", { driver: "ast", ready: false, not_ready: ["not bound to vfio-pci (driver: ast)"] }),
          // Neither is a GPU a VM could be given.
          { address: "0000:c4:00.0", vendor_id: "0x10de", class_id: "0x068000", assignable: false, fabric: true },
          { address: "0000:41:00.0", vendor_id: "0x15b3", class_id: "0x020000", assignable: true, ready: true },
        ],
      },
    },
  }));
  await page.goto("/");
  await page.getByRole("button", { name: "+ Create VM" }).click();
});

test("lists GPU pools and the node's GPUs, and sends what is picked", async ({ page }) => {
  await expect(page.getByText("GPUs (optional)")).toBeVisible();
  // The GPU pool is here; the NIC pool stays under Device Pools.
  await expect(page.getByLabel("GPUs from r9700")).toBeVisible();
  await expect(page.getByLabel("Devices from cx7-vfs")).toBeVisible();
  await expect(page.getByLabel("Devices from r9700")).toHaveCount(0);
  // The node's GPUs: the free one can be picked, the others say why not.
  const ready = page.getByLabel("GPU 0000:03:00.0");
  await expect(ready).toBeEnabled();
  await expect(page.getByText("available", { exact: true })).toBeVisible();
  await expect(page.getByLabel("GPU 0000:43:00.0")).toBeDisabled();
  await expect(page.getByText("PCI device 0000:43:00.0 is used by VM gpu-a")).toBeVisible();
  await expect(page.getByLabel("GPU 0000:c6:00.0")).toBeDisabled();
  await expect(page.getByText("not bound to vfio-pci (driver: ast)")).toBeVisible();
  await expect(page.getByLabel("GPU 0000:c4:00.0")).toHaveCount(0);
  await expect(page.getByLabel("GPU 0000:41:00.0")).toHaveCount(0);

  await ready.check();
  await page.getByLabel("GPUs from r9700").fill("1");
  // The pool sizes the VM.
  await expect(field(page, "vCPU Count")).toHaveValue("6");
  await expect(field(page, "Memory (MiB)")).toHaveValue("10240");
  await expect(page.getByText("2 GPUs; the vCPU and memory fields follow the pool's sizing.")).toBeVisible();

  await field(page, "VM Name").fill("gpu-form");
  await field(page, "Boot Disk").selectOption("path");
  await field(page, "Disk Image Path").fill("/nonexistent/disk.img");
  let sent: any;
  await rpcRoute(page, "glidex.vm.v1.VmService/CreateVm", (req) => {
    sent = req;
    return { error: { code: 6, reason: "conflict", message: "not this time", httpStatus: 409 } };
  });
  await page.getByRole("button", { name: "Create VM", exact: true }).click();
  await expect(page.getByText("not this time")).toBeVisible();
  expect(sent).toMatchObject({
    vfio_devices: ["/sys/bus/pci/devices/0000:03:00.0"],
    gpus: { count: 1, pool: "r9700" },
  });
});

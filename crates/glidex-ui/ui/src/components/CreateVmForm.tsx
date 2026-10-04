import { useRef, useEffect, useState, type FormEvent } from "react";
import { listCredentials, listDisks, listImages, listNetworks } from "../api";
import type { CreateVmRequest, CredentialInfo, DiskInfo, HypervisorType, ImageInfo, Network } from "../types";
import { HYPERVISOR_LABELS, formatBytes, networkUsableBy } from "../types";
import { useSession } from "../session";

type BootMode = "firmware" | "kernel";

/** Where each hypervisor's UEFI firmware usually lives. */
const DEFAULT_FIRMWARE: Record<HypervisorType, string> = {
  cloudhypervisor: "~/.glidex/CLOUDHV.fd",
  qemu: "/usr/share/OVMF/OVMF_CODE_4M.fd",
};

const FIRMWARE_HINT: Record<HypervisorType, string> = {
  cloudhypervisor: "Downloaded by glidex-install",
  qemu: "OVMF code image from the ovmf / edk2-ovmf package",
};
/** Where a firmware-boot VM's root disk comes from (spec/images.md §7). */
type RootSource = "image" | "disk" | "path";

interface CreateVmFormProps {
  onSubmit: (request: CreateVmRequest) => void;
  onCancel: () => void;
}

export default function CreateVmForm({ onSubmit, onCancel }: CreateVmFormProps) {
  const { project, me } = useSession();
  const [name, setName] = useState("");
  const [vcpuCount, setVcpuCount] = useState(1);
  const [memSizeMib, setMemSizeMib] = useState(512);
  const [hypervisor, setHypervisor] = useState<HypervisorType>("cloudhypervisor");
  const [bootMode, setBootMode] = useState<BootMode>("firmware");
  const [firmwarePath, setFirmwarePath] = useState(DEFAULT_FIRMWARE.cloudhypervisor);
  const [credential, setCredential] = useState("");
  // Set once the user picks a credential, so a late load never overrides it.
  const credentialChosen = useRef(false);
  const [credentials, setCredentials] = useState<CredentialInfo[]>([]);
  const [networks, setNetworks] = useState<Network[]>([]);
  const [selectedNetworks, setSelectedNetworks] = useState<string[]>([]);
  const [kernelPath, setKernelPath] = useState("");
  const [rootfsPath, setRootfsPath] = useState("");
  const [kernelArgs, setKernelArgs] = useState("");
  const [vfioDevices, setVfioDevices] = useState("");
  const [images, setImages] = useState<ImageInfo[]>([]);
  const [freeDisks, setFreeDisks] = useState<DiskInfo[]>([]);
  const [rootSource, setRootSource] = useState<RootSource>("path");
  const [image, setImage] = useState("");
  const [rootSizeGib, setRootSizeGib] = useState("");
  const [rootDisk, setRootDisk] = useState("");
  const [submitting, setSubmitting] = useState(false);

  const firmware = bootMode === "firmware";

  // Follow the hypervisor's default firmware unless the user typed one.
  const changeHypervisor = (next: HypervisorType) => {
    if (firmwarePath === DEFAULT_FIRMWARE[hypervisor]) setFirmwarePath(DEFAULT_FIRMWARE[next]);
    setHypervisor(next);
  };

  useEffect(() => {
    listCredentials(project)
      .then((creds) => {
        setCredentials(creds);
        // Default to the signed-in user's own credential, if the project
        // has one by that name.
        const mine = me.user?.display_name;
        if (!credentialChosen.current && mine && creds.some((c) => c.username === mine)) {
          setCredential(mine);
        }
      })
      .catch(() => setCredentials([]));
    listNetworks()
      .then((all) => {
        // Only networks this project may attach to.
        const nets = all.filter((n) => networkUsableBy(n, project));
        setNetworks(nets);
        if (nets.some((n) => n.name === "default")) setSelectedNetworks(["default"]);
      })
      .catch(() => setNetworks([]));
    listImages()
      .then((imgs) => {
        const ready = imgs.filter((i) => i.status.state === "ready");
        setImages(ready);
        if (ready.length > 0) {
          setImage(ready[0].id);
          setRootSource("image");
        }
      })
      .catch(() => setImages([]));
    listDisks(project)
      .then((ds) => {
        // Not another VM's (or going away); a pending disk is fine, the VM
        // starts once it's made.
        const free = ds.filter((d) => !d.attached_to && !d.owner && !d.deleting && d.status !== "failed" && d.status !== "missing");
        setFreeDisks(free);
        if (free.length > 0) setRootDisk(free[0].id);
      })
      .catch(() => setFreeDisks([]));
  }, [project, me.user?.display_name]);

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);

    const devices = vfioDevices
      .split(",")
      .map((d) => d.trim())
      .filter((d) => d.length > 0);

    onSubmit({
      name,
      vcpu_count: vcpuCount,
      mem_size_mib: memSizeMib,
      kernel_image_path: firmware ? "" : kernelPath,
      firmware_path: firmware ? firmwarePath : undefined,
      credential: firmware && credential ? credential : undefined,
      networks:
        selectedNetworks.length > 0 ? selectedNetworks.map((network) => ({ network })) : undefined,
      rootfs_path: !firmware || rootSource === "path" ? rootfsPath : undefined,
      image: firmware && rootSource === "image" ? image : undefined,
      root_disk_size_gib:
        firmware && rootSource === "image" && rootSizeGib ? Number(rootSizeGib) : undefined,
      root_disk: firmware && rootSource === "disk" ? rootDisk : undefined,
      hypervisor,
      kernel_args: !firmware && kernelArgs ? kernelArgs : undefined,
      vfio_devices: devices.length > 0 ? devices : undefined,
    });
  };

  return (
    <form onSubmit={handleSubmit} className="space-y-4">
      <div>
        <label className="block text-sm font-medium text-gray-700">
          VM Name
        </label>
        <input
          type="text"
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
          placeholder="my-vm"
          required
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-700">
          Hypervisor Backend
        </label>
        <select
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
          value={hypervisor}
          onChange={(e) => changeHypervisor(e.target.value as HypervisorType)}
        >
          {(Object.entries(HYPERVISOR_LABELS) as [HypervisorType, string][]).map(
            ([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ),
          )}
        </select>
      </div>

      <div className="grid grid-cols-2 gap-4">
        <div>
          <label className="block text-sm font-medium text-gray-700">
            vCPU Count
          </label>
          <input
            type="number"
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            min="1"
            max="32"
            value={vcpuCount}
            onChange={(e) => setVcpuCount(Number(e.target.value))}
          />
        </div>
        <div>
          <label className="block text-sm font-medium text-gray-700">
            Memory (MiB)
          </label>
          <input
            type="number"
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            min="128"
            max="32768"
            value={memSizeMib}
            onChange={(e) => setMemSizeMib(Number(e.target.value))}
          />
        </div>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-700">
          Boot Mode
        </label>
        <select
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
          value={bootMode}
          onChange={(e) => setBootMode(e.target.value as BootMode)}
        >
          <option value="firmware">UEFI firmware (distro cloud image)</option>
          <option value="kernel">Direct kernel boot</option>
        </select>
      </div>

      {firmware ? (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-700">
              UEFI Firmware Path
            </label>
            <input
              type="text"
              className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
              required
              value={firmwarePath}
              onChange={(e) => setFirmwarePath(e.target.value)}
            />
            <p className="mt-1 text-xs text-gray-500">
              {FIRMWARE_HINT[hypervisor]}
            </p>
          </div>

          <div>
            <label className="block text-sm font-medium text-gray-700">
              Boot Disk
            </label>
            <select
              className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
              value={rootSource}
              onChange={(e) => setRootSource(e.target.value as RootSource)}
            >
              <option value="image" disabled={images.length === 0}>
                New disk from an image{images.length === 0 ? " (pull one on the Images page)" : ""}
              </option>
              <option value="disk" disabled={freeDisks.length === 0}>
                Existing disk{freeDisks.length === 0 ? " (none unattached)" : ""}
              </option>
              <option value="path">Disk image file path</option>
            </select>
          </div>

          {rootSource === "image" && (
            <div className="grid grid-cols-2 gap-4">
              <div>
                <label className="block text-sm font-medium text-gray-700">Image</label>
                <select
                  className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
                  value={image}
                  onChange={(e) => setImage(e.target.value)}
                >
                  {images.map((i) => (
                    <option key={i.id} value={i.id}>
                      {i.name} ({formatBytes(i.virtual_size_bytes)})
                    </option>
                  ))}
                </select>
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-700">Root disk (GiB)</label>
                <input
                  type="number"
                  min="1"
                  className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
                  placeholder="10"
                  value={rootSizeGib}
                  onChange={(e) => setRootSizeGib(e.target.value)}
                />
              </div>
              <p className="col-span-2 -mt-2 text-xs text-gray-500">
                A linked disk is created for this VM, its root partition extended, and deleted with the VM.
              </p>
            </div>
          )}

          {rootSource === "disk" && (
            <div>
              <label className="block text-sm font-medium text-gray-700">Disk</label>
              <select
                className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
                value={rootDisk}
                onChange={(e) => setRootDisk(e.target.value)}
              >
                {freeDisks.map((d) => (
                  <option key={d.id} value={d.id}>
                    {d.name} ({formatBytes(d.size_bytes)})
                  </option>
                ))}
              </select>
            </div>
          )}

          {rootSource === "path" && (
            <div>
              <label className="block text-sm font-medium text-gray-700">
                Disk Image Path
              </label>
              <input
                type="text"
                className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
                placeholder="~/images/ubuntu-cloudimg.raw"
                required
                value={rootfsPath}
                onChange={(e) => setRootfsPath(e.target.value)}
              />
              <p className="mt-1 text-xs text-gray-500">
                UEFI-bootable image file (raw or qcow2), used as is
              </p>
            </div>
          )}

          <div>
            <label className="block text-sm font-medium text-gray-700">
              Login Credential
            </label>
            <select
              className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
              value={credential}
              onChange={(e) => {
                credentialChosen.current = true;
                setCredential(e.target.value);
              }}
            >
              <option value="">
                {credentials.length === 0
                  ? "No login available"
                  : "None (no login)"}
              </option>
              {credentials.map((c) => (
                <option key={c.username} value={c.username}>
                  {c.username}
                </option>
              ))}
            </select>
            <p className="mt-1 text-xs text-gray-500" data-testid="credential-hint">
              {credentials.length === 0
                ? "This project has no credentials, so the VM will have no way to log in. Add one on the Credentials page first."
                : credential
                  ? "Provisioned by cloud-init on first boot."
                  : "Without a credential the VM has no way to log in. Manage credentials on the Credentials page."}
            </p>
          </div>
        </>
      ) : (
        <>
        <div>
          <label className="block text-sm font-medium text-gray-700">
            Kernel Image Path
          </label>
          <input
            type="text"
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            placeholder="/path/to/vmlinux"
            required
            value={kernelPath}
            onChange={(e) => setKernelPath(e.target.value)}
          />
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700">
            Root Filesystem Path
          </label>
          <input
            type="text"
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            placeholder="/path/to/rootfs.ext4"
            required
            value={rootfsPath}
            onChange={(e) => setRootfsPath(e.target.value)}
          />
        </div>

        <div>
          <label className="block text-sm font-medium text-gray-700">
            Kernel Arguments (optional)
          </label>
          <input
            type="text"
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            placeholder="root=/dev/vda reboot=k panic=1"
            value={kernelArgs}
            onChange={(e) => setKernelArgs(e.target.value)}
          />
        </div>
        </>
      )}

      {networks.length > 0 && (
        <div>
          <label className="block text-sm font-medium text-gray-700">
            Networks
          </label>
          <div className="mt-1 flex flex-wrap gap-3">
            {networks.map((n) => (
              <label key={n.name} className="flex items-center space-x-2 text-sm">
                <input
                  type="checkbox"
                  checked={selectedNetworks.includes(n.name)}
                  onChange={(e) =>
                    setSelectedNetworks((cur) =>
                      e.target.checked ? [...cur, n.name] : cur.filter((x) => x !== n.name),
                    )
                  }
                />
                <span className="font-mono">{n.name}</span>
                <span className="text-gray-400">({n.mode})</span>
              </label>
            ))}
          </div>
          <p className="mt-1 text-xs text-gray-500">
            One NIC per selected network, in this order. Manage networks on the
            Networking page.
          </p>
        </div>
      )}

      <div>
        <label className="block text-sm font-medium text-gray-700">
          VFIO PCI Devices (optional)
        </label>
        <input
          type="text"
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
          placeholder="/sys/bus/pci/devices/0000:41:00.0"
          value={vfioDevices}
          onChange={(e) => setVfioDevices(e.target.value)}
        />
        <p className="mt-1 text-xs text-gray-500">
          Comma-separated VFIO device paths for GPU passthrough
        </p>
      </div>

      <div className="flex justify-end space-x-3 pt-4">
        <button
          type="button"
          className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg transition-colors"
          onClick={onCancel}
        >
          Cancel
        </button>
        <button
          type="submit"
          className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg transition-colors disabled:opacity-50"
          disabled={submitting}
        >
          {submitting ? "Creating..." : "Create VM"}
        </button>
      </div>
    </form>
  );
}

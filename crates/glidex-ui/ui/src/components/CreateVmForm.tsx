import { useRef, useEffect, useState, type FormEvent } from "react";
import { listCredentials, listDisks, listImages, listNetworks, listProjectPciPools, pullImage } from "../api";
import type { PciPoolJson } from "../gen/glidex/cluster/v1/cluster_pb";
import { Link } from "react-router-dom";
import type { CreateVmRequest, CredentialInfo, DiskInfo, HypervisorType, ImageInfo, Network } from "../types";
import { HYPERVISOR_LABELS, HYPERVISOR_TO_PROTO, archLabel, firmwareFor, formatBytes, imageState, isDiskImage, isKernel, networkModeLabel, networkUsableBy } from "../types";
import { useSession } from "../session";
import { useNodes } from "../nodes";
import PlacementPicker from "./PlacementPicker";

type BootMode = "firmware" | "kernel";
/** Where a kernel-boot VM's kernel comes from (spec/kernel-boot.md §6). */
type KernelSource = "image" | "path";

/** The firmware catalog entry to pull for each hypervisor (Images page). */
const FIRMWARE_KEY: Record<HypervisorType, string> = {
  cloudhypervisor: "cloudhv-edk2",
  qemu: "ovmf",
};
/** Where a VM's root disk comes from (spec/images.md §7). */
type RootSource = "image" | "disk" | "path";

interface CreateVmFormProps {
  /** Rejects with the error to show in the form. */
  onSubmit: (request: CreateVmRequest) => Promise<void>;
  onCancel: () => void;
}

export default function CreateVmForm({ onSubmit, onCancel }: CreateVmFormProps) {
  const { project, me } = useSession();
  const [name, setName] = useState("");
  const [vcpuCount, setVcpuCount] = useState(1);
  const [memSizeMib, setMemSizeMib] = useState(512);
  const [hypervisor, setHypervisor] = useState<HypervisorType>("cloudhypervisor");
  const [bootMode, setBootMode] = useState<BootMode>("firmware");
  /** Firmware image id; "" until the images load (or none exists). */
  const [firmwareImage, setFirmwareImage] = useState("");
  const [credential, setCredential] = useState("");
  // Set once the user picks a credential, so a late load never overrides it.
  const credentialChosen = useRef(false);
  const [credentials, setCredentials] = useState<CredentialInfo[]>([]);
  const [networks, setNetworks] = useState<Network[]>([]);
  const [selectedNetworks, setSelectedNetworks] = useState<string[]>([]);
  const [kernelPath, setKernelPath] = useState("");
  const [initrdPath, setInitrdPath] = useState("");
  const [kernelSource, setKernelSource] = useState<KernelSource>("image");
  /** Kernel image id; "" until chosen. */
  const [kernelImage, setKernelImage] = useState("");
  const [kernelImages, setKernelImages] = useState<ImageInfo[]>([]);
  const [followGuest, setFollowGuest] = useState(false);
  const [extracting, setExtracting] = useState<string | null>(null);
  const [rootfsPath, setRootfsPath] = useState("");
  const [kernelArgs, setKernelArgs] = useState("");
  const [vfioDevices, setVfioDevices] = useState("");
  const [pools, setPools] = useState<PciPoolJson[]>([]);
  /** Devices asked of each pool, and whether under one PCIe switch. */
  const [poolAsks, setPoolAsks] = useState<Record<string, { count: number; sameSwitch: boolean }>>({});
  const [images, setImages] = useState<ImageInfo[]>([]);
  const [firmwareImages, setFirmwareImages] = useState<ImageInfo[]>([]);
  const [freeDisks, setFreeDisks] = useState<DiskInfo[]>([]);
  const [rootSource, setRootSource] = useState<RootSource>("path");
  const [image, setImage] = useState("");
  const [rootSizeGib, setRootSizeGib] = useState("");
  const [rootDisk, setRootDisk] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [startNow, setStartNow] = useState(false);
  const [restartPolicy, setRestartPolicy] = useState<"on_failure" | "never">("on_failure");
  const [onHostBoot, setOnHostBoot] = useState<"resume" | "stop">("resume");
  // In a cluster: automatic placement ("") or a node.
  const nodes = useNodes();
  const [node, setNode] = useState("");

  const firmware = bootMode === "firmware";
  const fromKernelImage = !firmware && kernelSource === "image";
  // A managed root disk, or a kernel image, gets the generated cloud-init
  // seed, and so a credential (spec/kernel-boot.md K4).
  const seeded = firmware || fromKernelImage || rootSource !== "path";

  // Kernels for the chosen image's architecture; the one extracted from
  // it first (spec/kernel-boot.md K3).
  const rootImage = rootSource === "image" ? images.find((i) => i.id === image) : undefined;
  const kernelChoices = kernelImages
    .filter((k) => !rootImage || k.arch === rootImage.arch)
    .sort((a, b) => Number(b.for_image === image) - Number(a.for_image === image));
  const chosenKernel = kernelImages.find((k) => k.id === kernelImage);
  useEffect(() => {
    if (!kernelChoices.some((k) => k.id === kernelImage)) setKernelImage(kernelChoices[0]?.id ?? "");
  }, [kernelChoices, kernelImage]);
  const extractKernel = async () => {
    if (!rootImage) return;
    setExtracting(rootImage.name);
    try {
      await pullImage({ kernel_from: rootImage.id });
      // Ready in seconds; poll until it is (or failed).
      for (let i = 0; i < 90; i++) {
        const all = await listImages();
        const k = all.find((x) => isKernel(x) && x.for_image === rootImage.id);
        if (k && imageState(k) !== "extracting") {
          setKernelImages(all.filter((x) => isKernel(x) && imageState(x) === "ready" && !x.deleting));
          if (imageState(k) === "ready") setKernelImage(k.id);
          else setSubmitError(`Extracting the kernel failed: ${k.status?.reason ?? "unknown"}`);
          return;
        }
        await new Promise((r) => setTimeout(r, 2000));
      }
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : "Extracting the kernel failed");
    } finally {
      setExtracting(null);
    }
  };

  // Firmware is built for one hypervisor: pick the newest for the new one.
  const firmwareChoices = firmwareFor(firmwareImages, hypervisor);
  const changeHypervisor = (next: HypervisorType) => {
    setFirmwareImage(firmwareFor(firmwareImages, next)[0]?.id ?? "");
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
        // A network named `default`, else the project's only one.
        // (By id: a project network and a host network may share a name.)
        const def = nets.find((n) => n.name === "default");
        if (def) setSelectedNetworks([def.id]);
        else if (nets.length === 1) setSelectedNetworks([nets[0].id]);
      })
      .catch(() => setNetworks([]));
    listImages()
      .then((imgs) => {
        const ready = imgs.filter((i) => imageState(i) === "ready" && !i.deleting && isDiskImage(i));
        setImages(ready);
        setFirmwareImages(imgs);
        setKernelImages(imgs.filter((i) => isKernel(i) && imageState(i) === "ready" && !i.deleting));
        setFirmwareImage((cur) => cur || (firmwareFor(imgs, "cloudhypervisor")[0]?.id ?? ""));
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
    // Only the pools this project may use; none: no section.
    if (project) {
      listProjectPciPools(project)
        .then(setPools)
        .catch(() => setPools([]));
    }
  }, [project, me.user?.display_name]);

  const poolRequests = Object.entries(poolAsks)
    .filter(([, a]) => a.count > 0)
    .map(([pool, a]) => ({ pool, count: a.count, same_switch: a.sameSwitch && a.count > 1 ? true : undefined }));
  const askPool = (pool: string, change: Partial<{ count: number; sameSwitch: boolean }>) =>
    setPoolAsks((cur) => ({ ...cur, [pool]: { ...(cur[pool] ?? { count: 0, sameSwitch: false }), ...change } }));

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setSubmitError(null);

    const devices = vfioDevices
      .split(",")
      .map((d) => d.trim())
      .filter((d) => d.length > 0);

    try {
      await onSubmit({
        name,
        vcpu_count: vcpuCount,
        mem_size_mib: memSizeMib,
        kernel_image_path: !firmware && kernelSource === "path" ? kernelPath : "",
        initrd_path: !firmware && kernelSource === "path" && initrdPath ? initrdPath : undefined,
        kernel: fromKernelImage ? kernelImage : undefined,
        kernel_from_disk: fromKernelImage && rootSource !== "path" && followGuest ? true : undefined,
        firmware: firmware ? firmwareImage : undefined,
        credential: seeded && credential ? credential : undefined,
        networks:
          selectedNetworks.length > 0 ? selectedNetworks.map((network) => ({ network })) : undefined,
        rootfs_path: rootSource === "path" ? rootfsPath : undefined,
        image: rootSource === "image" ? image : undefined,
        root_disk_size_gib: rootSource === "image" && rootSizeGib ? Number(rootSizeGib) : undefined,
        root_disk: rootSource === "disk" ? rootDisk : undefined,
        hypervisor: HYPERVISOR_TO_PROTO[hypervisor],
        kernel_args: !firmware && kernelArgs ? kernelArgs : undefined,
        vfio_devices: devices.length > 0 ? devices : undefined,
        devices: poolRequests.length > 0 ? poolRequests : undefined,
        power: startNow ? "POWER_STATE_RUNNING" : undefined,
        restart_policy: restartPolicy === "never" ? "RESTART_POLICY_NEVER" : "RESTART_POLICY_ON_FAILURE",
        on_host_boot: onHostBoot === "stop" ? "HOST_BOOT_POLICY_STOP" : "HOST_BOOT_POLICY_RESUME",
        node: nodes.clustered && node ? node : undefined,
      });
    } catch (err) {
      // Keep the form (and what was typed) so it can be corrected.
      setSubmitError(err instanceof Error ? err.message : "Failed to create VM");
    } finally {
      setSubmitting(false);
    }
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

      {nodes.clustered && (
        <PlacementPicker
          request={{
            project: project ?? undefined,
            vcpu_count: vcpuCount,
            mem_size_mib: memSizeMib,
            hypervisor: HYPERVISOR_TO_PROTO[hypervisor],
            networks: selectedNetworks.map((network) => ({ network })),
            vfio_devices: vfioDevices
              .split(",")
              .map((d) => d.trim())
              .filter((d) => d.length > 0),
            devices: poolRequests.length > 0 ? poolRequests : undefined,
            image: rootSource === "image" && image ? image : undefined,
          }}
          value={node}
          onChange={setNode}
        />
      )}

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
          <div>
            <label className="block text-sm font-medium text-gray-700">
              UEFI Firmware
            </label>
            <select
              className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
              required
              value={firmwareImage}
              onChange={(e) => setFirmwareImage(e.target.value)}
            >
              {firmwareChoices.length === 0 && <option value="">No firmware image for {HYPERVISOR_LABELS[hypervisor]}</option>}
              {firmwareChoices.map((i) => (
                <option key={i.id} value={i.id}>
                  {i.name}
                  {i.source?.firmware?.version ? ` (${i.source.firmware.version})` : ""}
                </option>
              ))}
            </select>
            <p className="mt-1 text-xs text-gray-500" data-testid="firmware-hint">
              {firmwareChoices.length === 0 ? (
                <>
                  Pull <span className="font-mono">{FIRMWARE_KEY[hypervisor]}</span> from the firmware catalog on the{" "}
                  <Link to="/images" className="text-sky-700 hover:underline">
                    Images page
                  </Link>{" "}
                  first.
                </>
              ) : (
                "A firmware image from the Images page, built for this hypervisor"
              )}
            </p>
          </div>
      ) : (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-700">
              Kernel
            </label>
            <select
              className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
              value={kernelSource}
              onChange={(e) => setKernelSource(e.target.value as KernelSource)}
            >
              <option value="image">A kernel image</option>
              <option value="path">Kernel files on the host (administrators)</option>
            </select>
          </div>

          {kernelSource === "image" ? (
            <div>
              <label className="block text-sm font-medium text-gray-700">
                Kernel Image
              </label>
              <select
                className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
                required
                value={kernelImage}
                onChange={(e) => setKernelImage(e.target.value)}
                data-testid="kernel-image"
              >
                {kernelChoices.length === 0 && <option value="">No kernel image{rootImage ? ` for ${archLabel(rootImage.arch)}` : ""}</option>}
                {kernelChoices.map((k) => (
                  <option key={k.id} value={k.id}>
                    {k.name}
                    {k.kernel?.release ? ` (${k.kernel.release})` : ""}
                    {k.for_image === image && rootImage ? " — from this image" : ""}
                  </option>
                ))}
              </select>
              <p className="mt-1 text-xs text-gray-500" data-testid="kernel-hint">
                {rootImage && !kernelChoices.some((k) => k.for_image === rootImage.id) ? (
                  <>
                    No kernel from {rootImage.name} yet.{" "}
                    <button
                      type="button"
                      className="text-sky-700 hover:underline disabled:opacity-50"
                      disabled={extracting !== null}
                      onClick={extractKernel}
                    >
                      {extracting ? `Extracting from ${extracting}…` : "Extract it from the image"}
                    </button>
                  </>
                ) : kernelChoices.length === 0 ? (
                  <>
                    A catalog image brings its kernel; pull one on the{" "}
                    <Link to="/images" className="text-sky-700 hover:underline">
                      Images page
                    </Link>
                    .
                  </>
                ) : (
                  "Use the kernel the image was built with: its modules are on the disk."
                )}
              </p>
            </div>
          ) : (
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
              <div>
                <label className="block text-sm font-medium text-gray-700">
                  Kernel Image Path
                </label>
                <input
                  type="text"
                  className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
                  placeholder="/path/to/vmlinuz"
                  required
                  value={kernelPath}
                  onChange={(e) => setKernelPath(e.target.value)}
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-700">
                  Initramfs Path (optional)
                </label>
                <input
                  type="text"
                  className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
                  placeholder="/path/to/initrd.img"
                  value={initrdPath}
                  onChange={(e) => setInitrdPath(e.target.value)}
                />
              </div>
            </div>
          )}

          <div>
            <label className="block text-sm font-medium text-gray-700">
              Kernel Arguments (optional)
            </label>
            <input
              type="text"
              className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
              placeholder={
                fromKernelImage && chosenKernel?.kernel?.cmdline
                  ? chosenKernel.kernel.cmdline
                  : "console=ttyS0 root=/dev/vda reboot=k panic=1"
              }
              value={kernelArgs}
              onChange={(e) => setKernelArgs(e.target.value)}
              data-testid="kernel-args"
            />
            <p className="mt-1 text-xs text-gray-500">
              {fromKernelImage && chosenKernel?.kernel?.cmdline
                ? "Empty: the kernel image's own command line (shown)."
                : "Empty: the default above. A cloud image needs its root, e.g. root=LABEL=cloudimg-rootfs ro."}
            </p>
          </div>
        </>
      )}

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
                      {i.name} ({archLabel(i.arch)}, {formatBytes(i.virtual_size_bytes)})
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
                {firmware
                  ? "UEFI-bootable image file (raw or qcow2), used as is"
                  : "A root filesystem or disk image file (raw or qcow2), used as is"}
              </p>
            </div>
          )}

          {fromKernelImage && rootSource !== "path" && (
            <label className="flex items-start gap-2 text-sm text-gray-700">
              <input type="checkbox" className="mt-1" checked={followGuest} onChange={(e) => setFollowGuest(e.target.checked)} />
              <span>
                Follow the guest&apos;s kernel
                <span className="block text-xs text-gray-500">
                  Before each start, use the kernel installed on the disk if it changed (e.g. after an upgrade in the guest).
                </span>
              </span>
            </label>
          )}

      {seeded && (
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
      )}

      {networks.length === 0 && (
        <p className="text-sm text-gray-500">
          No networks this project can use: the VM will have no NIC. Create one on the Networking page first.
        </p>
      )}
      {networks.length > 0 && (
        <div>
          <label className="block text-sm font-medium text-gray-700">
            Networks
          </label>
          <div className="mt-1 flex flex-wrap gap-3">
            {networks.map((n) => (
              <label key={n.id} className="flex items-center space-x-2 text-sm">
                <input
                  type="checkbox"
                  checked={selectedNetworks.includes(n.id)}
                  onChange={(e) =>
                    setSelectedNetworks((cur) =>
                      e.target.checked ? [...cur, n.id] : cur.filter((x) => x !== n.id),
                    )
                  }
                />
                <span className="font-mono">{n.name}</span>
                <span className="text-gray-400">({networkModeLabel(n.mode)})</span>
              </label>
            ))}
          </div>
          <p className="mt-1 text-xs text-gray-500">
            One NIC per selected network, in this order. Manage networks on the
            Networking page.
          </p>
        </div>
      )}

      {pools.length > 0 && (
        <div>
          <label className="block text-sm font-medium text-gray-700">
            Device Pools (optional)
          </label>
          <div className="mt-1 space-y-2">
            {pools.map((p) => {
              const name = p.name ?? "";
              const ask = poolAsks[name] ?? { count: 0, sameSwitch: false };
              return (
                <div key={name} className="flex flex-wrap items-center gap-3 text-sm">
                  <span className="font-mono w-32 truncate" title={name}>{name}</span>
                  {p.class && <span className="text-xs text-gray-500">{p.class}</span>}
                  {p.from && (
                    <span className="text-xs text-gray-500">
                      tenant pool of {p.from}, {p.used ?? 0}/{p.max ?? 0} held
                    </span>
                  )}
                  <span className="text-xs text-gray-500">{p.free ?? 0} free</span>
                  <input
                    type="number"
                    min="0"
                    aria-label={`Devices from ${name}`}
                    className="w-20 px-2 py-1 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
                    value={ask.count}
                    onChange={(e) => askPool(name, { count: Math.max(0, Number(e.target.value)) })}
                  />
                  {ask.count > 1 && (
                    <label className="flex items-center gap-1 text-xs text-gray-600">
                      <input
                        type="checkbox"
                        checked={ask.sameSwitch}
                        onChange={(e) => askPool(name, { sameSwitch: e.target.checked })}
                      />
                      Under one PCIe switch
                    </label>
                  )}
                </div>
              );
            })}
          </div>
          <p className="mt-1 text-xs text-gray-500">
            How many devices to take from each pool; the scheduler picks which,
            near each other and the VM&apos;s memory.
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

      <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
        <div>
          <label className="block text-sm font-medium text-gray-700">
            If It Crashes
          </label>
          <select
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg bg-white focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            value={restartPolicy}
            onChange={(e) => setRestartPolicy(e.target.value as "on_failure" | "never")}
          >
            <option value="on_failure">Restart it (with backoff)</option>
            <option value="never">Leave it stopped</option>
          </select>
        </div>
        <div>
          <label className="block text-sm font-medium text-gray-700">
            After a Host Reboot
          </label>
          <select
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg bg-white focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            value={onHostBoot}
            onChange={(e) => setOnHostBoot(e.target.value as "resume" | "stop")}
          >
            <option value="resume">Start it again if it was running</option>
            <option value="stop">Leave it stopped</option>
          </select>
        </div>
      </div>
      <p className="-mt-2 text-xs text-gray-500">
        A guest that shuts itself down stays stopped either way.
      </p>

      <label className="flex items-center gap-2 text-sm text-gray-700">
        <input type="checkbox" checked={startNow} onChange={(e) => setStartNow(e.target.checked)} />
        Start it now
      </label>

      {submitError && (
        <div className="p-3 bg-red-50 border border-red-200 rounded-lg text-sm text-red-700">{submitError}</div>
      )}

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

import { useRef, useEffect, useState, type FormEvent } from "react";
import { listCredentials, listDisks, listImages, listNetworks, listProjectPciPools, listServiceAccounts, pullImage } from "../api";
import type { PciPoolJson } from "../gen/glidex/cluster/v1/cluster_pb";
import { Link } from "react-router-dom";
import type { ApiToken, CreateVmRequest, CredentialInfo, DiskInfo, HypervisorType, ImageInfo, Network } from "../types";
import { HYPERVISOR_LABELS, HYPERVISOR_TO_PROTO, archLabel, archToProto, firmwareFor, formatBytes, imageState, isDiskImage, isKernel, networkModeLabel, networkUsableBy } from "../types";
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
  pylon: "cloudhv-edk2",
};
/** Where the last credential a user gave a VM in a project is kept (this browser). */
const lastCredentialKey = (user: string | undefined, project: string | null | undefined) =>
  `glidex.credential.${user ?? ""}.${project ?? ""}`;
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
  const [isElastic, setIsElastic] = useState(false);
  const [minVcpus, setMinVcpus] = useState<number | "">("");
  const [maxVcpus, setMaxVcpus] = useState<number | "">("");
  const [cpuWeight, setCpuWeight] = useState<number | "">("");
  const [memSizeMib, setMemSizeMib] = useState(1024);
  const [hypervisor, setHypervisor] = useState<HypervisorType>("cloudhypervisor");
  const [bootMode, setBootMode] = useState<BootMode>("firmware");
  /** Firmware image id; "" until the images load (or none exists). */
  const [firmwareImage, setFirmwareImage] = useState("");
  const [credential, setCredential] = useState("");
  // Set once the user picks a credential, so a late load never overrides it.
  const credentialChosen = useRef(false);
  const [credentials, setCredentials] = useState<CredentialInfo[]>([]);
  // Workload identity (spec/workload-identity.md W5).
  const [serviceAccount, setServiceAccount] = useState("");
  const [serviceAccounts, setServiceAccounts] = useState<ApiToken[]>([]);
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
  /** The vCPU or memory field was changed by hand: GPUs no longer size the VM. */
  const [sizedByHand, setSizedByHand] = useState(false);
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
  /** The advanced options are shown (they open by themselves when one needs a choice). */
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [imagesLoaded, setImagesLoaded] = useState(false);
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

  // The VM's architecture (spec/clustering.md §9.2.1): its image's, else
  // the chosen node's. The kernels and firmware offered are of it.
  const rootImage = rootSource === "image" ? images.find((i) => i.id === image) : undefined;
  const nodeArch = (nodes.clustered && node && nodes.nodes.get(node)?.status?.features?.arch) || undefined;
  const vmArch = rootImage?.arch ?? (nodeArch ? archToProto(nodeArch) : undefined);
  const ofVmArch = (a?: string) => !vmArch || !a || a === "ARCH_UNSPECIFIED" || a === vmArch;
  // A node chosen of another architecture than the image: an image of
  // the node's instead, when there is one.
  useEffect(() => {
    if (!nodeArch || rootSource !== "image" || !rootImage || rootImage.arch === archToProto(nodeArch)) return;
    const same = images.find((i) => i.arch === archToProto(nodeArch));
    if (same) setImage(same.id);
    // Only when the node changes: an image picked afterwards stays.
  }, [nodeArch]);
  const archMismatch =
    nodeArch && rootImage?.arch && rootImage.arch !== archToProto(nodeArch) ? archLabel(rootImage.arch) : undefined;
  // Kernels for that architecture; the one extracted from the image first
  // (spec/kernel-boot.md K3).
  const kernelChoices = kernelImages
    .filter((k) => ofVmArch(k.arch))
    .sort((a, b) => Number(b.for_image === image) - Number(a.for_image === image));
  const chosenKernel = kernelImages.find((k) => k.id === kernelImage);
  // Preselect the image's own kernel only: another image's initramfs looks
  // for that image's root, and its modules aren't on this disk.
  const ownKernel = rootImage ? kernelChoices.find((k) => k.for_image === rootImage.id) : undefined;
  const defaultKernel = rootImage ? (ownKernel?.id ?? "") : (kernelChoices[0]?.id ?? "");
  useEffect(() => {
    setKernelImage(defaultKernel);
  }, [image, rootSource, defaultKernel]);
  // Taken from a different image than the one this VM boots.
  const mismatched =
    rootImage && chosenKernel?.for_image && chosenKernel.for_image !== rootImage.id ? chosenKernel : undefined;
  const imageName = (id?: string) => images.find((i) => i.id === id)?.name ?? "another image";
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

  // Firmware is built for one hypervisor and architecture: the newest
  // for the VM's, unless the one chosen is.
  const firmwareChoices = firmwareFor(firmwareImages, hypervisor).filter((i) => ofVmArch(i.arch));
  const firmwareKey = firmwareChoices.map((i) => i.id).join(",");
  useEffect(() => {
    setFirmwareImage((cur) => (firmwareChoices.some((i) => i.id === cur) ? cur : (firmwareChoices[0]?.id ?? "")));
  }, [firmwareKey]);
  const changeHypervisor = (next: HypervisorType) => {
    setFirmwareImage(firmwareFor(firmwareImages, next).filter((i) => ofVmArch(i.arch))[0]?.id ?? "");
    setHypervisor(next);
  };

  useEffect(() => {
    if (project) {
      listServiceAccounts(project)
        .then(setServiceAccounts)
        .catch(() => setServiceAccounts([]));
    }
    listCredentials(project)
      .then((creds) => {
        setCredentials(creds);
        // Default to the credential this user last gave a VM here, else
        // their own, if the project has one by that name.
        let last: string | null = null;
        try {
          last = localStorage.getItem(lastCredentialKey(me.user_id, project));
        } catch {
          // Storage off: no memory.
        }
        const mine = me.user?.display_name;
        const pick = [last, mine].find((u) => u && creds.some((c) => c.username === u));
        if (!credentialChosen.current && pick) {
          setCredential(pick);
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
        setImagesLoaded(true);
        setFirmwareImage((cur) => cur || (firmwareFor(imgs, "cloudhypervisor")[0]?.id ?? ""));
        if (ready.length > 0) {
          setImage(ready[0].id);
          setRootSource("image");
        }
      })
      .catch(() => {
        setImages([]);
        setImagesLoaded(true);
      });
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
  }, [project, me.user_id, me.user?.display_name]);

  // The first GPU pool asked for (not under one switch) goes as `gpus`:
  // aligned NUMA placement, and the pool's per-device sizing for the vCPU
  // and memory fields until they're set by hand (spec/gpu-vms.md §3).
  const gpuPool = pools.find(
    (p) => p.class === "gpu" && (poolAsks[p.name ?? ""]?.count ?? 0) > 0 && !poolAsks[p.name ?? ""]?.sameSwitch,
  );
  const gpuCount = gpuPool ? poolAsks[gpuPool.name ?? ""].count : 0;
  const gpus = gpuPool ? { count: gpuCount, pool: gpuPool.name } : undefined;
  useEffect(() => {
    if (!gpuPool || sizedByHand) return;
    if (gpuPool.per_device_vcpus) setVcpuCount(gpuCount * gpuPool.per_device_vcpus);
    if (gpuPool.per_device_memory_mib) setMemSizeMib(gpuCount * gpuPool.per_device_memory_mib);
  }, [gpuPool, gpuCount, sizedByHand]);
  const poolRequests = Object.entries(poolAsks)
    .filter(([pool, a]) => a.count > 0 && pool !== gpuPool?.name)
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
        vcpu_count: hypervisor === "pylon" && isElastic && maxVcpus !== "" ? Number(maxVcpus) : vcpuCount,
        min_vcpus: isElastic && minVcpus !== "" ? Number(minVcpus) : undefined,
        max_vcpus: isElastic && maxVcpus !== "" ? Number(maxVcpus) : undefined,
        cpu_weight: isElastic && cpuWeight !== "" ? Number(cpuWeight) : undefined,
        mem_size_mib: memSizeMib,
        kernel_image_path: !firmware && kernelSource === "path" ? kernelPath : "",
        initrd_path: !firmware && kernelSource === "path" && initrdPath ? initrdPath : undefined,
        kernel: fromKernelImage ? kernelImage : undefined,
        kernel_from_disk: fromKernelImage && rootSource !== "path" && followGuest ? true : undefined,
        firmware: firmware ? firmwareImage : undefined,
        credential: seeded && credential ? credential : undefined,
        service_account: serviceAccount || undefined,
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
        gpus,
        power: startNow ? "POWER_STATE_RUNNING" : undefined,
        restart_policy: restartPolicy === "never" ? "RESTART_POLICY_NEVER" : "RESTART_POLICY_ON_FAILURE",
        on_host_boot: onHostBoot === "stop" ? "HOST_BOOT_POLICY_STOP" : "HOST_BOOT_POLICY_RESUME",
        node: nodes.clustered && node ? node : undefined,
      });
      if (seeded && credential) {
        try {
          localStorage.setItem(lastCredentialKey(me.user_id, project), credential);
        } catch {
          // Storage off: nothing remembered.
        }
      }
    } catch (err) {
      // Keep the form (and what was typed) so it can be corrected.
      setSubmitError(err instanceof Error ? err.message : "Failed to create VM");
    } finally {
      setSubmitting(false);
    }
  };

  // An advanced option the form can't send without (a required field):
  // shown, so the browser can point at it.
  const advancedNeeded =
    imagesLoaded && (firmware ? !firmwareImage : kernelSource === "image" ? !kernelImage : !kernelPath);
  // Once the form opened Advanced options for a missing choice, they stay
  // open: making that choice must not fold them away under the user.
  useEffect(() => {
    if (advancedNeeded) setShowAdvanced(true);
  }, [advancedNeeded]);
  const firmwareName = firmwareChoices.find((i) => i.id === firmwareImage)?.name;
  const advancedSummary = firmware
    ? `UEFI firmware${firmwareName ? ` (${firmwareName})` : ""}`
    : `Direct kernel boot${fromKernelImage && chosenKernel ? ` (${chosenKernel.name})` : ""}`;

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
            max="255"
            value={vcpuCount}
            onChange={(e) => {
              setSizedByHand(true);
              setVcpuCount(Number(e.target.value));
            }}
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
            max="4194304"
            value={memSizeMib}
            onChange={(e) => {
              setSizedByHand(true);
              setMemSizeMib(Number(e.target.value));
            }}
          />
        </div>
      </div>

      <div className="border border-gray-200 rounded-lg p-3 bg-gray-50/50">
        <label className="flex items-center gap-2 cursor-pointer text-sm font-medium text-gray-700">
          <input
            type="checkbox"
            className="rounded border-gray-300 text-sky-600 focus:ring-sky-500"
            checked={isElastic}
            onChange={(e) => {
              setIsElastic(e.target.checked);
              if (e.target.checked) {
                if (minVcpus === "") setMinVcpus(vcpuCount);
                if (maxVcpus === "") setMaxVcpus(Math.max(vcpuCount, 4));
              }
            }}
          />
          Elastic CPU scaling (min/max vCPUs, weight)
        </label>
        {isElastic && (
          <div className="grid grid-cols-3 gap-3 mt-2">
            <div>
              <label className="block text-xs font-medium text-gray-600">Min vCPUs</label>
              <input
                type="number"
                className="mt-1 w-full px-2 py-1 text-sm border border-gray-300 rounded focus:ring-2 focus:ring-sky-500 bg-white"
                min={hypervisor === "pylon" ? 0 : 1}
                max={hypervisor === "pylon" ? (typeof maxVcpus === "number" ? maxVcpus : 64) : vcpuCount}
                value={minVcpus}
                onChange={(e) => setMinVcpus(e.target.value === "" ? "" : Number(e.target.value))}
              />
            </div>
            <div>
              <label className="block text-xs font-medium text-gray-600">Max vCPUs</label>
              <input
                type="number"
                className="mt-1 w-full px-2 py-1 text-sm border border-gray-300 rounded focus:ring-2 focus:ring-sky-500 bg-white"
                min={hypervisor === "pylon" ? (typeof minVcpus === "number" ? minVcpus : 1) : vcpuCount}
                max={hypervisor === "pylon" ? 64 : 255}
                value={maxVcpus}
                onChange={(e) => {
                  const val = e.target.value === "" ? "" : Number(e.target.value);
                  setMaxVcpus(val);
                  if (hypervisor === "pylon" && typeof val === "number") {
                    setVcpuCount(val);
                  }
                }}
              />
            </div>
            <div>
              <label className="block text-xs font-medium text-gray-600">Weight (1–10000)</label>
              <input
                type="number"
                className="mt-1 w-full px-2 py-1 text-sm border border-gray-300 rounded focus:ring-2 focus:ring-sky-500 bg-white"
                min="1"
                max="10000"
                placeholder="100"
                value={cpuWeight}
                onChange={(e) => setCpuWeight(e.target.value === "" ? "" : Number(e.target.value))}
              />
            </div>
          </div>
        )}
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
                  {i.name} ({archLabel(i.arch)}, {formatBytes(i.virtual_size_bytes)})
                </option>
              ))}
            </select>
            {archMismatch && (
              <p className="mt-1 text-xs text-amber-700" data-testid="arch-mismatch">
                {nodes.name(node)} is {nodeArch}; this image is {archMismatch}. Choose an {nodeArch} image or another
                node.
              </p>
            )}
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

      {serviceAccounts.length > 0 && (
        <div>
          <label className="block text-sm font-medium text-gray-700">
            Service Account (optional)
          </label>
          <select
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
            value={serviceAccount}
            onChange={(e) => setServiceAccount(e.target.value)}
            data-testid="service-account"
          >
            <option value="">None</option>
            {serviceAccounts.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
          </select>
          <p className="mt-1 text-xs text-gray-500">
            Software in the VM gets short-lived glidex tokens acting as it, from its metadata endpoint, with no secret stored in the guest.
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
                  {p.per_device_vcpus && p.per_device_memory_mib && (
                    <span className="text-xs text-gray-500">
                      {p.per_device_vcpus} vCPUs and {Math.round(p.per_device_memory_mib / 1024)} GiB each
                    </span>
                  )}
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
            near each other and the VM&apos;s memory. GPUs place the VM on their NUMA
            nodes, with its vCPUs and memory split between them
            {gpuPool?.per_device_vcpus && !sizedByHand ? ", and size it from the pool" : ""}.
          </p>
        </div>
      )}

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
            gpus,
            image: rootSource === "image" && image ? image : undefined,
          }}
          value={node}
          onChange={setNode}
        />
      )}

      <details
        className="rounded-lg border border-gray-200 px-4 py-3"
        open={showAdvanced || advancedNeeded}
        onToggle={(e) => !advancedNeeded && setShowAdvanced(e.currentTarget.open)}
        data-testid="advanced-options"
      >
        <summary className="cursor-pointer select-none text-sm font-medium text-gray-700">
          Advanced options
          <span className="ml-2 text-xs font-normal text-gray-500">{advancedSummary}</span>
        </summary>
        <div className="mt-4 space-y-4">
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
                  {firmwareChoices.length === 0 && (
                    <option value="">
                      No firmware image for {HYPERVISOR_LABELS[hypervisor]}
                      {vmArch ? ` on ${archLabel(vmArch)}` : ""}
                    </option>
                  )}
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
                      Pull <span className="font-mono">{FIRMWARE_KEY[hypervisor]}</span>
                      {vmArch ? ` for ${archLabel(vmArch)}` : ""} from the firmware catalog on the{" "}
                      <Link to="/images" className="text-sky-700 hover:underline">
                        Images page
                      </Link>{" "}
                      first.
                    </>
                  ) : (
                    `A firmware image from the Images page, built for this hypervisor${vmArch ? ` and ${archLabel(vmArch)}` : ""}`
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
                    {kernelChoices.length === 0 ? (
                      <option value="">No kernel image{vmArch ? ` for ${archLabel(vmArch)}` : ""}</option>
                    ) : (
                      !kernelImage && <option value="">Choose a kernel</option>
                    )}
                    {kernelChoices.map((k) => (
                      <option key={k.id} value={k.id}>
                        {k.name}
                        {k.kernel?.release ? ` (${k.kernel.release})` : ""}
                        {rootImage && k.for_image === rootImage.id
                          ? " — from this image"
                          : rootImage && k.for_image
                            ? ` — from ${imageName(k.for_image)}`
                            : ""}
                      </option>
                    ))}
                  </select>
                  {mismatched && (
                    <p className="mt-1 text-xs text-amber-700" data-testid="kernel-mismatch">
                      This kernel was taken from {imageName(mismatched.for_image)}; {rootImage?.name} may not boot with
                      it (its initramfs looks for that image&apos;s root disk).
                    </p>
                  )}
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
                      : // The server's default: the serial console differs on aarch64 (PL011).
                        `console=${(fromKernelImage ? chosenKernel?.arch : rootImage?.arch) === "ARCH_AARCH64" ? "ttyAMA0" : "ttyS0"} root=/dev/vda reboot=k panic=1`
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
        </div>
      </details>

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

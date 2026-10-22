# Glidex compared

This page places Glidex next to other platforms that run virtual machines on
your own hardware. It is for contributors: what Glidex does differently on
purpose, and what it does not try to do.

Facts about other products are as of **October 2026**, checked against
their official documentation and project pages (see
[References](#references)). Products change quickly; check the linked pages
before you rely on a detail. Facts about Glidex are from this repository at
the same date. A dash (—) marks a cell this page does not claim.

## Positioning

Glidex is a KVM control plane written in Rust. It runs VMs on
Cloud Hypervisor (the default) or QEMU, on one host or on a cluster of
hosts that replicate their state with Raft embedded in the control-plane
binary. One proto-defined service layer serves both a REST API and
gRPC/gRPC-Web/Connect; a React web UI and the `gxctl` CLI use it.
Networking is Open vSwitch on a host and OVN across a cluster.
Authorization is Cedar policy over projects and tenants, with identities
from local accounts, passkeys, OIDC providers, SCIM and directory sync.

It sits between a single-host tool and a full private cloud (OpenStack,
CloudStack). It aims at labs, teams and small clouds of up to about 50
hosts, especially ones that hand out GPUs and other PCI devices. It is
young (v0.0.x tags, first commit December 2025) and has fewer features
than any mature product below.

## Comparison matrix

The Glidex column describes what is built today; planned work is in
[Non-goals and current gaps](#non-goals-and-current-gaps).

| | Glidex | Proxmox VE | OpenStack | vSphere | Harvester / KubeVirt | Incus / LXD | OpenNebula | CloudStack |
|---|---|---|---|---|---|---|---|---|
| Control-plane model | One binary per host; 1, 3 or 5 *server* hosts run the API and Raft, *agent* hosts run VMs only | No central controller; every node runs the API and web UI | Many services (Nova, Neutron, Cinder, Keystone…) talking over a message queue, with a shared SQL database | vCenter Server appliance manages ESX hosts | Kubernetes controllers; VMs are custom resources run in `virt-launcher` pods | One daemon per host; clustered daemons share a replicated database | Front-end (`oned`) plus hypervisor hosts | Management server (Tomcat) plus hypervisor hosts |
| Hypervisor | KVM via Cloud Hypervisor or QEMU | QEMU/KVM; LXC containers | KVM/QEMU mainly (libvirt); also LXC, vSphere, Ironic bare metal and others | ESX | KVM via libvirt/QEMU in pods | QEMU/KVM VMs; system and application containers | KVM, LXC | KVM, vSphere, XenServer/Xen, Hyper-V, LXC, bare metal |
| State store and HA | Embedded Raft (openraft) over ReDB; no automatic VM failover | pmxcfs (database-driven file system) replicated by Corosync; HA manager with watchdog fencing restarts guests | SQL database (MySQL/MariaDB) and a message queue | vCenter with embedded PostgreSQL; vSphere HA and Fault Tolerance | Kubernetes API (etcd) | Incus: Cowsql replicated with Raft; LXD: Dqlite | Raft between an odd number of front-ends; one leader takes writes | MySQL; HA-enabled VMs restarted, possibly on another host |
| Live migration | No (planned on shared datastores) | Yes (shared storage) | Yes | Yes (vMotion) | Yes | Yes (Incus, `migration.stateful`) | Yes (`onevm migrate --live`) | Yes |
| Networking | OVS per host (NAT, isolated, bridged, VLAN); OVN across a cluster with an HA NAT edge and per-project routers | Linux/OVS bridges; SDN zones incl. VLAN, VXLAN, EVPN | Neutron ML2 with OVS or OVN drivers, among others | Standard and distributed switches | Kubernetes networking; VLAN networks in Harvester | Bridges; OVN networks across a cluster (Incus) | — | Basic (flat L2, L3 isolation) and Advanced (VLAN, SDN) |
| Storage | `dir` (node-local), `shared-dir` (NFS-like mount), node-local LVM-thin; offline moves between them | Ceph RBD, ZFS, iSCSI, LVM, LVM-thin, NFS and more | Cinder with many back-end drivers | VMFS, NFS, vSAN, vVols | Longhorn distributed block storage (Harvester) | Incus: dir, Btrfs, LVM, ZFS, Ceph RBD/CephFS/Object, LINSTOR, TrueNAS | — | iSCSI, NFS, Ceph RBD, PowerFlex, ONTAP, local disk |
| API style | REST + gRPC/Connect from one proto service layer | REST (JSON, JSON Schema) | REST per service | REST (vSphere Automation API) | Kubernetes API | REST | XML-RPC core, with bindings and REST APIs for some services | REST-like query API, EC2 compatibility layer |
| Authorization | Cedar policies; project and host roles; audit log | Roles on paths (ACL model); pools | Keystone roles and policy | Global and inventory permissions | Rancher cluster and project roles (Harvester) | Incus: OpenFGA or restricted TLS certificates; projects | — | Roles, including dynamic custom roles |
| Multi-tenancy | Tenants, projects, teams, quotas; per-project routers | Pools and permissions | Projects and domains | — | Rancher projects and namespaces (Harvester) | Projects | VDCs, groups | Domains, accounts, projects |
| Identity integration | PAM, passkeys, OIDC presets (Google, JumpCloud), SCIM, Google Workspace pull sync, workload federation | PAM, LDAP, Active Directory, OpenID Connect | LDAP backend; SAML2 and OpenID Connect federation | AD FS, Okta, Entra ID, PingFederate federation | — | Incus: TLS client certificates, OIDC | LDAP, SAML | LDAP/AD, SAML 2.0, OAuth2 (Google, GitHub) |
| GPU / devices | VFIO passthrough with NUMA topology, device pools, GPU requests by count, hugepages, dedicated CPUs; no vGPU | PCIe passthrough, mediated devices (vGPU), SR-IOV | PCI passthrough, SR-IOV, vGPU | — | PCI passthrough; NVIDIA SR-IOV vGPU (Harvester); host devices and mediated devices (KubeVirt) | Incus VMs: physical, mdev, SR-IOV, native-context GPUs | PCI passthrough, SR-IOV, NVIDIA vGPU and MIG | GPU passthrough and vGPU on KVM |
| Install / ops footprint | `cargo run -p glidex-install` from source on an existing distro; systemd units; no external database | Debian-based distribution | Several services plus SQL and message queue | vCenter appliance and ESX hosts | Bare-metal HCI distribution on SUSE Linux Micro (Harvester) | Daemon on an existing distro | Front-end plus hosts | Management server, MySQL, hypervisor hosts |
| License | MIT OR Apache-2.0 | AGPL-3.0 | Apache-2.0 | Proprietary; subscription only (VCF, VVF) | Apache-2.0 | Incus: Apache-2.0; LXD: AGPL-3.0 | Apache-2.0 | Apache-2.0 |
| Maturity and status | Early (v0.0.x) | Mature; major version 9 | Mature; 2026.2 "Hibiscus" | Mature; Broadcom | Production releases | Mature; Incus is a community fork of LXD | Mature; 7.4 | Mature; 4.23.0.0 regular release |

## Products

### Proxmox VE

A Debian-based server virtualization platform for QEMU/KVM and LXC, with a
web UI, a REST API, HA, SDN and broad storage support.

- **Differs:** every Proxmox node is equal and ships as a distribution;
  Glidex installs onto an existing distro and separates server and agent
  roles. Proxmox has live migration, HA restart with fencing, mediated
  devices and many storage types; Glidex has none of these today. Glidex
  has tenants, Cedar policy and SCIM and directory sync built in.
- **Pick Proxmox when** you want a mature, all-in-one virtualization host
  for a small or medium site, with VMs and containers.

### OpenStack

A family of services (Nova compute, Neutron networking, Cinder block
storage, Keystone identity…) that make a full IaaS cloud, run by the
OpenInfra Foundation.

- **Differs:** Glidex is one binary with an embedded store, not a set of
  services over a message queue and SQL database. It uses similar ideas at
  a smaller scale: OVN networks with routers, projects and quotas,
  federated identity. OpenStack covers far more: many hypervisor and
  storage drivers, vGPU, bare metal, very large deployments.
- **Pick OpenStack when** you need a full cloud API, many back ends, or a
  large deployment with a team to run it.

### oVirt and Red Hat Virtualization

oVirt manages KVM hosts from a central Engine (web UI and REST API) on
PostgreSQL, with the VDSM agent on each host. Its current release is
4.5.7, and it describes itself as community-driven. Red Hat Virtualization
4.4, the product built on it, left its Maintenance Phase on 31 August 2024
and its Extended Life Phase on 31 August 2026. Red Hat points RHV users
to OpenShift Virtualization.

- **Differs:** oVirt keeps state in a central PostgreSQL database behind
  the Engine; Glidex replicates its own store with Raft and needs no
  external database.
- **Pick oVirt when** you already run it and the community release meets
  your needs; former RHV customers are directed to OpenShift
  Virtualization.

### VMware vSphere and vCenter

ESX hosts managed by the vCenter appliance, with vMotion, vSphere HA,
Fault Tolerance and vSAN. Broadcom ended perpetual licensing; vSphere is
sold as subscriptions, chiefly in VMware Cloud Foundation (VCF) and
VMware vSphere Foundation (VVF), licensed per core.

- **Differs:** Glidex runs on stock Linux and KVM, is open source and has
  no license cost. vSphere is far ahead on live migration, HA, storage,
  ecosystem and vendor support.
- **Pick vSphere when** you need those operational features and vendor
  support, and the subscription fits.

### Harvester and KubeVirt

KubeVirt adds VMs to Kubernetes as custom resources, run in pods with
libvirt and QEMU. Harvester (SUSE) is a bare-metal HCI distribution built
on Kubernetes, KubeVirt and Longhorn, managed alongside Rancher.

- **Differs:** Glidex does not need Kubernetes; its state is in its own
  Raft store, and VMs are not pods. Glidex's draft app layer
  ([apps.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/apps.md))
  would run Kubernetes *on* Glidex VMs, not Glidex on Kubernetes.
- **Pick Harvester or KubeVirt when** your team already runs Kubernetes and
  wants VMs and containers under one API and toolchain.

### Incus and LXD

Incus is a community-driven alternative to Canonical's LXD, maintained by
many of LXD's original developers. Both manage system containers and VMs
through a REST API, on one host or a cluster with a replicated database
(Cowsql for Incus, Dqlite for LXD). Incus is Apache-2.0; LXD is AGPL-3.0
and maintained by Canonical.

- **Differs:** the closest in shape: one daemon, a REST API, embedded
  replicated state, OVN networks. Glidex chose Raft over ReDB instead of a
  replicated SQL database
  ([clustering.md D1](https://github.com/CMKL-APEX/glidex/blob/main/spec/clustering.md)),
  manages VMs only, offers Cloud Hypervisor as well as QEMU, and puts more
  into the control plane for tenants, identity and NUMA-aware device
  placement. Incus has containers, live migration and many storage
  drivers, and years of use.
- **Pick Incus or LXD when** you want containers and VMs together, or a
  mature, packaged tool.

### OpenNebula

A cloud manager with a front-end (`oned`, HA through Raft across front-end
servers) and KVM or LXC hosts, with VDCs for multi-tenancy, LDAP and SAML
sign-in, GPU passthrough and vGPU.

- **Differs:** both target private clouds. Both replicate front-end state
  with Raft; OpenNebula replicates SQL statements to its database, Glidex
  replicates write sets to ReDB. OpenNebula has live migration, broad
  GPU support and many integrations; Glidex has a smaller feature set,
  REST and gRPC from one schema, and Cedar authorization.
- **Pick OpenNebula when** you need a proven private cloud today.

### Apache CloudStack

An IaaS platform: Java management servers on MySQL drive KVM, vSphere,
XenServer/Xen, Hyper-V and other hosts, organized as regions, zones, pods
and clusters, with domains, accounts and projects.

- **Differs:** CloudStack supports many hypervisors and is built for
  multi-zone clouds; Glidex is KVM-only and much smaller.
- **Pick CloudStack when** you run a multi-zone cloud for many tenants
  and need broad hypervisor and storage support.

### MicroVM stacks (Firecracker, Kata Containers)

Firecracker is a minimal KVM-based VMM built at AWS for serverless and
container services (it runs AWS Lambda and Fargate). Kata Containers is a
container runtime that runs workloads in lightweight VMs, through OCI and
the Kubernetes CRI, with QEMU, Cloud Hypervisor or Firecracker. Cloud
Hypervisor, Glidex's default VMM, is built on the same rust-vmm crates.

- **Differs:** these are VMMs or runtimes, not control planes. Glidex
  manages long-lived VMs with disks, networks and devices; it does not aim
  at per-request VM creation or container semantics.
- **Pick them when** you need sandboxed, short-lived workloads in large
  numbers.

## Key differentiators

Each follows from a design decision; see
[Design decisions and trade-offs](design-decisions.md) for the reasoning.

| Differentiator | Design decision | Evidence |
|---|---|---|
| No external database or message bus, even in a cluster | Raft embedded in the control plane over ReDB; the leader computes write sets, others apply bytes | [clustering.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/clustering.md) D1–D5, `crates/glidex-raftstore` |
| A single host stays a single host | Standalone uses the local store; Raft only after `gxctl cluster init` | clustering.md D5 |
| Guests survive control-plane restarts and upgrades | Each VM runs under its own `glidex-vm-shim` in its own systemd unit; the control plane adopts them on start | [reconciliation.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/reconciliation.md) |
| Two hypervisors behind one API | One driver contract for Cloud Hypervisor and QEMU | [hypervisors.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/hypervisors.md) |
| One API, two transports | Protos are the source of truth; one service layer under REST and gRPC/Connect | [grpc.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/grpc.md) |
| Policy as code | Cedar policies with shipped roles, decisions recorded in the audit log | [security.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/security.md) §7, §10 |
| Identity from the organization | Connectors (OIDC presets), SCIM, Google Workspace pull sync, invitations, passkeys, workload federation | [tenancy.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/tenancy.md) T1–T5, [passkeys.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/passkeys.md) |
| Topology-aware device placement | Each node reports NUMA, CPUs, hugepages and PCIe tree; scheduler aligns devices, memory and dedicated CPUs; pools and GPU requests | [host-topology.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/host-topology.md), [gpu-vms.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/gpu-vms.md) |
| Unprivileged control plane | Root work goes through small helpers (`glidex-netd`, `glidex-authd`, `glidex-stord`) | [architecture.md](architecture.md), storage.md S12, S17 |
| Storage by mounts, not plugins first | `shared-dir` uses mounts the operator makes; driver trait shaped like CSI for later | [storage.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/storage.md) S1 |
| Verified images and kernel boot | Checksum-verified catalog, linked qcow2 disks, kernels extracted from cloud images | [images.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/images.md), [kernel-boot.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/kernel-boot.md) |
| Metering built in | Hourly exactly-once usage ledger, per-VM and per-network traffic | [metering.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/metering.md) |

## Non-goals and current gaps

Non-goals (by design):

- **Automatic VM failover.** A node that stops heartbeating leaves its VMs
  unknown; nothing restarts them elsewhere, because a partitioned node may
  still run them (clustering.md D8). Moving a stopped or lost VM is an
  explicit operator action.
- **Containers.** Glidex runs VMs only.
- **Running on Kubernetes**, or an etcd/SQL dependency.
- **An eBPF datapath** (clustering.md D15).
- **vGPU and mediated devices** (host-topology.md §1).

Gaps (not built yet, some planned):

- Live migration (planned after shared datastores; clustering.md §18).
- Disk snapshots, backups, encryption at rest (storage.md §1).
- Shared block storage: `lvm` is node-local LVM-thin; shared LVM, Ceph RBD
  and a CSI adapter are later milestones. Ceph is usable only as a mounted
  CephFS through `shared-dir`.
- Serial console only: no VNC/SPICE or graphical display. Cloud Hypervisor
  itself runs Windows guests, but Glidex's catalog holds Linux cloud images
  provisioned by cloud-init, and Windows guests are not a target today.
  <!-- TODO(verify): whether Windows guest support is planned. -->
- Security groups, floating IPs, BGP and load balancers (clustering.md §2).
- Scale: designed for about 50 hosts and 2,000 VMs (clustering.md §4.1).
- LDAP, Entra ID and GitHub connectors (tenancy.md T2b, T4b); directories
  apart from clusters and fleets (T6, T7).
- Distribution packages: install is from source with Rust.

## References

Glidex:

- https://github.com/CMKL-APEX/glidex/blob/main/README.md
- https://github.com/CMKL-APEX/glidex/blob/main/spec/README.md
- https://github.com/CMKL-APEX/glidex/blob/main/spec/clustering.md
- https://github.com/CMKL-APEX/glidex/blob/main/spec/storage.md
- https://github.com/CMKL-APEX/glidex/blob/main/spec/host-topology.md
- https://github.com/CMKL-APEX/glidex/blob/main/spec/tenancy.md

Other products (official pages fetched for this comparison, October 2026):

- Proxmox VE: https://pve.proxmox.com/wiki/Main_Page,
  https://pve.proxmox.com/pve-docs/pve-admin-guide.html,
  https://pve.proxmox.com/pve-docs/pveum.1.html,
  https://pve.proxmox.com/wiki/PCI(e)_Passthrough
- OpenStack: https://www.openstack.org/,
  https://docs.openstack.org/nova/latest/admin/architecture.html,
  https://docs.openstack.org/nova/latest/admin/live-migration-usage.html,
  https://docs.openstack.org/nova/latest/admin/virtual-gpu.html,
  https://docs.openstack.org/neutron/latest/install/ovn/manual_install.html,
  https://docs.openstack.org/cinder/latest/drivers.html,
  https://docs.openstack.org/keystone/latest/admin/federation/introduction.html
- oVirt and RHV: https://www.ovirt.org/,
  https://www.ovirt.org/documentation/installing_ovirt_as_a_standalone_manager_with_local_databases/,
  https://docs.redhat.com/en/documentation/red_hat_virtualization/4.4,
  https://access.redhat.com/support/policy/updates/rhev
- VMware vSphere: https://techdocs.broadcom.com/us/en/vmware-cis/vsphere/vsphere/9-0/vcenter-installation-and-setup/introduction-to-vsphere-installation-and-setup/overview-of-the-vcsa.html,
  https://techdocs.broadcom.com/us/en/vmware-cis/vsphere/vsphere/9-0/vsphere-availability.html,
  https://techdocs.broadcom.com/us/en/vmware-cis/vsphere/vsphere/9-0/vsphere-storage.html,
  https://techdocs.broadcom.com/us/en/vmware-cis/vsphere/vsphere/9-0/vsphere-networking.html,
  https://techdocs.broadcom.com/us/en/vmware-cis/vsphere/vsphere/9-0/vsphere-authentication/vsphere-authentication-with-vcenter-single-sign-on/understanding-vcenter-server-identity-provider-federation.html,
  https://developer.broadcom.com/xapis/vsphere-automation-api/latest/,
  https://techdocs.broadcom.com/us/en/vmware-cis/vcf/vcf-9-0-and-later/9-1/licensing/licensing-overview.html,
  https://blogs.vmware.com/cloud-foundation/2024/01/22/vmware-end-of-availability-of-perpetual-licensing-and-saas-services/
- Harvester: https://docs.harvesterhci.io/v1.9/,
  https://docs.harvesterhci.io/v1.6/advanced/addons/pcidevices,
  https://docs.harvesterhci.io/v1.8/advanced/vgpusupport/,
  https://docs.harvesterhci.io/v1.8/rancher/rancher-rbac/,
  https://github.com/harvester/harvester
- KubeVirt: https://kubevirt.io/user-guide/,
  https://kubevirt.io/user-guide/architecture/architecture/,
  https://github.com/kubevirt/kubevirt
- Incus: https://linuxcontainers.org/incus/introduction/,
  https://linuxcontainers.org/incus/docs/main/explanation/clustering/,
  https://linuxcontainers.org/incus/docs/main/authentication/,
  https://linuxcontainers.org/incus/docs/main/reference/storage_drivers/,
  https://linuxcontainers.org/incus/docs/main/howto/move_instances/,
  https://linuxcontainers.org/incus/docs/main/reference/devices_gpu/
- LXD: https://canonical.com/lxd/docs/latest/,
  https://canonical.com/lxd/docs/latest/database/,
  https://github.com/canonical/lxd
- OpenNebula: https://docs.opennebula.io/7.4/,
  https://docs.opennebula.io/7.4/product/control_plane_configuration/high_availability/frontend_ha/,
  https://docs.opennebula.io/7.4/product/integration_references/system_interfaces/api/,
  https://docs.opennebula.io/7.4/product/virtual_machines_operation/virtual_machines/vm_instances/,
  https://github.com/OpenNebula/one
- Apache CloudStack: https://cloudstack.apache.org/,
  https://docs.cloudstack.apache.org/en/latest/conceptsandterminology/concepts.html,
  https://docs.cloudstack.apache.org/en/latest/adminguide/accounts.html,
  https://docs.cloudstack.apache.org/en/latest/adminguide/virtual_machines.html,
  https://docs.cloudstack.apache.org/en/4.21.0.0/adminguide/hosts.html
- Firecracker: https://github.com/firecracker-microvm/firecracker
- Kata Containers: https://katacontainers.io/
- Cloud Hypervisor: https://github.com/cloud-hypervisor/cloud-hypervisor

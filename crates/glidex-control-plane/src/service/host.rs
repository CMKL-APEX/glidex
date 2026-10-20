//! The host family (spec/grpc.md §9, G3): this host's networking through
//! glidex-netd, and its PCI devices. Every operation acts on the host that
//! answers it; an agent's UI has a server authorize it on the agent's
//! `Host` and run it there (`PreAuthorized`, api/relay.rs).

use super::{Call, ServiceError, ServiceResult};
use crate::authz::{Ent, EntitySet};
use crate::network::{NetError, Netd, NetdAccess};
use crate::service::common::ts;
use crate::state::VmManagerError;
use axum::http::StatusCode;
use glidex_api::proto::glidex::host::v1 as pb;
use glidex_netd::proto::{
    BridgeRecord, EnsureUplinkArgs, OnBehalfOf, Op, UplinkPhase, UplinkResult,
};
use glidex_ovs::bridge::{BridgeSpec, Datapath};
use glidex_ovs::install::{DpdkSettings, InstallReport, InstallRequest, Profile};
use glidex_ovs::uplink::{UplinkKind, UplinkSpec, XdpMode};

pub(crate) fn manager_err(e: VmManagerError) -> ServiceError {
    crate::api::error_to_response(e).into()
}

fn invalid(m: impl Into<String>) -> ServiceError {
    ServiceError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", m)
}

fn on_behalf_of(call: &Call) -> OnBehalfOf {
    OnBehalfOf {
        user: call
            .p
            .user_id()
            .map(String::from)
            .unwrap_or_else(|| call.p.display()),
        project: None,
        request_id: Some(call.request_id.clone()),
    }
}

/// Run a blocking netd call off the async runtime, on behalf of `call`.
async fn netd<T, F>(call: &Call, f: F) -> ServiceResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Netd, &OnBehalfOf) -> Result<T, NetError> + Send + 'static,
{
    let netd = call.manager().netd().clone();
    let who = on_behalf_of(call);
    tokio::task::spawn_blocking(move || f(&netd, &who))
        .await
        .map_err(|e| {
            ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
        })?
        .map_err(|e| manager_err(VmManagerError::Network(e)))
}

fn host(call: &Call) -> ServiceResult<()> {
    Ok(call.require(Ent::Host, EntitySet::new())?)
}

/// This node's uplinks changed: report what it has now (the shared edge's
/// defaults read it), without waiting for the next periodic report.
fn report_soon(call: &Call) {
    let m = call.app.manager.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = m.report_self() {
            tracing::warn!("reporting this node after an uplink change: {}", e);
        }
    });
}

// ---- conversions ----------------------------------------------------------

fn datapath_pb(d: Datapath) -> pb::Datapath {
    match d {
        Datapath::System => pb::Datapath::DATAPATH_SYSTEM,
        Datapath::Netdev => pb::Datapath::DATAPATH_NETDEV,
    }
}

fn datapath_of(d: pb::Datapath) -> ServiceResult<Datapath> {
    match d {
        pb::Datapath::DATAPATH_SYSTEM => Ok(Datapath::System),
        pb::Datapath::DATAPATH_NETDEV => Ok(Datapath::Netdev),
        pb::Datapath::DATAPATH_UNSPECIFIED => Err(invalid("datapath is required")),
    }
}

fn xdp_pb(m: XdpMode) -> pb::XdpMode {
    match m {
        XdpMode::BestEffort => pb::XdpMode::XDP_MODE_BEST_EFFORT,
        XdpMode::Native => pb::XdpMode::XDP_MODE_NATIVE,
        XdpMode::NativeWithZerocopy => pb::XdpMode::XDP_MODE_NATIVE_WITH_ZEROCOPY,
        XdpMode::Generic => pb::XdpMode::XDP_MODE_GENERIC,
    }
}

fn xdp_of(m: pb::XdpMode) -> XdpMode {
    match m {
        pb::XdpMode::XDP_MODE_NATIVE => XdpMode::Native,
        pb::XdpMode::XDP_MODE_NATIVE_WITH_ZEROCOPY => XdpMode::NativeWithZerocopy,
        pb::XdpMode::XDP_MODE_GENERIC => XdpMode::Generic,
        pb::XdpMode::XDP_MODE_BEST_EFFORT | pb::XdpMode::XDP_MODE_UNSPECIFIED => {
            XdpMode::BestEffort
        }
    }
}

fn caps_pb(c: &glidex_ovs::host::HostCapabilities) -> pb::HostCapabilities {
    pb::HostCapabilities {
        ovs_installed: Some(c.ovs_installed),
        ovs_version: c.ovs_version.clone(),
        ovs_version_supported: Some(c.ovs_version_supported),
        ovs_running: Some(c.ovs_running),
        iface_types: c.iface_types.clone(),
        datapath_types: c.datapath_types.clone(),
        ovs_dpdk_build: Some(c.ovs_dpdk_build),
        dpdk_mempool_driver: c.dpdk_mempool_driver,
        dpdk_initialized: Some(c.dpdk_initialized),
        hugepages: c
            .hugepages
            .iter()
            .map(|h| pb::Hugepages {
                size_kb: h.size_kb,
                total: h.total,
                free: h.free,
                ..Default::default()
            })
            .collect(),
        iommu: Some(c.iommu),
        ch_net_admin: c.ch_net_admin,
        dnsmasq: Some(c.dnsmasq),
        nft: Some(c.nft),
        ip_forward: Some(c.ip_forward),
        firewalls: c.firewalls.clone(),
        network_managers: c.network_managers.clone(),
        combinations: c
            .combinations
            .iter()
            .map(|x| pb::Combination {
                id: x.id.clone(),
                description: x.description.clone(),
                available: Some(x.available),
                missing: x.missing.clone(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn bridge_pb(b: &BridgeRecord) -> pb::Bridge {
    pb::Bridge {
        spec: pb::BridgeSpec {
            name: b.spec.name.clone(),
            datapath: datapath_pb(b.spec.datapath).into(),
            mtu: b.spec.mtu.map(u32::from),
            adopt: Some(b.spec.adopt),
            isolated: b.spec.isolated,
            ..Default::default()
        }
        .into(),
        live: b
            .live
            .as_ref()
            .map(|l| pb::BridgeInfo {
                name: l.name.clone(),
                datapath: datapath_pb(l.datapath).into(),
                owned: Some(l.owned),
                ports: l.ports.clone(),
                ..Default::default()
            })
            .into(),
        ..Default::default()
    }
}

fn uplink_spec_pb(s: &UplinkSpec) -> pb::UplinkSpec {
    let mut out = pb::UplinkSpec {
        name: s.name.clone(),
        bridge: s.bridge.clone(),
        migrate_ip: Some(s.migrate_ip),
        ..Default::default()
    };
    match &s.kind {
        UplinkKind::Kernel { ifname } => {
            out.kind = pb::UplinkKind::UPLINK_KIND_KERNEL.into();
            out.ifname = Some(ifname.clone());
        }
        UplinkKind::Afxdp {
            ifname,
            xdp_mode,
            n_rxq,
        } => {
            out.kind = pb::UplinkKind::UPLINK_KIND_AFXDP.into();
            out.ifname = Some(ifname.clone());
            out.xdp_mode = xdp_pb(*xdp_mode).into();
            out.n_rxq = Some(u32::from(*n_rxq));
        }
        UplinkKind::Dpdk { pci, n_rxq } => {
            out.kind = pb::UplinkKind::UPLINK_KIND_DPDK.into();
            out.pci = Some(pci.clone());
            out.n_rxq = Some(u32::from(*n_rxq));
        }
    }
    out
}

fn uplink_pb(u: &UplinkResult) -> pb::Uplink {
    let r = &u.record;
    pb::Uplink {
        record: pb::UplinkRecord {
            spec: uplink_spec_pb(&r.spec).into(),
            orig_driver: r.orig_driver.clone(),
            snapshot: r
                .snapshot
                .as_ref()
                .map(|s| pb::Snapshot {
                    ifname: s.ifname.clone(),
                    mac: s.mac.clone(),
                    addresses: s.addresses.clone(),
                    routes: s
                        .routes
                        .iter()
                        .map(|x| pb::Route {
                            dst: x.dst.clone(),
                            gateway: x.gateway.clone(),
                            metric: x.metric,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                })
                .into(),
            pending: r
                .pending
                .as_ref()
                .map(|p| pb::PendingCommit {
                    token: p.token.clone(),
                    deadline: ts(p.deadline).into(),
                    ..Default::default()
                })
                .into(),
            boot_rollback: r
                .boot_rollback
                .as_ref()
                .map(|b| pb::BootRollback {
                    at: ts(b.at).into(),
                    reason: b.reason.clone(),
                    ..Default::default()
                })
                .into(),
            accept_dhcp: r.accept_dhcp,
            ..Default::default()
        }
        .into(),
        phase: match u.phase {
            UplinkPhase::Active => pb::UplinkPhase::UPLINK_PHASE_ACTIVE,
            UplinkPhase::PendingCommit => pb::UplinkPhase::UPLINK_PHASE_PENDING_COMMIT,
        }
        .into(),
        live: pb::UplinkState {
            error: u.live.error.clone(),
            link_state: u.live.link_state.clone(),
            warnings: u.live.warnings.clone(),
            ..Default::default()
        }
        .into(),
        classification: u
            .classification
            .as_ref()
            .map(|c| pb::Classification {
                reasons: c.reasons.clone(),
                addresses: c.addresses.clone(),
                ..Default::default()
            })
            .into(),
        ..Default::default()
    }
}

/// Whether an uplink's address still waits for its commit.
pub fn pending(u: &pb::Uplink) -> bool {
    u.phase.as_known() == Some(pb::UplinkPhase::UPLINK_PHASE_PENDING_COMMIT)
}

fn rxq(n: Option<u32>) -> ServiceResult<u16> {
    match n {
        None => Ok(1),
        Some(n) => u16::try_from(n)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid("n_rxq is out of range")),
    }
}

fn uplink_kind_of(req: &pb::EnsureUplinkRequest) -> ServiceResult<UplinkKind> {
    let ifname = || {
        req.ifname
            .clone()
            .ok_or_else(|| invalid("this kind of uplink needs `ifname`"))
    };
    match req.kind.as_known() {
        Some(pb::UplinkKind::UPLINK_KIND_KERNEL) => Ok(UplinkKind::Kernel { ifname: ifname()? }),
        Some(pb::UplinkKind::UPLINK_KIND_AFXDP) => Ok(UplinkKind::Afxdp {
            ifname: ifname()?,
            xdp_mode: xdp_of(req.xdp_mode.as_known().unwrap_or_default()),
            n_rxq: rxq(req.n_rxq)?,
        }),
        Some(pb::UplinkKind::UPLINK_KIND_DPDK) => Ok(UplinkKind::Dpdk {
            pci: req
                .pci
                .clone()
                .ok_or_else(|| invalid("a DPDK uplink needs `pci`"))?,
            n_rxq: rxq(req.n_rxq)?,
        }),
        _ => Err(invalid("kind is required")),
    }
}

// ---- status and installation ----------------------------------------------

pub async fn ovs_status(
    call: &Call,
    _req: pb::GetOvsStatusRequest,
) -> ServiceResult<pb::GetOvsStatusResponse> {
    host(call)?;
    let netd = call.manager().netd().clone();
    let (access, result) = tokio::task::spawn_blocking(move || netd.probe())
        .await
        .map_err(|e| {
            ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
        })?;
    let access = match access {
        NetdAccess::Full => pb::NetdAccess::NETD_ACCESS_FULL,
        NetdAccess::Status => pb::NetdAccess::NETD_ACCESS_STATUS,
        NetdAccess::None => pb::NetdAccess::NETD_ACCESS_NONE,
    };
    let (available, error, host) = match result {
        Ok(v) => (
            true,
            None,
            serde_json::from_value::<glidex_ovs::host::HostCapabilities>(v)
                .ok()
                .map(|c| caps_pb(&c)),
        ),
        Err(e) => (false, Some(e.to_string()), None),
    };
    Ok(pb::GetOvsStatusResponse {
        netd: pb::NetdStatus {
            available: Some(available),
            access: access.into(),
            error,
            ..Default::default()
        }
        .into(),
        host: host.into(),
        ..Default::default()
    })
}

pub async fn install_ovs(
    call: &Call,
    req: pb::InstallOvsRequest,
) -> ServiceResult<pb::InstallOvsResponse> {
    host(call)?;
    let profile = match req.profile.as_known() {
        Some(pb::OvsProfile::OVS_PROFILE_KERNEL) => Profile::Kernel,
        Some(pb::OvsProfile::OVS_PROFILE_DPDK) => Profile::Dpdk,
        _ => return Err(invalid("profile is required")),
    };
    let args = InstallRequest {
        profile,
        source_build: req.source_build,
        confirm: req.confirm,
    };
    let r: InstallReport = netd(call, move |n, who| n.call_as(Op::InstallOvs(args), who)).await?;
    Ok(pb::InstallOvsResponse {
        changed: Some(r.changed),
        method: r.method,
        packages: r.packages,
        ovs_version: r.ovs_version,
        warnings: r.warnings,
        output_tail: r.output_tail,
        ..Default::default()
    })
}

pub async fn init_dpdk(
    call: &Call,
    req: pb::InitDpdkRequest,
) -> ServiceResult<pb::InitDpdkResponse> {
    host(call)?;
    let settings = DpdkSettings {
        socket_mem: req.socket_mem,
        pmd_cpu_mask: req.pmd_cpu_mask,
        confirm: req.confirm,
    };
    netd(call, move |n, who| {
        n.call_as::<serde_json::Value>(Op::InitDpdk(settings), who)
    })
    .await?;
    Ok(Default::default())
}

// ---- bridges --------------------------------------------------------------

pub async fn list_bridges(
    call: &Call,
    _req: pb::ListBridgesRequest,
) -> ServiceResult<pb::ListBridgesResponse> {
    host(call)?;
    let bridges: Vec<BridgeRecord> = netd(call, |n, _| n.call(Op::ListBridges)).await?;
    Ok(pb::ListBridgesResponse {
        bridges: bridges.iter().map(bridge_pb).collect(),
        ..Default::default()
    })
}

pub async fn create_bridge(
    call: &Call,
    req: pb::CreateBridgeRequest,
) -> ServiceResult<pb::CreateBridgeResponse> {
    call.set_target(format!("bridge:{}", req.name));
    host(call)?;
    let spec = BridgeSpec {
        name: req.name,
        datapath: datapath_of(req.datapath.as_known().unwrap_or_default())?,
        mtu: req
            .mtu
            .map(|m| u16::try_from(m).map_err(|_| invalid("mtu is out of range")))
            .transpose()?,
        adopt: req.adopt,
        isolated: req.isolated,
    };
    let b: BridgeRecord = netd(call, move |n, who| n.call_as(Op::EnsureBridge(spec), who)).await?;
    Ok(pb::CreateBridgeResponse {
        bridge: bridge_pb(&b).into(),
        ..Default::default()
    })
}

pub async fn delete_bridge(
    call: &Call,
    req: pb::DeleteBridgeRequest,
) -> ServiceResult<pb::DeleteBridgeResponse> {
    let name = req.name;
    call.set_target(format!("bridge:{name}"));
    host(call)?;
    if let Some(net) = call
        .manager()
        .list_networks()
        .map_err(manager_err)?
        .into_iter()
        .find(|n| n.bridge == name)
    {
        return Err(manager_err(VmManagerError::Network(NetError::Conflict(
            format!("bridge '{}' is used by network '{}'", name, net.name),
        ))));
    }
    call.manager().meter_final_sample().await;
    netd(call, move |n, who| {
        n.call_as::<serde_json::Value>(Op::DeleteBridge { name }, who)
    })
    .await?;
    Ok(Default::default())
}

// ---- uplinks --------------------------------------------------------------

pub async fn list_uplinks(
    call: &Call,
    req: pb::ListUplinksRequest,
) -> ServiceResult<pb::ListUplinksResponse> {
    host(call)?;
    let all: Vec<UplinkResult> = netd(call, |n, _| n.call(Op::ListUplinks)).await?;
    Ok(pb::ListUplinksResponse {
        uplinks: all
            .iter()
            .filter(|u| u.record.spec.bridge == req.bridge)
            .map(uplink_pb)
            .collect(),
        ..Default::default()
    })
}

pub async fn ensure_uplink(
    call: &Call,
    req: pb::EnsureUplinkRequest,
) -> ServiceResult<pb::EnsureUplinkResponse> {
    call.set_target(format!("uplink:{}/{}", req.bridge, req.name));
    // A confirmed uplink may move the host's IP: step-up (spec/security.md §7.1).
    let action = if req.confirm {
        "confirmUplink"
    } else {
        "ensureUplink"
    };
    call.require_action(action, Ent::Host, EntitySet::new(), &[])?;
    let args = EnsureUplinkArgs {
        spec: UplinkSpec {
            kind: uplink_kind_of(&req)?,
            name: req.name,
            bridge: req.bridge,
            migrate_ip: req.migrate_ip,
        },
        confirm: req.confirm,
        accept_dhcp: req.accept_dhcp,
    };
    let u: UplinkResult = netd(call, move |n, who| n.call_as(Op::EnsureUplink(args), who)).await?;
    report_soon(call);
    Ok(pb::EnsureUplinkResponse {
        uplink: uplink_pb(&u).into(),
        ..Default::default()
    })
}

pub async fn commit_uplink(
    call: &Call,
    req: pb::CommitUplinkRequest,
) -> ServiceResult<pb::CommitUplinkResponse> {
    call.set_target(format!("uplink:{}/{}", req.bridge, req.name));
    host(call)?;
    call.manager().meter_final_sample().await;
    let u: UplinkResult = netd(call, move |n, who| {
        n.call_as(
            Op::CommitUplink {
                bridge: req.bridge,
                name: req.name,
                token: req.token,
            },
            who,
        )
    })
    .await?;
    report_soon(call);
    Ok(pb::CommitUplinkResponse {
        uplink: uplink_pb(&u).into(),
        ..Default::default()
    })
}

pub async fn reapply_uplink(
    call: &Call,
    req: pb::ReapplyUplinkRequest,
) -> ServiceResult<pb::ReapplyUplinkResponse> {
    call.set_target(format!("uplink:{}/{}", req.bridge, req.name));
    // It moves the host's IP again: as a confirmed uplink, step-up.
    call.require_action("confirmUplink", Ent::Host, EntitySet::new(), &[])?;
    let u: UplinkResult = netd(call, move |n, who| {
        n.call_as(
            Op::ReapplyUplink {
                bridge: req.bridge,
                name: req.name,
            },
            who,
        )
    })
    .await?;
    report_soon(call);
    Ok(pb::ReapplyUplinkResponse {
        uplink: uplink_pb(&u).into(),
        ..Default::default()
    })
}

pub async fn delete_uplink(
    call: &Call,
    req: pb::DeleteUplinkRequest,
) -> ServiceResult<pb::DeleteUplinkResponse> {
    call.set_target(format!("uplink:{}/{}", req.bridge, req.name));
    host(call)?;
    call.manager().meter_final_sample().await;
    netd(call, move |n, who| {
        n.call_as::<serde_json::Value>(
            Op::DeleteUplink {
                bridge: req.bridge,
                name: req.name,
            },
            who,
        )
    })
    .await?;
    report_soon(call);
    Ok(Default::default())
}

// ---- PCI devices ----------------------------------------------------------

pub fn list_pci_devices(
    call: &Call,
    _req: pb::ListPciDevicesRequest,
) -> ServiceResult<pb::ListPciDevicesResponse> {
    host(call)?;
    // Readiness from the tree (spec/host-topology.md §5.1).
    let tree = call.manager().local_topology();
    let ready = |a: &str| tree.devices.iter().find(|d| d.address == a && d.assignable);
    Ok(pb::ListPciDevicesResponse {
        devices: crate::pci::scan_pci_devices()
            .into_iter()
            .map(|d| {
                let r = ready(&d.address);
                pb::PciDevice {
                    numa_node: d.numa_node,
                    ready: r.map(|f| f.ready),
                    not_ready: r.map(|f| f.not_ready.clone()).unwrap_or_default(),
                    address: d.address,
                    vendor_id: d.vendor_id,
                    device_id: d.device_id,
                    class_id: d.class_id,
                    driver: d.driver,
                    iommu_group: d.iommu_group,
                    sysfs_path: d.sysfs_path,
                    ..Default::default()
                }
            })
            .collect(),
        ..Default::default()
    })
}

/// `GET /host/topology`: this host's NUMA and PCIe topology.
pub fn get_host_topology(
    call: &Call,
    _req: pb::GetHostTopologyRequest,
) -> ServiceResult<pb::GetHostTopologyResponse> {
    host(call)?;
    Ok(pb::GetHostTopologyResponse {
        topology: topology_pb(&call.manager().local_topology()).into(),
        ..Default::default()
    })
}

/// A topology tree in its proto form.
pub(crate) fn topology_pb(t: &crate::topology::TopologyTree) -> pb::Topology {
    use crate::topology::PciKind;
    let s = &t.summary;
    pb::Topology {
        summary: pb::TopologySummary {
            numa_nodes: s
                .numa_nodes
                .iter()
                .map(|n| pb::NumaNode {
                    id: n.id,
                    cores: n
                        .cores
                        .iter()
                        .map(|c| pb::CpuCore {
                            threads: c.clone(),
                            ..Default::default()
                        })
                        .collect(),
                    memory_mib: n.memory_mib,
                    hugepages: n.hugepages.clone().into_iter().collect(),
                    ..Default::default()
                })
                .collect(),
            isolated: s.isolated.clone(),
            reserved_cpus: s.reserved_cpus.clone().into_iter().collect(),
            reserved_by_config: s.reserved_by_config,
            device_numa: s.device_numa.clone().into_iter().collect(),
            ..Default::default()
        }
        .into(),
        devices: t
            .devices
            .iter()
            .map(|d| pb::PciFunction {
                address: d.address.clone(),
                vendor_id: d.vendor_id.clone(),
                device_id: d.device_id.clone(),
                class_id: d.class_id.clone(),
                driver: d.driver.clone(),
                iommu_group: d.iommu_group.clone(),
                numa_node: d.numa_node,
                parent: d.parent.clone(),
                kind: match d.kind {
                    PciKind::Endpoint => pb::PciKind::PCI_KIND_ENDPOINT,
                    PciKind::RootPort => pb::PciKind::PCI_KIND_ROOT_PORT,
                    PciKind::SwitchUpstream => pb::PciKind::PCI_KIND_SWITCH_UPSTREAM,
                    PciKind::SwitchDownstream => pb::PciKind::PCI_KIND_SWITCH_DOWNSTREAM,
                }
                .into(),
                root_port: d.root_port.clone(),
                switch: d.switch.clone(),
                link: d
                    .link
                    .as_ref()
                    .map(|l| pb::PcieLink {
                        current_speed: l.current_speed.clone(),
                        current_width: l.current_width,
                        max_speed: l.max_speed.clone(),
                        max_width: l.max_width,
                        ..Default::default()
                    })
                    .into(),
                sriov: d
                    .sriov
                    .as_ref()
                    .map(|v| pb::Sriov {
                        total_vfs: v.total_vfs,
                        enabled_vfs: v.enabled_vfs,
                        vf_driver: v.vf_driver.clone(),
                        ready_vfs: v.ready_vfs,
                        ..Default::default()
                    })
                    .into(),
                physfn: d.physfn.clone(),
                assignable: d.assignable,
                ready: d.ready,
                not_ready: d.not_ready.clone(),
                aer: d
                    .aer
                    .map(|a| pb::AerCounts {
                        correctable: a.correctable,
                        nonfatal: a.nonfatal,
                        fatal: a.fatal,
                        ..Default::default()
                    })
                    .into(),
                ..Default::default()
            })
            .collect(),
        warnings: t
            .warnings
            .iter()
            .map(|w| pb::TopologyWarning {
                kind: w.kind.clone(),
                detail: w.detail.clone(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

//! Probe parsing against the canned outputs of the test DPU
//! (`tests/fixtures/`, spec/31-dpu.md D0 acceptance 5).

use glidex_dpu::VfId;
use glidex_dpu::probe::{ControllerState, DpuCapabilities, PROBE_NAMES};
use glidex_dpu::virtnet::{self, FunctionType};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn dir(set: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(set)
}

fn raw(set: &str) -> BTreeMap<String, String> {
    PROBE_NAMES
        .iter()
        .map(|n| {
            let path = dir(set).join(format!("dpu-{n}.txt"));
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (n.to_string(), text)
        })
        .collect()
}

#[test]
fn the_prepared_dpu_has_no_gaps() {
    for set in ["pf-only", "one-vf"] {
        let caps = DpuCapabilities::from_raw(&raw(set));
        assert!(caps.missing.is_empty(), "{set}: {:?}", caps.missing);
        assert_eq!(caps.ovs_version.as_deref(), Some("3.5.0052"));
        assert!(caps.doca_initialized);
        assert_eq!(caps.doca_version.as_deref(), Some("3.5.0098"));
        assert!(caps.iface_types.iter().any(|t| t == "doca"));
        assert!(!caps.iface_types.iter().any(|t| t == "dpdk"));
        assert_eq!(caps.datapath_types, ["doca", "netdev", "system"]);
        assert_eq!(caps.dpdk_hugepage_dir.as_deref(), Some("/hugepages-2M"));
        assert_eq!(caps.pmd_cpu_mask.as_deref(), Some("0xc0"));
        assert_eq!(caps.other_config["hw-offload"], "true");
        let two_m = caps.hugepages.iter().find(|p| p.size_kib == 2048).unwrap();
        assert_eq!(two_m.total, 1024);
        assert_eq!(caps.pmd_threads, 2);
        assert_eq!(caps.eswitch_mode.as_deref(), Some("switchdev"));
        assert_eq!(caps.virtio_controller, Some(ControllerState::Active));
        assert_eq!(caps.virtio_controller_version.as_deref(), Some("v26.07.10"));
        assert_eq!(caps.virtio_pfs, ["61:00.1"]);
        assert_eq!(caps.firmware["VIRTIO_NET_EMULATION_ENABLE"], "True");
        assert_eq!(caps.firmware["VIRTIO_NET_EMULATION_NUM_VF"], "16");
        assert_eq!(caps.firmware["INTERNAL_CPU_MODEL"], "EMBEDDED_CPU");
        assert_eq!(caps.firmware.len(), 12);
        assert!(!caps.local_ovn_central);
        assert!(!caps.ovn_controller_active);
        // Not a chassis yet: the image's random system-id alone isn't one.
        assert_eq!(caps.chassis, None);
        let names: Vec<_> = caps.bridges.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["ovsbr1", "br-int"]);
        assert!(
            caps.bridges
                .iter()
                .all(|b| b.datapath_type.as_deref() == Some("doca"))
        );
        assert_eq!(caps.bridges[1].fail_mode.as_deref(), Some("secure"));
        assert_eq!(caps.bridges[0].fail_mode, None);
    }
}

#[test]
fn the_probe_matches_what_the_script_derived() {
    for set in ["pf-only", "one-vf"] {
        let caps = DpuCapabilities::from_raw(&raw(set));
        let script: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir(set).join("probe.json")).unwrap())
                .unwrap();
        assert_eq!(
            caps.offloaded_flows,
            script["offloaded_flows"].as_u64().unwrap(),
            "{set}"
        );
        assert_eq!(
            u64::from(caps.pmd_threads),
            script["pmd_threads"].as_u64().unwrap()
        );
        assert_eq!(
            caps.eswitch_mode.as_deref(),
            script["eswitch_mode"].as_str()
        );
        for (k, v) in script["firmware"].as_object().unwrap() {
            assert_eq!(
                caps.firmware.get(k).map(String::as_str),
                v.as_str(),
                "{set} {k}"
            );
        }
    }
}

#[test]
fn virtnet_list_gives_the_pf_its_vfs_and_their_representors() {
    let list = virtnet::parse_list(&raw("pf-only")["virtnet_list"]).unwrap();
    assert_eq!(list.devices.len(), 1);
    let pf = &list.devices[0];
    assert_eq!(pf.function, FunctionType::StaticPf);
    assert_eq!(pf.bdf, "61:00.1");
    assert_eq!(pf.representor.as_deref(), Some("en3f0pf0sf1000"));
    assert_eq!(list.vf(VfId { pf: 0, vf: 0 }), None);

    let list = virtnet::parse_list(&raw("one-vf")["virtnet_list"]).unwrap();
    assert_eq!(list.devices.len(), 2);
    let vf = list.vf(VfId { pf: 0, vf: 0 }).unwrap();
    assert_eq!(vf.bdf, "61:02.3");
    assert_eq!(vf.mtu, Some(1442));
    assert_eq!(vf.max_queue_pairs, Some(5));
    assert_eq!(
        list.representor(VfId { pf: 0, vf: 0 }),
        Some("en3f0pf0sf3000")
    );
    let caps = DpuCapabilities::from_raw(&raw("one-vf"));
    assert_eq!(caps.virtio_vfs_enabled, 1);

    // `virtnet query -a -b` has the same shape.
    let q = std::fs::read_to_string(dir("one-vf").join("dpu-virtnet_query.txt")).unwrap();
    let list = virtnet::parse_list(&q).unwrap();
    assert_eq!(
        list.representor(VfId { pf: 0, vf: 0 }),
        Some("en3f0pf0sf3000")
    );
}

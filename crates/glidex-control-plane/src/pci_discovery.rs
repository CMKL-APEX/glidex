//! Discovered device pools (spec/22-host-topology.md H21, H24, §9.1b): GPUs,
//! accelerators and NICs no defined pool covers, grouped by model across
//! the nodes that report them, and NICs' VFs by their PF's model. Every server works them out the same way from the
//! replicated node summaries, so they need no write.

use crate::config::{PciPool, PoolMatch};
use crate::node::Node;
use crate::topology::SummaryDevice;
use std::collections::{BTreeMap, BTreeSet};

fn hex4(id: &str) -> String {
    let h = id.trim().trim_start_matches("0x").to_ascii_lowercase();
    format!("{h:0>4}")
}

pub use glidex_topology::pci::{
    pool_slug as slug, pool_slug_ids as ids_of, pool_vf_slug as vf_slug,
    pool_vf_slug_ids as vf_ids_of,
};

/// Display controllers of server management controllers (BMCs): ASPEED and
/// Matrox's G200 server parts. They are the host's console, not GPUs.
const BMC_DISPLAY_VENDORS: &[&str] = &["1a03", "102b"];

/// `gpu` for display controllers (not a BMC's), `accelerator` for
/// processing accelerators and coprocessors, `nic` for network controllers
/// (Ethernet, InfiniBand, H24); other classes aren't discovered.
fn class_of(vendor_id: &str, class_id: &str) -> Option<&'static str> {
    let c = class_id
        .trim()
        .trim_start_matches("0x")
        .to_ascii_lowercase();
    if c.starts_with("03") {
        (!BMC_DISPLAY_VENDORS.contains(&hex4(vendor_id).as_str())).then_some("gpu")
    } else if c.starts_with("12") || c.starts_with("0b40") {
        Some("accelerator")
    } else if c.starts_with("02") {
        Some("nic")
    } else {
        None
    }
}

/// The discovered pool a device would join: its class, its model (a VF's
/// is its PF's) and whether it is a VF. Only NICs' VFs are discovered.
fn model_of(d: &SummaryDevice) -> Option<(&'static str, (String, String, bool))> {
    match &d.physfn {
        None => class_of(&d.vendor_id, &d.class_id)
            .map(|c| (c, (hex4(&d.vendor_id), hex4(&d.device_id), false))),
        Some(pf) => class_of(&pf.vendor_id, &pf.class_id)
            .filter(|c| *c == "nic")
            .map(|c| (c, (hex4(&pf.vendor_id), hex4(&pf.device_id), true))),
    }
}

fn on_node(p: &PciPool, n: &Node) -> bool {
    p.nodes.is_empty() || p.nodes.iter().any(|x| *x == n.meta.id || *x == n.spec.name)
}

/// The discovered pools over `nodes`, given the `defined` ones.
pub fn discover(defined: &[PciPool], nodes: &[Node]) -> Vec<PciPool> {
    // (vendor, device, of VFs) → (class, nodes with an uncovered device of it)
    let mut found: BTreeMap<(String, String, bool), (&'static str, BTreeSet<String>)> =
        BTreeMap::new();
    for n in nodes.iter().filter(|n| !n.status.phase.is_tombstone()) {
        let Some(t) = n.status.topology.as_ref() else {
            continue;
        };
        for d in &t.devices {
            if d.excluded {
                continue;
            }
            let Some((class, model)) = model_of(d) else {
                continue;
            };
            if defined
                .iter()
                .any(|p| on_node(p, n) && p.matches.matches_device(d))
            {
                continue;
            }
            found
                .entry(model)
                .or_insert((class, BTreeSet::new()))
                .1
                .insert(n.meta.id.clone());
        }
    }
    found
        .into_iter()
        .map(|((v, d, vf), (class, on))| PciPool {
            name: if vf { vf_slug(&v, &d) } else { slug(&v, &d) },
            class: Some(class.into()),
            matches: {
                let ids = PoolMatch {
                    vendor_id: Some(format!("0x{v}")),
                    device_id: Some(format!("0x{d}")),
                    ..Default::default()
                };
                if vf {
                    PoolMatch {
                        vf_of: Some(Box::new(ids)),
                        ..Default::default()
                    }
                } else {
                    ids
                }
            },
            nodes: on.into_iter().collect(),
            prepare: false,
            per_device: None,
            bar_mib: None,
        })
        .filter(|p| !defined.iter().any(|q| q.name == p.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::TopologySummary;

    fn dev(a: &str, vendor: &str, device: &str, class: &str) -> SummaryDevice {
        SummaryDevice {
            address: a.into(),
            vendor_id: vendor.into(),
            device_id: device.into(),
            class_id: class.into(),
            ready: true,
            ..Default::default()
        }
    }

    fn node(id: &str, devices: Vec<SummaryDevice>) -> Node {
        let mut n = Node::new(
            id,
            crate::node::NodeSpec {
                name: id.into(),
                role: crate::node::NodeRole::Agent,
                unschedulable: false,
                labels: Default::default(),
                br_int_datapath: Default::default(),
                bridge_mappings: Default::default(),
                drop_spare_files_before: None,
                cordoned_devices: Default::default(),
            },
        );
        n.status.topology = Some(TopologySummary {
            devices,
            ..Default::default()
        });
        n
    }

    fn r9700(name: &str) -> PciPool {
        PciPool {
            name: name.into(),
            class: Some("gpu".into()),
            matches: PoolMatch {
                vendor_id: Some("0x1002".into()),
                device_id: Some("0x7551".into()),
                ..Default::default()
            },
            nodes: vec![],
            prepare: false,
            per_device: None,
            bar_mib: None,
        }
    }

    #[test]
    fn slugs_name_the_model_and_read_back() {
        assert_eq!(slug("0x1002", "0x7551"), "amd-7551");
        assert_eq!(slug("0x10DE", "0x2330"), "nvidia-2330");
        assert_eq!(slug("0x15b3", "0xa2dc"), "15b3-a2dc");
        assert_eq!(ids_of("amd-7551"), Some(("0x1002".into(), "0x7551".into())));
        assert_eq!(
            ids_of("15b3-a2dc"),
            Some(("0x15b3".into(), "0xa2dc".into()))
        );
        assert_eq!(ids_of("r9700"), None);
        assert_eq!(vf_slug("0x15b3", "0x1021"), "15b3-1021-vf");
        assert_eq!(
            vf_ids_of("15b3-1021-vf"),
            Some(("0x15b3".into(), "0x1021".into()))
        );
        // A VF pool's name isn't a whole device's, nor the other way.
        assert_eq!(ids_of("15b3-1021-vf"), None);
        assert_eq!(vf_ids_of("15b3-1021"), None);
    }

    #[test]
    fn nics_and_their_vfs_form_pools_by_the_cards_model() {
        let pf = |a: &str| SummaryDevice {
            ready: false,
            ..dev(a, "0x15b3", "0x1021", "0x020000")
        };
        let vf = |a: &str, of: &str| SummaryDevice {
            physfn: Some(crate::topology::PhysFn {
                address: of.into(),
                vendor_id: "0x15b3".into(),
                device_id: "0x1021".into(),
                class_id: "0x020000".into(),
            }),
            ..dev(a, "0x15b3", "0x101e", "0x020000")
        };
        // A VF of a GPU stays out: only NICs' VFs are discovered.
        let gpu_vf = SummaryDevice {
            physfn: Some(crate::topology::PhysFn {
                address: "0000:c1:00.0".into(),
                vendor_id: "0x1002".into(),
                device_id: "0x7551".into(),
                class_id: "0x030000".into(),
            }),
            ..dev("0000:c1:00.1", "0x1002", "0x7552", "0x030000")
        };
        let nodes = vec![
            node(
                "a",
                vec![
                    pf("0000:41:00.0"),
                    vf("0000:41:00.2", "0000:41:00.0"),
                    vf("0000:41:00.3", "0000:41:00.0"),
                    gpu_vf,
                    // InfiniBand is a network controller too.
                    dev("0000:61:00.0", "0x15b3", "0x1021", "0x020700"),
                ],
            ),
            node("b", vec![pf("0000:81:00.0")]),
        ];
        let found = discover(&[], &nodes);
        let names: Vec<_> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["15b3-1021", "15b3-1021-vf"]);
        let (cards, vfs) = (&found[0], &found[1]);
        assert_eq!(cards.class.as_deref(), Some("nic"));
        assert_eq!(cards.nodes, ["a", "b"]);
        assert_eq!(cards.matches.device_id.as_deref(), Some("0x1021"));
        assert!(!cards.prepare);
        assert_eq!(vfs.class.as_deref(), Some("nic"));
        assert_eq!(vfs.nodes, ["a"]);
        assert!(vfs.matches.vendor_id.is_none() && vfs.matches.device_id.is_none());
        let of = vfs.matches.vf_of.as_deref().unwrap();
        assert_eq!(of.vendor_id.as_deref(), Some("0x15b3"));
        assert_eq!(of.device_id.as_deref(), Some("0x1021"));
        assert!(
            nodes[0].status.topology.as_ref().unwrap().devices[1..3]
                .iter()
                .all(|d| vfs.matches.matches_device(d))
        );

        // A defined pool of the VFs takes them out of discovery; the cards
        // stay.
        let defined = PciPool {
            name: "cx7-vfs".into(),
            class: Some("nic".into()),
            matches: PoolMatch {
                vf_of: Some(Box::new(PoolMatch {
                    vendor_id: Some("0x15b3".into()),
                    device_id: Some("0x1021".into()),
                    ..Default::default()
                })),
                ..Default::default()
            },
            ..r9700("cx7-vfs")
        };
        let names: Vec<_> = discover(&[defined], &nodes)
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["15b3-1021"]);
    }

    #[test]
    fn gpus_no_pool_covers_form_a_pool_per_model() {
        let nodes = vec![
            node(
                "gpu-dev2",
                vec![
                    dev("0000:03:00.0", "0x1002", "0x7551", "0x030000"),
                    dev("0000:43:00.0", "0x1002", "0x7551", "0x030000"),
                    // A NIC is (H24); NVMe and the BMC's console aren't.
                    dev("0000:41:00.0", "0x15b3", "0x101b", "0x020000"),
                    dev("0000:c1:00.0", "0x144d", "0xa80a", "0x010802"),
                    dev("0000:c2:00.0", "0x1a03", "0x2000", "0x030000"),
                ],
            ),
            node(
                "prism1",
                vec![dev("0000:17:00.0", "0x10de", "0x20b0", "0x030200")],
            ),
        ];
        let found = discover(&[], &nodes);
        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        // Sorted by vendor and device id: 1002, 10de, 15b3.
        assert_eq!(names, ["amd-7551", "nvidia-20b0", "15b3-101b"]);
        assert_eq!(found[2].class.as_deref(), Some("nic"));
        assert_eq!(found[0].nodes, ["gpu-dev2"]);
        assert_eq!(found[0].class.as_deref(), Some("gpu"));
        assert!(!found[0].prepare);
        assert!(
            found[0]
                .matches
                .matches_device(&nodes[0].status.topology.as_ref().unwrap().devices[0])
        );
    }

    #[test]
    fn a_defined_pool_an_exclude_and_a_name_clash_each_remove_a_device() {
        let mut kept = dev("0000:43:00.0", "0x1002", "0x7551", "0x030000");
        kept.excluded = true;
        let nodes = vec![node(
            "gpu-dev2",
            vec![dev("0000:03:00.0", "0x1002", "0x7551", "0x030000"), kept],
        )];
        assert_eq!(discover(&[], &nodes)[0].name, "amd-7551");
        // The pool covers the model: nothing left to discover.
        assert!(discover(&[r9700("r9700")], &nodes).is_empty());
        // A defined pool on another node only doesn't cover this one's.
        let mut elsewhere = r9700("r9700");
        elsewhere.nodes = vec!["prism1".into()];
        assert_eq!(discover(&[elsewhere], &nodes).len(), 1);
        // Only the excluded card left: no pool.
        let only_kept = vec![node(
            "gpu-dev2",
            vec![nodes[0].status.topology.as_ref().unwrap().devices[1].clone()],
        )];
        assert!(discover(&[], &only_kept).is_empty());
        // A defined pool already called amd-7551 (matching something else)
        // keeps the name; no discovered pool is formed.
        let mut clash = r9700("amd-7551");
        clash.matches.device_id = Some("0x7550".into());
        assert!(discover(&[clash], &nodes).is_empty());
    }
}

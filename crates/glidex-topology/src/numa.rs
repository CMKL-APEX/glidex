//! NUMA nodes, their CPUs and the socket each belongs to.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Numa {
    pub id: u32,
    /// Its online CPUs.
    pub cpus: Vec<u32>,
    /// The socket (`physical_package_id`) of its first CPU; a node with no
    /// CPUs (memory or devices only) has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<u32>,
}

/// `0-3,8,10-11` as CPUs.
pub fn parse_list(list: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in list.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    out.extend(a..=b);
                }
            }
            None => out.extend(part.parse::<u32>().ok()),
        }
    }
    out
}

/// The NUMA nodes under `sys`; one node 0 holding every online CPU on a
/// host without NUMA information.
pub fn read(sys: &Path) -> Vec<Numa> {
    let text = |p: &Path| std::fs::read_to_string(p).ok();
    let socket = |cpu: u32| {
        text(&sys.join(format!(
            "devices/system/cpu/cpu{cpu}/topology/physical_package_id"
        )))
        .and_then(|s| s.trim().parse().ok())
    };
    let nodes = text(&sys.join("devices/system/node/online"))
        .map(|s| parse_list(&s))
        .unwrap_or_default();
    if nodes.is_empty() {
        let cpus = text(&sys.join("devices/system/cpu/online"))
            .map(|s| parse_list(&s))
            .unwrap_or_default();
        let socket = cpus.first().and_then(|c| socket(*c));
        return vec![Numa {
            id: 0,
            cpus,
            socket,
        }];
    }
    nodes
        .into_iter()
        .map(|id| {
            let cpus = text(&sys.join(format!("devices/system/node/node{id}/cpulist")))
                .map(|s| parse_list(&s))
                .unwrap_or_default();
            let socket = cpus.first().and_then(|c| socket(*c));
            Numa { id, cpus, socket }
        })
        .collect()
}

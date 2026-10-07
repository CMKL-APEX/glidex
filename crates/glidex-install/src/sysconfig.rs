//! Persistent host settings glidex needs, as drop-in files it owns:
//!
//! - `/etc/sysctl.d/90-glidex.conf`: `net.ipv4.ip_forward=1` (NAT) and, for
//!   OVS-DPDK, `vm.nr_hugepages`. Each setting records the value it had
//!   before glidex (`# glidex-previous: key=value`), so the uninstaller can
//!   put it back. Re-running the installer keeps the *original* value.
//! - `/etc/modules-load.d/glidex.conf`: `vfio-pci` for DPDK NIC uplinks.

use std::collections::BTreeMap;
use std::path::PathBuf;

pub const SYSCTL_DROPIN: &str = "/etc/sysctl.d/90-glidex.conf";
pub const MODULES_DROPIN: &str = "/etc/modules-load.d/glidex.conf";
pub const IP_FORWARD: &str = "net.ipv4.ip_forward";
pub const NR_HUGEPAGES: &str = "vm.nr_hugepages";
const PREVIOUS: &str = "# glidex-previous: ";

/// 2 MiB hugepages: 4 GiB for OVS-DPDK's mempools plus 8 GiB for
/// hugepage-backed vhost-user guests. At MTU 9000 OVS allocates a second,
/// fixed 262144-mbuf mempool of about 2.5 GiB next to the ~0.8 GiB one for
/// MTU 1500; a 4 GiB pool ran out as soon as a second guest started.
pub const DEFAULT_HUGEPAGES: u64 = 6144;
/// Below this the pool can't hold OVS's jumbo-frame mempools (about
/// 3.3 GiB at MTU 9000) and a few guests; the installer warns.
pub const JUMBO_HUGEPAGES: u64 = 3072;
/// DPDK socket memory ceiling (MiB), enough to pre-allocate both mempools.
pub const MAX_SOCKET_MEM_MB: u64 = 4096;
/// Below this OVS-DPDK's default mempool doesn't fit.
pub const MIN_HUGEPAGES: u64 = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    pub value: String,
    /// Value before glidex changed it; `None` if unknown.
    pub previous: Option<String>,
}

/// `/proc/sys/net/ipv4/ip_forward` for `net.ipv4.ip_forward`.
pub fn proc_path(key: &str) -> PathBuf {
    PathBuf::from(format!("/proc/sys/{}", key.replace('.', "/")))
}

pub fn render(settings: &[Setting]) -> String {
    let mut s = String::from(
        "# Managed by glidex-install; removed (and previous values restored)\n\
         # by `glidex-install uninstall`.\n",
    );
    for st in settings {
        if let Some(prev) = &st.previous {
            s.push_str(&format!("{}{}={}\n", PREVIOUS, st.key, prev));
        }
        s.push_str(&format!("{} = {}\n", st.key, st.value));
    }
    s
}

pub fn parse(contents: &str) -> Vec<Setting> {
    let mut previous: BTreeMap<String, String> = BTreeMap::new();
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(PREVIOUS) {
            if let Some((k, v)) = rest.split_once('=') {
                previous.insert(k.trim().to_string(), v.trim().to_string());
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim().to_string();
                out.push(Setting {
                    previous: previous.get(&key).cloned(),
                    key,
                    value: v.trim().to_string(),
                });
            }
        }
    }
    out
}

/// Combine an existing drop-in with the settings wanted now. Previous
/// values come from the existing file when it has one for the key (so a
/// re-run never records glidex's own value as "previous"), else from the
/// live value. Settings only in the existing file are kept.
pub fn merge(
    existing: Option<&str>,
    wanted: &[(&str, String)],
    current: impl Fn(&str) -> Option<String>,
) -> Vec<Setting> {
    let mut settings = existing.map(parse).unwrap_or_default();
    for (key, value) in wanted {
        match settings.iter_mut().find(|s| s.key == *key) {
            Some(s) => s.value = value.clone(),
            None => settings.push(Setting {
                key: key.to_string(),
                value: value.clone(),
                previous: current(key),
            }),
        }
    }
    settings
}

/// Hugepages to reserve given total RAM: the default, capped at a third
/// of RAM, never lowering what's already reserved.
pub fn hugepages_for(mem_total_kb: u64, already_reserved: u64) -> u64 {
    let third = mem_total_kb / 3 / 2048;
    DEFAULT_HUGEPAGES.min(third).max(already_reserved)
}

/// DPDK socket memory (MiB) for a hugepage pool: half the pool, up to
/// [`MAX_SOCKET_MEM_MB`]; the rest stays free for guests.
pub fn socket_mem_mb(hugepages: u64) -> u64 {
    // pages × 2 MiB ÷ 2 = `hugepages` MiB.
    hugepages.min(MAX_SOCKET_MEM_MB)
}

/// Uninstall: which settings to restore now, and which to keep in the
/// drop-in. Hugepages are kept while OVS-DPDK stays configured (it would
/// fail to start without them).
pub fn split_for_uninstall(settings: &[Setting], keep_hugepages: bool) -> (Vec<Setting>, Vec<Setting>) {
    settings
        .iter()
        .cloned()
        .partition(|s| !(keep_hugepages && s.key == NR_HUGEPAGES))
}

pub fn mem_total_kb(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_parse_round_trip() {
        let settings = vec![
            Setting { key: IP_FORWARD.into(), value: "1".into(), previous: Some("0".into()) },
            Setting { key: NR_HUGEPAGES.into(), value: "2048".into(), previous: Some("0".into()) },
        ];
        let text = render(&settings);
        assert!(text.contains("# glidex-previous: net.ipv4.ip_forward=0\nnet.ipv4.ip_forward = 1\n"));
        assert_eq!(parse(&text), settings);
    }

    #[test]
    fn rerun_keeps_the_original_previous_value() {
        // First install recorded ip_forward's original 0; it's 1 now because of glidex.
        let first = render(&[Setting { key: IP_FORWARD.into(), value: "1".into(), previous: Some("0".into()) }]);
        let merged = merge(Some(&first), &[(IP_FORWARD, "1".into()), (NR_HUGEPAGES, "2048".into())], |k| {
            Some(if k == IP_FORWARD { "1".into() } else { "0".into() })
        });
        assert_eq!(merged[0].previous.as_deref(), Some("0"), "not glidex's own 1");
        assert_eq!(merged[1], Setting { key: NR_HUGEPAGES.into(), value: "2048".into(), previous: Some("0".into()) });
        // A kernel-profile re-run keeps the earlier hugepage setting.
        let second = render(&merged);
        let again = merge(Some(&second), &[(IP_FORWARD, "1".into())], |_| None);
        assert!(again.iter().any(|s| s.key == NR_HUGEPAGES));
    }

    #[test]
    fn hugepage_sizing() {
        assert_eq!(hugepages_for(64 * 1024 * 1024, 0), 6144, "64 GiB host: 12 GiB of hugepages");
        assert_eq!(hugepages_for(31_293 * 1024, 0), 5338, "31 GiB host: capped at a third");
        assert_eq!(hugepages_for(4 * 1024 * 1024, 0), 682, "4 GiB host: capped at a third");
        assert_eq!(hugepages_for(64 * 1024 * 1024, 8192), 8192, "never lower an existing reservation");
        assert_eq!(socket_mem_mb(6144), 4096);
        assert_eq!(socket_mem_mb(5338), 4096);
        assert_eq!(socket_mem_mb(1024), 1024);
        assert_eq!(mem_total_kb("MemTotal:       42082304 kB\nMemFree: 1 kB\n"), Some(42_082_304));
        assert_eq!(proc_path(IP_FORWARD), PathBuf::from("/proc/sys/net/ipv4/ip_forward"));
    }

    #[test]
    fn uninstall_keeps_hugepages_while_ovs_dpdk_stays() {
        let settings = vec![
            Setting { key: IP_FORWARD.into(), value: "1".into(), previous: Some("0".into()) },
            Setting { key: NR_HUGEPAGES.into(), value: "2048".into(), previous: Some("0".into()) },
        ];
        let (restore, keep) = split_for_uninstall(&settings, true);
        assert_eq!(restore.len(), 1);
        assert_eq!(keep[0].key, NR_HUGEPAGES);
        let (restore, keep) = split_for_uninstall(&settings, false);
        assert_eq!((restore.len(), keep.len()), (2, 0));
    }
}

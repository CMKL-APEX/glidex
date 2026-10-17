//! OVN on a cluster member matches the cluster's ovn-northd
//! (spec/installer.md, OVN): an `ovn-controller` older than the northd that
//! compiles the logical flows can't run them (its tables stop short of the
//! newer pipeline), so a VM's traffic is dropped on that node.
//!
//! The member reads northd's version from the southbound database, which
//! its node certificate may read, and brings `ovn-host` (and `ovn-central`
//! where installed) up to that release: from the distribution when it is
//! new enough, else, on Ubuntu, from the Ubuntu Cloud Archive pocket that
//! has it, pinned so that nothing but OVN comes from there.

use anyhow::{Result, bail};

/// The cluster's files on a member (`<SERVICE_HOME>/.glidex/cluster`).
pub const CLUSTER_DIR: &str = "/var/lib/glidex-control-plane/.glidex/cluster";
pub const UCA: &str = "http://ubuntu-cloud.archive.canonical.com/ubuntu";
pub const UCA_LIST: &str = "/etc/apt/sources.list.d/glidex-uca-ovn.list";
pub const UCA_PIN: &str = "/etc/apt/preferences.d/glidex-uca-ovn";
pub const UCA_KEYRING: &str = "/usr/share/keyrings/ubuntu-cloud-keyring.gpg";

/// An OVN release series: `(major, minor)` of "26.03.0", of
/// "26.03.0-2~cloud0", of "ovn-controller 26.03.0" or of northd's
/// internal version "26.03.0-21.8.0-82.13".
pub fn series(v: &str) -> Option<(u32, u32)> {
    let v = v.trim().rsplit(' ').next()?;
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

pub fn series_str(s: (u32, u32)) -> String {
    format!("{}.{:02}", s.0, s.1)
}

/// `options:northd_internal_version` as `ovn-sbctl get` prints it.
pub fn northd_series(sb_value: &str) -> Option<(u32, u32)> {
    series(sb_value.trim().trim_matches('"'))
}

/// The `ovn-remote` OVS external id, unquoted; `None` when unset.
pub fn ovn_remote(vsctl_value: &str) -> Option<String> {
    let v = vsctl_value.trim().trim_matches('"').trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// `apt-cache policy` → the candidate version of the one package asked.
pub fn apt_candidate(policy: &str) -> Option<String> {
    policy
        .lines()
        .find_map(|l| l.trim().strip_prefix("Candidate:"))
        .map(str::trim)
        .filter(|v| *v != "(none)")
        .map(str::to_string)
}

/// The pockets a release has in the Cloud Archive, from the index page of
/// `dists/<codename>-updates/`.
pub fn pockets(index_html: &str) -> Vec<String> {
    let mut out: Vec<String> = index_html
        .split("href=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .filter_map(|h| h.strip_suffix('/'))
        .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_lowercase()))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `ovn-host`'s version in a `Packages` index.
pub fn ovn_host_version(packages: &str) -> Option<String> {
    packages
        .split("\n\n")
        .find(|stanza| stanza.lines().any(|l| l == "Package: ovn-host"))
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Version: ")))
        .map(|v| v.trim().to_string())
}

/// The pocket to take OVN from: the one with exactly the wanted series,
/// else the oldest newer one (a controller may be newer than northd, never
/// older). `offers` is (pocket, ovn-host version) as found.
pub fn pick_pocket(offers: &[(String, String)], want: (u32, u32)) -> Option<(String, String)> {
    offers
        .iter()
        .filter(|o| series(&o.1).is_some_and(|s| s >= want))
        .min_by_key(|o| series(&o.1))
        .cloned()
}

/// The apt source line for a pocket.
pub fn uca_source(codename: &str, pocket: &str) -> String {
    format!("deb [signed-by={UCA_KEYRING}] {UCA} {codename}-updates/{pocket} main\n")
}

/// Only OVN comes from the pocket: its Open vSwitch (and the rest) is
/// never installed, so another vendor's OVS on the host stays (a DOCA OVS
/// conflicts with the distribution's).
pub fn uca_pin(codename: &str, pocket: &str) -> String {
    format!(
        "# glidex: OVN to match the cluster's ovn-northd; nothing else from the Cloud Archive.\n\
         Package: ovn-*\nPin: release n={codename}-updates/{pocket}\nPin-Priority: 500\n\n\
         Package: *\nPin: release n={codename}-updates/{pocket}\nPin-Priority: -1\n"
    )
}

/// `apt-get -s` output: the packages it would remove.
pub fn removals(simulation: &str) -> Vec<String> {
    simulation
        .lines()
        .filter_map(|l| l.strip_prefix("Remv "))
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// What to do, from the versions found.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// The local ovn-controller is the northd's release or newer.
    Matches,
    /// Upgrade from the distribution's own archive.
    Distro { candidate: String },
    /// Upgrade from this Cloud Archive pocket.
    CloudArchive { pocket: String, version: String },
}

/// Decide. `local` and `want` are series; `distro` the distribution's
/// candidate; `uca` the Cloud Archive offers (empty when not Ubuntu or
/// unreachable).
pub fn plan(
    local: Option<(u32, u32)>,
    want: (u32, u32),
    distro: Option<&str>,
    uca: &[(String, String)],
) -> Result<Plan> {
    if local.is_some_and(|l| l >= want) {
        return Ok(Plan::Matches);
    }
    if let Some(c) = distro.filter(|c| series(c).is_some_and(|s| s >= want)) {
        return Ok(Plan::Distro {
            candidate: c.to_string(),
        });
    }
    if let Some((pocket, version)) = pick_pocket(uca, want) {
        return Ok(Plan::CloudArchive { pocket, version });
    }
    bail!(
        "the cluster's ovn-northd is OVN {}, and neither this distribution ({}) nor the Ubuntu Cloud Archive has an ovn-host that new: install OVN {} or newer here (ovn-host), then re-run the installer",
        series_str(want),
        distro.unwrap_or("no candidate"),
        series_str(want)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_read_as_series() {
        assert_eq!(series("26.03.0"), Some((26, 3)));
        assert_eq!(series("26.03.0-2~cloud0"), Some((26, 3)));
        assert_eq!(series("24.03.8-0ubuntu0.24.04.1"), Some((24, 3)));
        assert_eq!(series("ovn-controller 24.03.8"), Some((24, 3)));
        assert_eq!(northd_series("\"26.03.0-21.8.0-82.13\"\n"), Some((26, 3)));
        assert_eq!(series(""), None);
        assert_eq!(series_str((26, 3)), "26.03");
        assert_eq!(
            ovn_remote("\"ssl:172.19.36.18:6642\"\n").as_deref(),
            Some("ssl:172.19.36.18:6642")
        );
        assert_eq!(ovn_remote("\"\"\n"), None);
    }

    #[test]
    fn apt_and_archive_listings_parse() {
        let policy = "ovn-host:\n  Installed: 24.03.8-0ubuntu0.24.04.1\n  Candidate: 24.03.8-0ubuntu0.24.04.1\n  Version table:\n";
        assert_eq!(
            apt_candidate(policy).as_deref(),
            Some("24.03.8-0ubuntu0.24.04.1")
        );
        assert_eq!(
            apt_candidate("x:\n  Installed: (none)\n  Candidate: (none)\n"),
            None
        );
        let index = r#"<a href="?C=N;O=D">Name</a> <a href="/ubuntu/dists/">Parent</a> <a href="dalmatian/">dalmatian/</a> <a href="epoxy/">epoxy/</a> <a href="gazpacho/">gazpacho/</a>"#;
        assert_eq!(pockets(index), ["dalmatian", "epoxy", "gazpacho"]);
        let packages = "Package: ovn-common\nVersion: 26.03.0-2~cloud0\n\nPackage: ovn-host\nArchitecture: amd64\nVersion: 26.03.0-2~cloud0\nDepends: openvswitch-switch (>= 2.17.0~)\n";
        assert_eq!(
            ovn_host_version(packages).as_deref(),
            Some("26.03.0-2~cloud0")
        );
        assert_eq!(
            removals(
                "NOTE: simulation\nInst ovn-host [24.03] (26.03)\nRemv doca-openvswitch-switch [3.5]\nRemv doca-all [3.5]\n"
            ),
            ["doca-openvswitch-switch", "doca-all"]
        );
    }

    #[test]
    fn the_pocket_is_the_northds_release_else_the_next_newer() {
        let offers = vec![
            (
                "epoxy".to_string(),
                "25.03.1-0ubuntu0.25.04.1~cloud0".to_string(),
            ),
            ("flamingo".into(), "25.09.0-1~cloud0".into()),
            ("gazpacho".into(), "26.03.0-2~cloud0".into()),
        ];
        assert_eq!(pick_pocket(&offers, (26, 3)).unwrap().0, "gazpacho");
        assert_eq!(
            pick_pocket(&offers, (25, 9)).unwrap().0,
            "flamingo",
            "exactly northd's series"
        );
        assert_eq!(
            pick_pocket(&offers, (25, 6)).unwrap().0,
            "flamingo",
            "the oldest that is newer"
        );
        assert!(pick_pocket(&offers, (26, 9)).is_none());
    }

    #[test]
    fn the_plan_prefers_what_is_there_then_the_distribution() {
        let uca = vec![("gazpacho".to_string(), "26.03.0-2~cloud0".to_string())];
        assert_eq!(
            plan(Some((26, 3)), (26, 3), None, &uca).unwrap(),
            Plan::Matches
        );
        assert_eq!(
            plan(Some((26, 9)), (26, 3), None, &[]).unwrap(),
            Plan::Matches,
            "a newer controller is fine"
        );
        assert_eq!(
            plan(Some((24, 3)), (26, 3), Some("26.03.0-1"), &uca).unwrap(),
            Plan::Distro {
                candidate: "26.03.0-1".into()
            }
        );
        assert_eq!(
            plan(
                Some((24, 3)),
                (26, 3),
                Some("24.03.8-0ubuntu0.24.04.1"),
                &uca
            )
            .unwrap(),
            Plan::CloudArchive {
                pocket: "gazpacho".into(),
                version: "26.03.0-2~cloud0".into()
            }
        );
        let e = plan(None, (26, 3), Some("24.03.8"), &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("OVN 26.03") && e.contains("24.03.8"), "{e}");
    }

    #[test]
    fn only_ovn_comes_from_the_archive() {
        let pin = uca_pin("noble", "gazpacho");
        let ovn = pin.find("Package: ovn-*").unwrap();
        let all = pin.find("Package: *").unwrap();
        assert!(
            ovn < all,
            "apt takes the first pin that matches: OVN's first"
        );
        assert!(
            pin.contains("Pin: release n=noble-updates/gazpacho\nPin-Priority: -1"),
            "{pin}"
        );
        assert_eq!(
            uca_source("noble", "gazpacho"),
            format!("deb [signed-by={UCA_KEYRING}] {UCA} noble-updates/gazpacho main\n")
        );
    }
}

use super::*;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

/// A copy of sysfs's layout, as on one quarter of an HGX A100 board and a
/// second socket: a root port, a switch with a second switch below it, a
/// GPU and an InfiniBand NIC under the second, an NVMe drive under the
/// first; and on socket 1 a 100 GbE NIC in a bond on its own root port.
struct Sys {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Sys {
    fn new() -> Sys {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        Sys { _dir: dir, root }
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn link(&self, rel: &str, to: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        symlink(self.root.join(to), p).unwrap();
    }

    /// A PCI function at `path` (components under `devices/`).
    fn function(&self, path: &[&str], class: &str, numa: i32) {
        let rel = format!("devices/{}", path.join("/"));
        let addr = path.last().unwrap();
        self.write(&format!("{rel}/class"), class);
        self.write(&format!("{rel}/vendor"), "0x15b3");
        self.write(&format!("{rel}/device"), "0x101b");
        self.write(&format!("{rel}/numa_node"), &numa.to_string());
        self.link(&format!("bus/pci/devices/{addr}"), &rel);
    }

    fn hgx() -> Sys {
        let s = Sys::new();
        s.write("devices/system/node/online", "0-1\n");
        s.write("devices/system/node/node0/cpulist", "0-1\n");
        s.write("devices/system/node/node1/cpulist", "2-3\n");
        for (cpu, pkg) in [(0, 0), (1, 0), (2, 1), (3, 1)] {
            s.write(
                &format!("devices/system/cpu/cpu{cpu}/topology/physical_package_id"),
                &format!("{pkg}\n"),
            );
        }
        let rp = ["pci0000:00", "0000:00:01.1"];
        let up = [&rp[..], &["0000:01:00.0"]].concat();
        let down = [&up[..], &["0000:02:04.0"]].concat();
        let up2 = [&down[..], &["0000:0a:00.0"]].concat();
        let bridge = "0x060400";
        s.function(&rp, bridge, 0);
        s.function(&up, bridge, 0);
        s.function(&down, bridge, 0);
        s.function(&up2, bridge, 0);
        let d2a = [&up2[..], &["0000:0b:00.0"]].concat();
        let d2b = [&up2[..], &["0000:0b:10.0"]].concat();
        s.function(&d2a, bridge, 0);
        s.function(&d2b, bridge, 0);
        let nic = [&d2a[..], &["0000:0c:00.0"]].concat();
        s.function(&nic, "0x020700", 0);
        // The GPU sits under two more bridges of its own board.
        let g1 = [&d2b[..], &["0000:0d:00.0"]].concat();
        let g2 = [&g1[..], &["0000:0e:00.0"]].concat();
        let gpu = [&g2[..], &["0000:0f:00.0"]].concat();
        s.function(&g1, bridge, 0);
        s.function(&g2, bridge, 0);
        s.function(&gpu, "0x030200", 0);
        let d1 = [&up[..], &["0000:02:00.0"]].concat();
        let nvme = [&d1[..], &["0000:09:00.0"]].concat();
        s.function(&d1, bridge, 0);
        s.function(&nvme, "0x010802", 0);
        // Socket 1: the 100 GbE NIC on its own root port.
        let rp1 = ["pci0000:e0", "0000:e0:03.1"];
        let eth = [&rp1[..], &["0000:e1:00.0"]].concat();
        s.function(&rp1, bridge, 1);
        s.function(&eth, "0x020000", 1);
        let nic_rel = format!("devices/{}", nic.join("/"));
        let eth_rel = format!("devices/{}", eth.join("/"));
        // Netdevs: the IPoIB port, the bonded Ethernet port, the bond.
        s.link("class/net/ibp12s0/device", &nic_rel);
        s.write("class/net/ibp12s0/type", "32\n");
        s.write("class/net/ibp12s0/operstate", "up\n");
        s.write("class/net/ibp12s0/speed", "200000\n");
        s.write("class/net/ibp12s0/mtu", "2044\n");
        s.link("class/net/enp225s0f0np0/device", &eth_rel);
        s.link("class/net/enp225s0f0np0/master", "class/net/bond-apex");
        s.write("class/net/enp225s0f0np0/type", "1\n");
        s.write("class/net/enp225s0f0np0/operstate", "up\n");
        s.write("class/net/enp225s0f0np0/speed", "100000\n");
        s.write("class/net/enp225s0f0np0/mtu", "9000\n");
        s.write("class/net/bond-apex/type", "1\n");
        s.write("class/net/bmc0/type", "1\n");
        s.write("devices/usb/3-1.3:2.0/uevent", "");
        s.link("class/net/bmc0/device", "devices/usb/3-1.3:2.0");
        s.write("class/net/bmc0/speed", "-1\n");
        // RDMA devices.
        s.link("class/infiniband/mlx5_0/device", &nic_rel);
        let p = "class/infiniband/mlx5_0/ports/1";
        s.write(&format!("{p}/link_layer"), "InfiniBand\n");
        s.write(&format!("{p}/state"), "4: ACTIVE\n");
        s.write(&format!("{p}/rate"), "200 Gb/sec (4X HDR)\n");
        s.write(
            &format!("{p}/gids/0"),
            "fe80:0000:0000:0000:0c42:a103:0055:5c1e\n",
        );
        s.write(&format!("{p}/sm_lid"), "0x7\n");
        s.link("class/infiniband/mlx5_bond_0/device", &eth_rel);
        let p = "class/infiniband/mlx5_bond_0/ports/1";
        s.write(&format!("{p}/link_layer"), "Ethernet\n");
        s.write(&format!("{p}/state"), "4: ACTIVE\n");
        s.write(&format!("{p}/rate"), "100 Gb/sec (4X EDR)\n");
        s.write(&format!("{p}/gid_attrs/ndevs/0"), "bond-apex\n");
        s
    }
}

fn addr(name: &str, ip: &str, prefix: u8) -> (String, net::PortAddr) {
    (
        name.to_string(),
        net::PortAddr {
            addr: ip.parse().unwrap(),
            prefix,
        },
    )
}

#[test]
fn places_find_root_ports_and_nested_switches() {
    let s = Sys::hgx();
    let host = Host::read_in(&s.root, &[]);
    assert_eq!(host.pci.len(), 14);
    let nic = &host.places["0000:0c:00.0"];
    assert_eq!(nic.kind, pci::PciKind::Endpoint);
    assert_eq!(nic.root_port.as_deref(), Some("0000:00:01.1"));
    // The nearest switch above it is the board's second level.
    assert_eq!(nic.switch.as_deref(), Some("0000:0a:00.0"));
    assert_eq!(
        host.places["0000:01:00.0"].kind,
        pci::PciKind::SwitchUpstream
    );
    assert_eq!(
        host.places["0000:02:04.0"].kind,
        pci::PciKind::SwitchDownstream
    );
    assert_eq!(host.places["0000:e1:00.0"].switch, None);
}

#[test]
fn closeness_meets_at_any_switch_level() {
    let s = Sys::hgx();
    let host = Host::read_in(&s.root, &[]);
    // GPU and NIC: their nearest switches differ, but they meet below the
    // root port.
    assert_eq!(
        host.closeness("0000:0f:00.0", "0000:0c:00.0"),
        Closeness::Switch
    );
    // The NVMe drive shares only the top switch with the NIC.
    assert_eq!(
        host.closeness("0000:09:00.0", "0000:0c:00.0"),
        Closeness::Switch
    );
    // Across sockets.
    assert_eq!(
        host.closeness("0000:09:00.0", "0000:e1:00.0"),
        Closeness::Far
    );
    assert!(Closeness::Switch < Closeness::Numa && Closeness::Socket < Closeness::Far);
}

#[test]
fn closeness_by_numa_node_and_socket() {
    let s = Sys::new();
    s.write("devices/system/node/online", "0-2\n");
    for (n, cpu) in [(0, 0), (1, 1), (2, 2)] {
        s.write(
            &format!("devices/system/node/node{n}/cpulist"),
            &format!("{cpu}\n"),
        );
    }
    for (cpu, pkg) in [(0, 0), (1, 0), (2, 1)] {
        s.write(
            &format!("devices/system/cpu/cpu{cpu}/topology/physical_package_id"),
            &format!("{pkg}\n"),
        );
    }
    s.function(&["pci0000:00", "0000:00:01.0"], "0x010802", 0);
    s.function(&["pci0000:00", "0000:00:02.0"], "0x020000", 0);
    s.function(&["pci0000:40", "0000:40:01.0"], "0x020000", 1);
    s.function(&["pci0000:80", "0000:80:01.0"], "0x020000", 2);
    let host = Host::read_in(&s.root, &[]);
    assert_eq!(
        host.closeness("0000:00:01.0", "0000:00:02.0"),
        Closeness::Numa
    );
    assert_eq!(
        host.closeness("0000:00:01.0", "0000:40:01.0"),
        Closeness::Socket
    );
    assert_eq!(
        host.closeness("0000:00:01.0", "0000:80:01.0"),
        Closeness::Far
    );
}

#[test]
fn one_numa_node_is_all_local() {
    let s = Sys::new();
    s.write("devices/system/cpu/online", "0-3\n");
    s.function(&["pci0000:00", "0000:00:01.0"], "0x010802", -1);
    s.function(&["pci0000:00", "0000:00:02.0"], "0x020000", -1);
    let host = Host::read_in(&s.root, &[]);
    assert_eq!(host.numa.len(), 1);
    assert_eq!(host.numa[0].cpus, vec![0, 1, 2, 3]);
    assert_eq!(
        host.closeness("0000:00:01.0", "0000:00:02.0"),
        Closeness::Numa
    );
}

#[test]
fn ports_with_rdma_fabric_bond_and_addresses() {
    let s = Sys::hgx();
    let addrs = [
        addr("ibp12s0", "192.0.2.21", 24),
        addr("bond-apex", "198.51.100.21", 24),
        addr("lo", "127.0.0.1", 8),
    ];
    let host = Host::read_in(&s.root, &addrs);
    let names: Vec<&str> = host.ports.iter().map(|p| p.name.as_str()).collect();
    // Only netdevs on a PCI function: no bond, no BMC's USB device.
    assert_eq!(names, ["enp225s0f0np0", "ibp12s0"]);
    let ib = &host.ports[1];
    assert_eq!(ib.link_layer, net::LinkLayer::Infiniband);
    assert_eq!((ib.speed_mbps, ib.mtu), (Some(200_000), 2044));
    assert_eq!(ib.addresses, vec![addrs[0].1.clone()]);
    let r = ib.rdma.as_ref().unwrap();
    assert_eq!(
        (r.device.as_str(), r.port, r.rate_gbps),
        ("mlx5_0", 1, Some(200))
    );
    assert_eq!(r.state, "ACTIVE");
    assert_eq!(r.subnet_prefix.as_deref(), Some("fe80:0000:0000:0000"));
    assert_eq!(r.sm_lid, Some(7));
    // A bond's member: its master, the bond's address and RDMA device.
    let eth = &host.ports[0];
    assert_eq!(eth.master.as_deref(), Some("bond-apex"));
    assert_eq!(eth.addresses, vec![addrs[1].1.clone()]);
    let r = eth.rdma.as_ref().unwrap();
    assert_eq!(
        (r.device.as_str(), r.link_layer),
        ("mlx5_bond_0", net::LinkLayer::Ethernet)
    );
    assert_eq!(r.subnet_prefix, None);
}

#[test]
fn reads_this_host() {
    // Whatever the host has, reading it doesn't fail, and every port's PCI
    // function is in the tree.
    let host = Host::read();
    assert!(!host.numa.is_empty());
    for p in &host.ports {
        assert!(host.function(&p.pci).is_some(), "{} on {}", p.name, p.pci);
    }
}

#[test]
fn cpu_lists_parse() {
    assert_eq!(
        numa::parse_list("0-3,8,10-11\n"),
        vec![0, 1, 2, 3, 8, 10, 11]
    );
    assert_eq!(numa::parse_list(""), Vec::<u32>::new());
}

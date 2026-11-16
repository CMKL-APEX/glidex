//! [Glidex](https://cmkl-apex.github.io/glidex/docs/) is a control plane for
//! KVM virtual machines (Cloud Hypervisor and QEMU) on one host or a cluster.
//!
//! This crate gathers its libraries under one name, each behind a feature of
//! the same name (`api` by default, `full` for all). The server, its helpers
//! and the web UI aren't libraries: they are installed with
//! `glidex-install` (see the [operator guide]).
//!
//! | Module | Feature | Crate |
//! |---|---|---|
//! | `api` | `api`, `client` (generated clients) | `glidex-api`: the RPC API's messages, services and clients |
//! | `auth` | `auth` | `glidex-auth`: secrets, session and token rules, a Cedar engine, OIDC login |
//! | `hv_client` | `hv-client` | `glidex-hv-client`: Cloud Hypervisor's HTTP API and QEMU's QMP |
//! | `ovn` | `ovn` | `glidex-ovn`: the OVN northbound database as glidex uses it |
//! | `ovs` | `ovs` | `glidex-ovs`: host networking with Open vSwitch, NAT and VM ports |
//! | `raftstore` | `raftstore` | `glidex-raftstore`: a replicated ReDB store on openraft |
//! | `tls` | `tls` | `glidex-tls`: HTTPS listeners, certificates and client trust |
//! | `topology` | `topology` | `glidex-topology`: a host's NUMA, PCIe and network ports from sysfs |
//! | `trust` | `trust` | `glidex-trust`: a cluster's side of trust with another organization's directory |
//!
//! [operator guide]: https://cmkl-apex.github.io/glidex/docs/

#[cfg(feature = "api")]
pub use glidex_api as api;
#[cfg(feature = "auth")]
pub use glidex_auth as auth;
#[cfg(feature = "hv-client")]
pub use glidex_hv_client as hv_client;
#[cfg(feature = "ovn")]
pub use glidex_ovn as ovn;
#[cfg(feature = "ovs")]
pub use glidex_ovs as ovs;
#[cfg(feature = "raftstore")]
pub use glidex_raftstore as raftstore;
#[cfg(feature = "tls")]
pub use glidex_tls as tls;
#[cfg(feature = "topology")]
pub use glidex_topology as topology;
#[cfg(feature = "trust")]
pub use glidex_trust as trust;

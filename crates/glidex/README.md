# glidex

[Glidex](https://github.com/CMKL-APEX/glidex) is a control plane for KVM
virtual machines (Cloud Hypervisor and QEMU) on one host or a cluster, with
its own directory, tenancy, Cedar authorization, networking (OVS/OVN),
storage and a web UI.

This crate gathers Glidex's libraries under one name, each behind a feature:

| Feature | Module | What it is |
|---|---|---|
| `api` (default) | `glidex::api` | The RPC API's messages and services |
| `client` | `glidex::api` | The RPC API's generated clients |
| `auth` | `glidex::auth` | Secrets, session and token rules, a Cedar engine, OIDC login |
| `hv-client` | `glidex::hv_client` | Cloud Hypervisor's HTTP API and QEMU's QMP |
| `ovn` | `glidex::ovn` | The OVN northbound database as Glidex uses it |
| `ovs` | `glidex::ovs` | Host networking: Open vSwitch, NAT, VM ports |
| `raftstore` | `glidex::raftstore` | A replicated ReDB store on openraft |
| `tls` | `glidex::tls` | HTTPS listeners, certificates and client trust |
| `topology` | `glidex::topology` | A host's NUMA, PCIe and network ports from sysfs |
| `full` | | All of the above |

```toml
[dependencies]
glidex = { version = "0.0.8", features = ["client"] }
```

To run Glidex itself (the server, its helpers and the web UI), install it
with `glidex-install`: see the
[documentation](https://cmkl-apex.github.io/glidex/docs/).

Licensed under either of Apache License 2.0 or MIT, at your option.

# gxctl

The command-line client for [Glidex](https://github.com/CMKL-APEX/glidex),
a control plane for KVM virtual machines (Cloud Hypervisor and QEMU) on
one host or a cluster.

```bash
cargo install gxctl
gxctl auth login --url https://host.example.org:8841 --profile lab --oidc
gxctl list
```

Run with no arguments for an interactive shell; `help` lists the
commands. On a Glidex host, `gxctl` talks to the local control plane over
its Unix socket with no login.

See the [user guide](https://cmkl-apex.github.io/glidex/docs/user-guide/)
and, for logging in from another machine,
[Logging in](https://cmkl-apex.github.io/glidex/docs/user-guide/logging-in.html).

Licensed under either of Apache License 2.0 or MIT, at your option.

# Spike: the metadata endpoint delivered by tc/eBPF (dev only)

A prototype for spec/workload-identity.md WI1b-2 (§7.1, OVN and isolated
networks), kept for reference; nothing in glidex loads it.

- `md.c`: `md_in` (tc ingress on a VM's port) rewrites a request for
  `169.254.169.254:80` to come from the port's peer address
  `169.254.192.<index>` and go to metad's port, remembers the VM's address
  and MAC, and redirects it into the host stack through a dummy device;
  `md_out` (tc egress on that device) rewrites the reply back and
  redirects it out of the VM's port. The index comes from the port the
  request arrived on, never from the guest's addresses.
- `spike.sh` (root): a scratch OVS bridge with **no host address or
  route** (like OVN or isolated networks), two network namespaces as
  VMs, a listener as `nobody` on `169.254.169.254:18775`; everything
  removed afterwards.

```bash
clang -O2 -g -target bpf -DLISTEN_PORT=18775 -I/usr/include/x86_64-linux-gnu -c md.c -o md.o
sudo ./spike.sh
```

Result on Linux 7.0, OVS 3.7, iproute2 6.19 (2026-10-23):

```
VM1: peer=169.254.192.1
VM2: peer=169.254.192.2
VM2 as .11: peer=169.254.192.2      # spoofing VM1's address: its own port decides
VM2 unindexed: ''                   # a port without an index: dropped
```

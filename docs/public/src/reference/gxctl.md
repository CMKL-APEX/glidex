# gxctl

`gxctl` is the Glidex command-line client. It runs as an interactive
shell, or runs one command and exits:

```bash
gxctl                                   # interactive shell: gxctl> …
gxctl list                              # one command, then exit
gxctl --project lab disk list
gxctl --url https://host.example.org:8841 auth login --oidc
```

In the shell, `help` (or `?`) lists the commands, Tab completes command
names, subcommands and file paths, and `exit` (`quit`, `q`) leaves.
Words can be quoted: `project create lab --description "Lab VMs"`.
Noun commands also answer to their plural (`images`, `projects`,
`tenants`, …), and many subcommands to short forms (`ls`, `rm`).

`gxctl --help` shows the global options. Commands have no `--help` of
their own: run one without its arguments to see its usage line, or
`gxctl help` for the full list. Both are authoritative over this page.

## Global options

| Option | Environment | Effect |
|---|---|---|
| `--socket <path>` | `GLIDEX_SOCKET` | Talk to this Unix socket. Default: the first that exists of `/run/glidex-cp/api.sock`, `$XDG_RUNTIME_DIR/glidex/api.sock`, `/tmp/glidex-<uid>/api.sock`; with none, `https://localhost:8841` |
| `-s`, `--url <url>` (alias `--server`) | | Talk to a control plane over HTTPS instead, e.g. `https://host.example.org:8841`. Wins over `--socket`. Plain `http://` only to loopback |
| `--profile <name>` | `GLIDEX_PROFILE` | Use this login profile instead of the current one |
| `-p`, `--project <id\|name>` | `GLIDEX_PROJECT` | Project for creates, lists and name lookups; default: your default project (`project use`) |

Other environment variables:

| Variable | Effect |
|---|---|
| `GLIDEX_TOKEN` | API token to send over HTTPS; wins over the saved one |
| `GLIDEX_CA_CERT` | A PEM file to trust, e.g. a copy of the host's `/run/glidex-cp/tls.crt` |
| `GLIDEX_CONFIG` | Path of the profile file (default `~/.config/glidex/config.json`); empty means no profile file, which is what CI should use |
| `GLIDEX_TLS_INSECURE=1` | With a profile that has no pinned fingerprint or CA: skip certificate checks for this run, with a warning. For throwaway test setups only: anyone on the network path can read your token |
| `GLIDEX_UI_URL` | What `ui` prints (default `https://localhost:5173`) |

## Connecting and logging in

- **On the host**, `gxctl` uses the Unix socket and you are your Unix
  user: no login. You need to be in `glidex-users` (or `glidex-admin`
  for administration).
- **From another machine**, log in once with `auth login`, which saves a
  *profile* (target URLs, TLS trust and token) in
  `~/.config/glidex/config.json` (mode `0600`). Later runs use the
  current profile. The older `login` command saves just a token in
  `~/.config/glidex/token`.
- A self-signed server certificate is either trusted through
  `GLIDEX_CA_CERT`, or pinned by fingerprint at `auth login` (compare it
  with the one the server printed at start).

See [Logging in](../user-guide/logging-in.md).

| Command | Effect |
|---|---|
| `auth login [--url U]… [--profile P] [--oidc \| --token \| --pam-user U] [--connector ID]` | Log in and save a profile. `--oidc`: device flow in a browser; `--token`: paste a token (read without echo); `--pam-user`: a password login. Also `--pin sha256/…` (expected fingerprint), `--insecure`, `--rebind`, `--use` (make it current), `--name N` / `--days N` (the token's name and lifetime), `--save` (record a new target as a profile named after its host), `--non-interactive` |
| `auth logout [--profile P] [--revoke] [--keep-token]` | Drop a profile's credential; `--revoke` also revokes the token on the server |
| `auth status [--profile P]` | Target, cluster, trust mode, token and expiry |
| `auth use <profile\|->` | Set the current profile (`-` clears it) |
| `auth profiles [--all]` | List profiles |
| `auth trust list \| fetch \| remove <pin> \| remove --all [--yes]` | A profile's pinned certificate fingerprints; `fetch` shows what the server claims, sending nothing else |
| `auth token …`, `auth whoami` | Same as `token …`, `whoami` |
| `login --oidc [--connector ID] \| --token` | Older form: log in over HTTPS and save the token file. Not needed on the socket |
| `logout [--revoke]` | Delete the token file; `--revoke` revokes the token first |
| `whoami` | Who you are: login method, teams, roles, default project |
| `ui` | Print the web UI's address |
| `health` | Check that the API answers |

## VMs

`<vm>` is a VM name or id.

| Command | Effect |
|---|---|
| `list`, `ls` | List VMs; `STATE` shows `state → desired` while they differ |
| `get <vm>` | VM details, and why it isn't ready yet if it isn't |
| `create` | Create a VM, asking for name, size, hypervisor, firmware, image or disk, credential and networks |
| `start <vm> [--no-wait]` | Start or resume; waits up to 60 s for the VM to run |
| `stop <vm> [--graceful [secs]] [--no-wait]` | Stop. `--graceful` presses the power button and stops it hard after `secs` (default 60) |
| `pause <vm> [--no-wait]` | Pause |
| `delete <vm> [--keep-disk] [--no-wait]`, `rm` | Delete after confirmation, with its own root disk unless `--keep-disk` |
| `events <vm>` | What happened to the VM: starts, exits, restarts |
| `connect <vm>`, `console`, `attach` | Serial console; `Ctrl+]` detaches |
| `log <vm>`, `logs` | The console log (last 1 MiB) |
| `attach-device <vm> <path>` | Attach a PCI device (`/sys/bus/pci/devices/<address>` or the bare address) |
| `detach-device <vm> <path>` | Detach a PCI device |
| `watch [kinds]` | Follow changes until Ctrl-C; kinds are `vms,disks,images,networks` (the default), also `nodes`, `imports`, `cluster`, `policy` |

`start`, `stop`, `pause` and `delete` wait for the controller and print
where the VM ended up; `--no-wait` returns at once. See
[Virtual machines](../user-guide/vms.md).

## Guest credentials

| Command | Effect |
|---|---|
| `credentials`, `creds` | List guest logins (no hashes) |
| `credential-add`, `cred-add` | Add one: username, password (asked twice, hidden) and/or SSH public key files |
| `credential-passwd <user>`, `cred-passwd` | Change the password |
| `credential-keys <user>`, `cred-keys` | Replace the SSH keys (empty: remove all) |
| `credential-rm <user>`, `cred-rm` | Delete |

Private key files are refused before anything is sent. See
[Guest credentials](../user-guide/credentials.md).

## Images and disks

| Command | Effect |
|---|---|
| `image catalog` | Built-in cloud images and UEFI firmware |
| `image list`, `images` | Downloaded images and their state |
| `image pull <catalog-key\|https-url> [--name N] [--sha256 H]` | Download and verify an image, showing progress |
| `image pull --firmware <key> [--name N]` | Download UEFI firmware from the catalog (`cloudhv-edk2`, `ovmf`) |
| `image pull <url> --firmware-for <cloudhypervisor\|qemu> [--vars-url U] [--vars-sha256 H]` | Download firmware from a URL (QEMU: with its variable store) |
| `image retry <image>` | Download a failed image again |
| `image rm <image>` | Delete an image |
| `disk list`, `disks` | List disks |
| `disk show <disk>` | Details and partitions |
| `disk create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend]` | Create a blank disk or one from an image. `--full`: a full copy instead of a linked clone; `--raw`: raw format; `--no-extend`: don't grow the root partition |
| `disk resize <disk> <GiB> [--no-extend]` | Grow or shrink |
| `disk extend-root <disk> [--on-boot]` | Grow the root partition now (offline) or at next boot |
| `disk rm <disk>` | Delete after confirmation |

See [Images and disks](../user-guide/images-and-disks.md).

## Networks

| Command | Effect |
|---|---|
| `networks`, `nets`, `network list` | List networks |
| `network show <net>` | A network's id, mode, grants or shares |
| `network-add <name> [--nat [--subnet CIDR] \| --isolated \| --bridged <bridge>] [--vhost-user] [--vlan N] [--bridge NAME] [--cluster \| --node]`, `net-add` | Create a host or cluster network (default NAT). `--cluster` / `--node`: on OVN across the cluster, or on one node's bridge |
| `network create <name> --project P [--isolated] [--vhost-user] [--subnet CIDR] [--mtu N]` | Create a project network (NAT unless `--isolated`). Without `--project`, same as `network-add` |
| `network rm <net>`, `network-rm`, `net-rm` | Delete, waiting up to a minute |
| `network grant <host-net> [--project P]… [--tenant T]… [--all-projects]` | Who may use a host network (replaces the current grants) |
| `network share <net> <project-id>` | Offer a project network to another project |
| `network unshare <net> <project-id>` | Withdraw the offer |
| `network shares <project>` | Shares offered to or accepted by a project |
| `network accept <project> <net>` | Accept a share |
| `network leave <project> <net>` | Leave a share |

Routers have no `gxctl` command yet: use the web UI or the
[REST API](../api/rest.md#networks). See [Networks](../user-guide/networks.md).

## Host networking and PCI

| Command | Effect |
|---|---|
| `ovs status`, `ovs` | Open vSwitch and networking helper status |
| `ovs install [--profile kernel\|dpdk] [--source] [--force]` | Install Open vSwitch (`--source`: build it from source) |
| `ovs dpdk-init [--socket-mem M] [--pmd-cpu-mask MASK] [--force]` | Initialize DPDK (`--socket-mem` default `1024`) |
| `bridges` | List glidex bridges |
| `uplinks <bridge>` | List a bridge's uplinks |
| `uplink-add <bridge> --kernel <if> [--migrate-ip] [--force] [--accept-dhcp]` | Add a NIC as an uplink. `--migrate-ip` moves the NIC's address to the bridge, rolled back unless confirmed in time |
| `uplink-add <bridge> --afxdp <if> [--xdp-mode best_effort\|native\|native_with_zerocopy\|generic] [--rxq N]` | Add an AF_XDP uplink |
| `uplink-add <bridge> --dpdk <pci-address> --name <port> [--rxq N]` | Add a DPDK uplink |
| `uplink-reapply <bridge> <name>` | Set up again an uplink rolled back at start |
| `uplink-rm <bridge> <name>` | Remove an uplink, restoring the NIC |
| `pci`, `pci-devices` | List the host's PCI devices, with their NUMA node and whether each is ready for passthrough |
| `pci pools [--project P]` | The device pools: their devices per node, holders, and how many are free; with `--project`, the pools that project may use and how many are free |
| `pci <address>` | One device in detail |

See [Host networking](../operator-guide/host-networking.md) and
[PCI passthrough](../operator-guide/pci-passthrough.md).

## Usage

| Command | Effect |
|---|---|
| `usage [--project P] [--from D] [--to D] [--by project,vm,disk,nic,network] [--granularity hour\|day\|month] [--meters m,…] [--tz Z] [--csv]` | Resource usage; default: this billing month by project |
| `usage bandwidth\|disk-io\|compute [--month YYYY-MM] [--by …] [--project P] [--csv]` | Average, 30-second peak and 95th percentile per billing month |
| `stats <vm>` | A VM's current CPU, memory, NIC and disk rates |
| `bandwidth <vm> \| --network N [--from D] [--to D]` | 5-minute bandwidth series with its 95th percentile |
| `io <vm> \| --disk D [--from D] [--to D]` | 5-minute disk I/O series |
| `compute <vm> [--from D] [--to D]` | 5-minute CPU and memory series |

Times (`--from`, `--to`) are Unix seconds or RFC 3339. See
[Usage](../user-guide/usage.md).

## Projects and access

| Command | Effect |
|---|---|
| `project list`, `projects` | Projects you can see |
| `project show <p>` | Quotas and usage |
| `project create <name> [--description TEXT]` | Create a project |
| `project delete <p>` | Delete after confirmation |
| `project quota <p> [key=value…]` | Show, or set limits: `vms`, `vcpus`, `memory_mib`, `disk_gib`, `running_vms`, `networks`, `devices.<pool>`; `none` means unlimited |
| `project use <p>` | Set your default project (`-` or `none` clears it) |
| `binding list <p>` | A project's role bindings |
| `binding add <p> <role> user:<id>\|team:<id>\|token:<id>` | Grant a role on a project |
| `binding remove <p> <binding-id>` | Remove a binding |
| `system-binding list [--node N] \| add <role> <principal> [--node N] \| remove <binding-id>` | System role bindings, cluster-wide or on one node's host |
| `token list`, `tokens` | Your API tokens |
| `token create <name> [--days N] [--service-account [--project P]] [--role ROLE[@PROJECT]]…` | Create a token; the secret is printed once |
| `token revoke <id>` | Revoke a token |
| `user list`, `users` | Users and their identities |
| `team list \| create <name> \| delete <team>` | Teams |
| `team add-member <team> <user-id>`, `remove-member <team> <user-id>` | Team membership |
| `policy list \| show <id>` | Site Cedar policies |
| `policy put <id> <file> [--disable] [--description TEXT]` | Create or replace a policy |
| `policy validate <id> <file>` | Check a policy without saving it |
| `policy history <id>` | A policy's versions |
| `policy delete <id>` | Delete after confirmation |
| `policy reload` | Reload policies |
| `audit [--project P] [--since <unix-ms>] [--limit N] [--user U]` | The audit log |

Roles are written `owner` or `role.owner`; principals are `user:<id>`,
`team:<id>` or `token:<id>`. See [Projects](../user-guide/projects.md)
and [Access control](../operator-guide/access-control.md).

## Tenants and login connectors

| Command | Effect |
|---|---|
| `tenant list \| show <t>` | Tenants |
| `tenant create <slug> [--name TEXT] [--operator]` | Create a tenant |
| `tenant rename <t> <name>` | Change its display name |
| `tenant suspend <t>`, `resume <t>`, `delete <t>` | Suspend, resume or delete |
| `tenant members <t>`, `add-member <t> <principal>`, `remove-member <t> <principal>` | Explicit members (`user:<id>`, `team:<id>`) |
| `tenant bindings <t>`, `bind <t> <tenant-admin\|tenant-auditor> <principal>`, `unbind <t> <link>` | Tenant roles |
| `tenant device-pools <t>` | The tenant's pools on this cluster: cluster pool, held/max, free, projects |
| `tenant device-pool set <t> <name> --from <pool> --max N [--projects p1,p2]`, `device-pool remove <t> <name>` | Add or change a tenant pool, or remove one |
| `admission list` | Tenants admitted on this site, with their caps |
| `admission set <tenant> [key=value… \| unlimited] [--suspend \| --resume]` | Admit a tenant and set its caps: `vms`, `vcpus`, `memory_mib`, `disk_gib`, `running_vms`, `networks`, `routers`, `external_ips` |
| `admission remove <tenant>` | Remove an admission |
| `connector list \| show <id>` | Login connectors (secrets show only whether they are set) |
| `connector add oidc <id> --issuer URL --client-id ID [--name TEXT] [--scope S]… [--groups-claim C] [--domain D]… [--mfa-acr V]… [common]` | Add an OIDC connector |
| `connector add pam <id> [--name TEXT] [--allow-group G]… [common]` | Add a PAM connector |
| `connector update <id> [--name TEXT] [--enable \| --disable] [common]` | Change a connector |
| `connector secret set <id> [field] [--from-file PATH]` | Set a secret (default `client_secret`), from a hidden prompt, stdin or a file |
| `connector test <id>` | Test it step by step |
| `connector remove <id>` | Delete it |

Common connector flags: `--jit` / `--no-jit`, `--require-group G`…,
`--mfa` / `--no-mfa`, `--team GROUP=TEAM`…, `--default-tenant T`,
`--token-max-days N`. See [Authentication](../operator-guide/authentication.md).

## Cluster and nodes

| Command | Effect |
|---|---|
| `cluster status`, `cluster` | Raft group, nodes, versions and port reachability; says so on a standalone host |
| `cluster init [--advertise <ip:port>] [--tunnel-ip <ip>] --force` | Make this host a cluster of one (keeps a backup; can't be undone) |
| `cluster join-token [--role agent\|server] [--ttl <hours>h] [--allow-import]` | Create a one-time join token, printed once (default role `agent`) |
| `cluster join --server <ip:8842> [--role server\|agent] [--advertise <ip:port>] [--tunnel-ip <ip>] [--name <name>] [--token-file <file>] [--import]` | Join this host to a cluster. The token is read from the file (mode `0600`) or stdin, never from the command line. `--import` brings this host's VMs and disks once an administrator approves |
| `cluster rejoin --server <ip:8842> [--node-id <id>] [--advertise <ip:port>] [--tunnel-ip <ip>] [--token-file <file>]` | Come back as the same node, with a token from `node rejoin-token` |
| `cluster promote <node>… [--force]` | Promote agents to servers |
| `cluster leave` | Go back to standalone after this node was removed |
| `cluster dissolve --force` | The last node ends the cluster, keeping everything |
| `cluster rotate-ca [--grace <secs>]` | Rotate the cluster CA |
| `cluster rotate-secrets-key` | Rotate the key that protects connector secrets |
| `cluster snapshot <file>` | Save a snapshot of the replicated store (written `0600`; store it like the database) |
| `nodes`, `node` | List nodes |
| `node <id>` | One node in detail |
| `node drain <node>`, `node undrain <node>` | Stop or resume placing VMs on a node |
| `node prepare-device <node> <address>`, `node release-device <node> <address>` | Bind a device's IOMMU group to vfio-pci on its node, or put it back on its drivers |
| `node sriov <node> <pf-address> <vfs>` | Give a physical function that many SR-IOV virtual functions (0 removes them) |
| `node cordon-device <node> <address>`, `node uncordon-device <node> <address>` | Stop or resume giving a device to new VMs |
| `node topology <node> [--devices]` | A node's NUMA nodes, CPUs, memory, hugepages and the devices a VM could be given, read on that node; `--devices`: the devices with their place in the PCIe tree |
| `node remove <node> [--force]` | Remove an empty node |
| `node forget <node> [--fenced] [--force]` | Forget a node that is gone for good |
| `node purge <node>` | Drop a forgotten node's record |
| `node rejoin-token <node> [--raft-lost]` | Token for the node to rejoin as itself, printed once |
| `node detach <node> [--map-network <cluster-net>=<node-net>]… [--with-access] [--timeout <secs>]` | Detach a node with its resources, back to standalone. `--abort` cancels; `--departed` records a node that already left on its own |
| `node import list \| show <plan> \| reject <plan>` | Import plans |
| `node import approve <plan> [--project src=existing\|src=new:name]… [--network a=b]… [--credential proj/user=new]… [--over-quota] [--dry-run]` | Approve an import with its mappings |

<!-- TODO(verify): the clustering design describes `node forget <node> --departed`; in the code only `node detach --departed` sends a flag the server reads (`/nodes/{id}/forget` ignores `departed`). -->

See [Clusters](../operator-guide/clusters.md).

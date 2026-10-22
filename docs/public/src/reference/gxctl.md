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
- **On a cluster agent**, which has no API of its own, other commands
  fail with *this host is a cluster agent: it has no local API*; `auth
  login`, `auth use`, `auth profiles` and `auth trust` still run. Log in
  to a server: `gxctl auth login --url https://<server>:8841`.

See [Logging in](../user-guide/logging-in.md).

| Command | Effect |
|---|---|
| `auth login [--url U]… [--profile P] [--oidc \| --token \| --pam-user U] [--connector ID]` | Log in and save a profile. `--server U` is the same as `--url U`. `--oidc`: device flow in a browser; `--token`: paste a token (read without echo); `--pam-user`: a password login. Also `--pin sha256/…` (expected fingerprint), `--insecure`, `--rebind`, `--use` (make it current), `--name N` / `--days N` (the token's name and lifetime), `--save` (record a new target as a profile named after its host: a host name's first label, or a whole IP address with dashes: `192.0.2.10` gives `192-0-2-10`, `[fd00::1]` gives `fd00-1`), `--non-interactive` |
| `auth logout [--profile P] [--revoke] [--keep-token]` | Drop a profile's credential; `--revoke` also revokes the token on the server |
| `auth status [--profile P]` | Target, cluster, trust mode, token and expiry |
| `auth use <profile\|->` | Set the current profile (`-` clears it) |
| `auth profiles [--all]` | List profiles |
| `auth profiles remove <name> [--forget-token]` | Delete a profile from the config file. One that still holds a token is refused: revoke it first with `auth logout --profile <name> --revoke`, or `--forget-token` drops it unrevoked. Removing the current profile leaves none current: while other profiles remain, commands fail with *no current profile; choose one with --profile*; pick one with `gxctl --profile <other> auth use <other>` |
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
| `get <vm>` | VM details, and why it isn't ready yet if it isn't; a `Pool:` line per device pool request with the devices it got; for a GPU request, `GPUs: <count> from <pool>` (with ` (NVLink)` when asked), and for a VM aligned over several NUMA nodes a `NUMA <i>:` line per guest NUMA node: its host node, vCPUs, memory and devices |
| `create` | Create a VM, asking for name, GPUs (`count[:pool][:nvlink]`, such as `4:a100`; optional: with GPUs, an empty vCPU or memory answer takes the pool's per-device sizing), size, hypervisor, firmware, image or disk, credential, networks, PCI devices and device pools (`a100:2`, or `a100:2:switch` for one PCIe switch). `none` at the firmware prompt is kernel boot from host paths: it asks for the kernel image path, the root filesystem path and the kernel arguments (default `console=ttyS0`, `ttyAMA0` on aarch64, `root=/dev/vda reboot=k panic=1`) |
| `start <vm> [--no-wait]` | Start or resume; waits up to 60 s for the VM to run |
| `stop <vm> [--graceful [secs]] [--no-wait]` | Stop. `--graceful` presses the power button and stops it hard after `secs` (default 60) |
| `pause <vm> [--no-wait]` | Pause |
| `kernel <vm> [--set <kernel image>] [--follow on\|off] [--extract]` | Show a kernel-boot VM's kernel (and the booted and previous ones when following the guest); change it from the next start; take the kernel on its disk |
| `delete <vm> [--keep-disk] [--no-wait]`, `rm` | Delete after confirmation, with its own root disk unless `--keep-disk` |
| `place <vm> [--node N]` | Place a stopped or lost VM on another node, or where there is room (its disks on shared datastores; `system-admin`). Prints the node, or *Not placed yet* with the reason it waits |
| `events <vm>` | What happened to the VM: starts, exits, restarts |
| `connect <vm>`, `console`, `attach` | Serial console; `Ctrl+]` detaches |
| `log <vm>`, `logs` | The console log (last 1 MiB) |
| `attach-device <vm> <path>` | Attach a PCI device (`/sys/bus/pci/devices/<address>` or the bare address) |
| `detach-device <vm> <path>` | Detach a PCI device |
| `watch [kinds]` | Follow changes until Ctrl-C; kinds are `vms,disks,images,networks` (the default), also `nodes`, `imports`, `cluster`, `policy`, `datastores` |

`start`, `stop`, `pause` and `delete` wait for the controller and print
where the VM ended up; `--no-wait` returns at once. See
[Virtual machines](../user-guide/vms.md); for `place`,
[Placing a stopped or lost VM again](../operator-guide/datastores.md#placing-a-stopped-or-lost-vm-again).

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
| `image list`, `images` | Images and their state. Kernel images show as `kernel <release>`, in state `extracting` until ready; the source of an extracted one is `extracted: <partition>:<path>` |
| `image pull <catalog-key\|https-url> [--name N] [--sha256 H]` | Download and verify an image, showing progress. A catalog pull also extracts the image's kernel as `<name>-kernel` where the host can |
| `image pull --firmware <key> [--name N]` | Download UEFI firmware from the catalog (`cloudhv-edk2`, `ovmf`) |
| `image pull <url> --firmware-for <cloudhypervisor\|qemu> [--vars-url U] [--vars-sha256 H]` | Download firmware from a URL (QEMU: with its variable store) |
| `image pull <key> --no-kernel` | Pull a catalog image without taking its kernel out |
| `image pull <url> --kernel [--initrd-url U] [--initrd-sha256 H] [--cmdline C]` | Download a kernel image |
| `image pull --kernel-path P [--initrd-path P] [--cmdline C]` | Copy a kernel image from host files (administrators) |
| `image extract-kernel <image>` | Take the kernel out of a disk image |
| `image retry <image>` | Download a failed image again, or run a failed kernel extraction again |
| `image rm <image>` | Delete an image; refused (`409`) while a disk uses it, or a VM boots or keeps a kernel image |
| `disk list`, `disks` | List disks |
| `disk show <disk>` | Details, datastore and partitions |
| `disk create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend] [--datastore D]` | Create a blank disk or one from an image. `--full`: a full copy instead of a linked clone; `--raw`: raw format (on an LVM-thin datastore disks are always raw, linked ones too); `--no-extend`: don't grow the root partition; `--datastore`: where it lives (default: the default datastore) |
| `disk resize <disk> <GiB> [--no-extend]` | Grow or shrink |
| `disk extend-root <disk> [--on-boot]` | Grow the root partition now (offline) or at next boot |
| `disk move <disk> --datastore D [--node N] [--flatten]` | Copy a disk no running VM uses to another datastore, waiting up to 5 minutes. `--node`: for a node-local destination, the node that has the disk; `--flatten`: a linked disk becomes a full copy |
| `disk rm <disk>` | Delete after confirmation |
| `datastore list`, `datastores`, `datastore ls` | Datastores, how many nodes reach each, and free space: `NAME`, `DEFAULT` (`*`), `NODES OK` (such as `2/3`), `FREE` (the most on one node), `DISKS`, `PATH` (empty for LVM-thin) |
| `datastore show <datastore>`, `get` | Id, paths (for LVM-thin, the volume group and thin pool, and each node's own volume group), nodes, overcommit, disks, and each node's view: free space, why it can't reach it, mount or thin pool warnings and, for LVM-thin, the image volumes it keeps (`image volumes:`, by image id) |
| `datastore create <name> (--path P [--shared] \| --lvm VG/POOL) [--node-path NODE=P]… [--nodes N,M] [--default] [--no-overcommit]` | Register a datastore (`system-admin`): a path under `/srv/glidex`, or with `--lvm` LVM-thin in volume group VG's thin pool POOL on each node. `--shared`: one filesystem every node mounts; `--nodes`: only these nodes; `--node-path`: a node's own path (for `--lvm`, its own volume group); `--default`: the default for new disks |
| `datastore set <datastore> [--node-path NODE=P]… [--nodes N,M\|--all-nodes] [--default\|--no-default] [--overcommit\|--no-overcommit]` | Change the nodes, the nodes' own paths, the default or overcommit (`system-admin`) |
| `datastore rm <datastore>`, `delete` | Delete one no disk is on and, for LVM-thin, where no node keeps image volumes; without confirmation; its files stay (`system-admin`) |

A datastore is named by its name or id. See
[Images and disks](../user-guide/images-and-disks.md),
[Kernel boot](../user-guide/kernel-boot.md) and
[Datastores](../operator-guide/datastores.md).

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
| `system-binding list [--node N] \| add <role> <principal> [--node N] \| remove <binding-id>` | System role bindings, cluster-wide or on one node's host (`auditor`, `net-admin`, `grant.host-paths` only); per-host ones list as `on host:<id>` |
| `token list`, `tokens` | Your API tokens |
| `token create <name> [--days N] [--service-account [--project P]] [--role ROLE[@PROJECT]]…` | Create a token; the secret is printed once |
| `token revoke <id>` | Revoke a token |
| `user list`, `users` | Users and their identities. Deleting a user is in the web UI or the API only |
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
| `connector list \| show <id>` | Login connectors (secrets show only whether they are set). `show` of a pulled connector adds its groups, administrator and interval (`Pull:`), and the last run: its result, changes, error, and the users a blocked run would disable |
| `connector add oidc <id> --issuer URL --client-id ID [--name TEXT] [--scope S]… [--groups-claim C] [--domain D]… [--mfa-acr V]… [common]` | Add an OIDC connector |
| `connector add pam <id> [--name TEXT] [--allow-group G]… [common]` | Add a PAM connector |
| `connector add google <id> --client-id ID [--name TEXT] (--domain D… [--trust-2sv] \| --consumer [--allow-email E]…) [--device-client-id ID] [common]` | Add a Google connector: Workspace accounts of the `--domain` domains, or (`--consumer`) any Google account of people invited or listed. `--device-client-id`: the *TVs and Limited Input devices* client for `auth login --oidc` |
| `connector add jumpcloud <id> --client-id ID [--name TEXT] [--groups-claim C] [--allow-email E]… [common]` | Add a JumpCloud connector; `--groups-claim` defaults to `memberOf` |
| `connector update <id> [--name TEXT] [--enable \| --disable] [common]` | Change a connector |
| `connector secret set <id> [field] [--from-file PATH]` | Set a secret (default `client_secret`; a Google device client's is `device_client_secret`, a directory sync's `service_account_key`), from a hidden prompt, stdin or a file. Give a service account key with `--from-file` |
| `connector test <id>` | Test it step by step |
| `connector sync <id> [--confirm]` | Run a pulled connector's directory sync now and print the result. `--confirm` applies a run the safety stop blocked (needs a recent login) |
| `connector remove <id> [--detach-users]` | Delete it and its SCIM tokens. The users it provisioned are disabled, or with `--detach-users` stay enabled as the directory's own; prints how many users and teams it released |
| `connector scim-token list <id>` | A SCIM connector's endpoint path and tokens: id, name, expiry, who made it; never the tokens |
| `connector scim-token create <id> [--name TEXT] [--days N]` | Make a token for the IdP (`gxs_…`), printed once with the endpoint path. `--days` 1–365 (default 180); at most five live per connector |
| `connector scim-token revoke <id> <token-id>` | Revoke a SCIM token |
| `invite list <tenant>` | A tenant's invitations: open, taken or expired |
| `invite create <tenant> <email> --connector C [--role member\|role.tenant-admin\|role.tenant-auditor] [--days N]` | Invite an email address into a tenant; prints the link once. `--role` defaults to `member`, `--days` to 7 (at most 30) |
| `invite revoke <id>` | Delete an invitation |
| `federation list <tenant>`, `show <tenant> <id>` | A tenant's workload federations: issuer, audience, enabled, and each rule's claims, service account and token lifetime |
| `federation add <tenant> <id> --issuer URL --audience AUD [--name TEXT] (--match CLAIM=VALUE… --service-account TOKEN-ID [--ttl SECS] \| --rules-file FILE)` | Trust a workload issuer. `--match` (repeated) and `--service-account` make one rule; `--rules-file` is a JSON list of `{match, service_account, ttl_secs}`. A rule names `repository`, `project_path` or `sub`, exact values only; `--ttl` 60–3600 (default 900). Needs a recent login |
| `federation enable <tenant> <id>`, `disable <tenant> <id>`, `remove <tenant> <id>` | Enable, disable or remove a federation; disabling or removing it stops the tokens it gave |

Common connector flags: `--jit` / `--no-jit`, `--require-group G`…,
`--mfa` / `--no-mfa`, `--team GROUP=TEAM`…, `--default-tenant T`,
`--token-max-days N`, `--provisioning none|scim|pull` (`scim`: the IdP
provisions users over SCIM, `oidc` and `jumpcloud` only; `pull`: a
Google Workspace connector reads its users from Google; both with
`--no-jit`), `--group-mode mapped|auto` (`auto`: a team per IdP
group, for a provisioned connector).

Directory sync flags, for `--provisioning pull`: `--sync-admin EMAIL`,
`--sync-group GROUP`… (1 to 50), `--sync-interval MIN` (default 15, at
least 5), `--sync-max-disable PCT` (1 to 100, default 10),
`--ssh-keys-field SCHEMA.FIELD`. Any of them replaces all the sync
settings: repeat the ones you keep.

See [Authentication](../operator-guide/authentication.md),
[Provisioning with SCIM](../operator-guide/authentication.md#provisioning-with-scim),
[Directory sync from Google Workspace](../operator-guide/authentication.md#directory-sync-from-google-workspace)
[Invitations](../operator-guide/authentication.md#invitations) and
[Workload federation](../operator-guide/workload-federation.md).

## Cluster and nodes

`<node>` is a node's name or id.

| Command | Effect |
|---|---|
| `cluster status`, `cluster` | Raft group, nodes, versions and port reachability; says so on a standalone host |
| `cluster init [--advertise <ip:port>] [--tunnel-ip <ip>] --force` | Make this host a cluster of one (keeps a backup; can't be undone) |
| `cluster join-token [--role agent\|server] [--ttl <hours>h] [--allow-import]` | Create a one-time join token, printed once (default role `agent`) |
| `cluster join --server <ip:8842> [--role server\|agent] [--advertise <ip:port>] [--tunnel-ip <ip>] [--name <name>] [--token-file <file>] [--import]` | Join this host to a cluster. The token is read from the file (mode `0600`) or stdin, never from the command line. `--import` brings this host's VMs and disks once an administrator approves |
| `cluster rejoin --server <ip:8842> [--node-id <id>] [--advertise <ip:port>] [--tunnel-ip <ip>] [--token-file <file>]` | Come back as the same node, with a token from `node rejoin-token` |
| `cluster promote <node>… [--force]` | Make learner servers Raft voters; `--force` allows an even number of voters |
| `cluster leave` | Go back to standalone after this node was removed |
| `cluster dissolve --force` | The last node ends the cluster, keeping everything |
| `cluster rotate-ca [--grace <secs>]` | Rotate the cluster CA |
| `cluster rotate-secrets-key` | Rotate the key that protects connector secrets |
| `cluster snapshot <file>` | Save a snapshot of the replicated store (written `0600`; store it like the database) |
| `nodes`, `node` | List nodes |
| `node <node>` | One node in detail |
| `node drain <node>`, `node undrain <node>` | Stop or resume placing VMs on a node |
| `node prepare-device <node> <address>`, `node release-device <node> <address>` | Bind a device's IOMMU group to vfio-pci on its node, or put it back on its drivers |
| `node sriov <node> <pf-address> <vfs>` | Give a physical function that many SR-IOV virtual functions (0 removes them) |
| `node cordon-device <node> <address>`, `node uncordon-device <node> <address>` | Stop or resume giving a device to new VMs |
| `node topology <node> [--devices]` | A node's NUMA nodes, CPUs, memory, hugepages and the devices a VM could be given, read on that node; `--devices`: the devices with their place in the PCIe tree |
| `node remove <node> [--force]` | Remove an empty node |
| `node forget <node> [--fenced] [--force]` | Forget a node that is gone for good |
| `node purge <node>` | Drop a forgotten node's record |
| `node rejoin-token <node> [--raft-lost]` | Token for the node to rejoin as itself, printed once |
| `node detach <node> [--map-network <cluster-net>=<node-net>]… [--with-access] [--timeout <secs>]` | Detach a node with its resources, back to standalone. `--abort` cancels; `--departed` records a node that already left on its own. The answer names the networks mapped or dropped |
| `node import list \| show <plan> \| reject <plan>` | Import plans |
| `node import approve <plan> [--project src=existing\|src=new:name]… [--network a=b]… [--credential proj/user=new]… [--over-quota] [--dry-run]` | Approve an import with its mappings |

See [Clusters](../operator-guide/clusters.md).

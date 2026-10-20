# Configuration

The control plane reads `/etc/glidex/control-plane.json` at start. The
file is optional: without it every setting has its default, and that is
how the installer leaves a host. `/etc/glidex/control-plane.json.example`
lists every top-level setting with its default; copy it to start:

```bash
sudo install -m 0640 -o root -g glidex \
  /etc/glidex/control-plane.json.example /etc/glidex/control-plane.json
sudoedit /etc/glidex/control-plane.json
sudo systemctl restart glidex-control-plane
```

- Keep the file `root:glidex`, mode `0640`: the control plane (user
  `glidex`) must be able to read it, nobody else needs to.
- Changes take effect on restart. Restarting the control plane leaves
  running VMs running.
- The file is checked strictly. An unknown key (a typo), a wrong type or
  a value out of range stops the control plane with a message naming
  the setting; it never falls back to defaults. Read it with
  `journalctl -u glidex-control-plane -b`.
- Every key is optional. A section you leave out, or a key inside one,
  keeps its default.
- The file holds no secrets. TLS keys, the OIDC client secret and the
  secrets key are given to the service as systemd credentials (see
  [Secrets](#secrets)).

## Reference

### Listeners and identity

| Key | Type | Default | Meaning |
|---|---|---|---|
| `listen` | array of `"addr:port"` | `["0.0.0.0:8841", "[::]:8841"]` | TCP listeners for the API. On a host without IPv6, `[::]` is skipped. |
| `tls` | `"auto"`, `"off"` or `{"cert", "key"}` | `"auto"` | HTTPS on the TCP listeners: a self-signed certificate (`auto`), your own (`{"cert": "/path/cert.pem"}`), or plain HTTP (`off`, only if every listener is loopback). See [TLS certificates](tls.md). |
| `server_name` | string | the host name | Display name the server reports to clients; `gxctl` shows it in login profiles. |
| `api_socket` | path | `<run dir>/api.sock` | Unix socket for local `gxctl` users. |
| `ui_socket` | path | `<run dir>/ui.sock` | Unix socket the web UI talks to. |
| `ui_user` | string | `"glidex-ui"` | The only user allowed on `ui_socket`. |
| `users_group` | string | `"glidex-users"` | Members may use `api_socket`. |
| `admin_group` | string | `"glidex-admin"` | Members (and root) are break-glass administrators on `api_socket`. |

### `auth`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `auth.allowed_origins` | array of URLs | `[]` | Browser origins allowed to make changes and open consoles when they call the API directly on port 8841. Empty means `https://<name>:5173` for every name and address of this host. Set it when browsers call the API under another name; it then replaces the default list. It doesn't restrict the web UI: requests through the UI are accepted from the address the user opened, once the UI has checked that name against `GLIDEX_UI_HOSTS`. To limit the names browsers may use, narrow `GLIDEX_UI_HOSTS` instead. |
| `auth.session.idle_minutes` | integer | `30` | A browser session ends after this long without a request. |
| `auth.session.absolute_hours` | integer | `12` | A browser session ends this long after login, whatever its use. |
| `auth.tokens.default_days` | integer | `90` | Lifetime of a new access token when none is asked for. |
| `auth.tokens.max_days` | integer | `365` | Longest lifetime a token may be given. |
| `auth.pam` | object | see below | Local accounts (the `file-pam` login connector). |
| `auth.oidc` | object | see below | Single sign-on through an OpenID Connect provider (the `file-oidc` connector). |
| `auth.secrets_key` | path | `secrets.kek` beside the database | File holding the key that encrypts connector secrets. The `secrets-kek` credential wins when present. |
| `auth.federation.allow_private_issuers` | bool | `false` | Let a [workload federation](authentication.md#workload-federation-ci-without-stored-tokens)'s issuer be on this host or the site's network. Off, an issuer must be `https` with public addresses: names that resolve only to loopback, private, link-local or unique-local addresses, and URLs naming such an address, are refused. Turn it on for an issuer on your own network (a self-hosted GitLab, a Kubernetes cluster's issuer); it also lets plain-HTTP loopback issuers in, for tests. |

`auth.pam`, see [Local accounts](authentication.md#local-accounts):

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `true` | Allow web logins with local passwords. |
| `allowed_groups` | array of strings | `["glidex-users"]` | Unix groups whose members may log in. Keep in step with `glidex-authd`'s own `allowed_groups`. |
| `jit` | bool | `true` | Create the Glidex user on their first successful login. |
| `group_teams` | object | `{}` | Unix group → team name; memberships are updated at every login. |
| `authd_socket` | path | `"/run/glidex-authd/auth.sock"` | `glidex-authd`'s socket. |
| `service` | string | `"glidex"` | PAM service name (`/etc/pam.d/glidex`). |

`auth.oidc`, see [OIDC](authentication.md#single-sign-on-oidc):

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | Turn OIDC login on. It also needs `issuer` and `client_id`. |
| `issuer` | URL | `""` | The provider's issuer URL (`https://…`). |
| `client_id` | string | `""` | The client id registered with the provider. |
| `scopes` | array of strings | `["openid", "profile", "email", "groups"]` | Scopes requested. |
| `groups_claim` | string | `"groups"` | ID token claim that lists the user's groups. |
| `jit` | bool | `true` | Create the Glidex user on their first login. |
| `allowed_domains` | array of strings | `[]` | If set, only these email domains may log in. |
| `required_groups` | array of strings | `[]` | If set, only members of one of these groups may log in. |
| `group_teams` | object | `{}` | Provider group → team name; memberships are updated at every login. |
| `redirect_uri` | URL | first allowed origin + `/api/auth/oidc/callback` | Where the provider sends the browser back. Register exactly this URL with the provider. |

### `authz`, `quotas`, `pci`, `audit`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `authz.policy_files_dir` | path | `"/etc/glidex/policies"` | Directory of read-only site policy files (`*.cedar`). See [Access control](access-control.md#policy-files). |
| `authz.policy_history` | integer, 1–1000 | `50` | Versions kept of each site policy. |
| `quotas.default` | object | `{"networks": 2}` | Quotas of new projects, and of the `default` project when it is first created. Keys: `vms`, `vcpus`, `memory_mib`, `disk_gib`, `running_vms`, `networks`, `routers`, `external_ips`, and `devices` (`{"a100": 2}`: devices per pool). An omitted key is unlimited, except `networks` (2), `routers` (1) and `external_ips` (1); `null` is unlimited. |
| `pci.allow` | array | `[]` | PCI devices projects may pass through without host rights: `{"bdf": "0000:01:00.0", "projects": [...], "tenants": [...], "node": "gpu-01"}` (ids or names; `node` optional: without it, the device at that address on every node), or `{"pool": "a100", ...}` for a pool. See [PCI passthrough](pci-passthrough.md). |
| `pci.pools` | array | `[]` | Device pools VMs ask for by count: `{"name": "a100", "class": "gpu", "match": {"vendor_id": "0x10de", "device_id": "0x20b0"}, "nodes": [...], "prepare": true}`; `prepare` binds the pool's devices to `vfio-pci` when glidex starts; `match.vf_of` (a match of PFs) takes their SR-IOV VFs. See [Device pools](host-resources.md#device-pools). |
| `audit.retention_days` | integer | `90` | Audit log entries older than this are deleted. |

The control plane refuses to start when the pools or grants don't add
up, with one of:

- `pci.pools: pool names must be set and unique (…)`;
- `pci.pools: pool <name> matches every device; give it a match`;
- `pci.pools: pools <a> and <b> match the same devices`;
- `pci.allow: no pool <name>`: a grant names an unknown pool;
- `pci.allow: each grant names one bdf or one pool`.

### `reconcile` and `console`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `reconcile.vm_runner` | `"auto"`, `"systemd"`, `"detached"` | `"auto"` | Where VM processes run. `auto` is `systemd` (one `glidex-vm@<id>` unit per VM) when the control plane runs as a systemd service, else `detached`. The installed service refuses `detached`. |
| `reconcile.workers` | integer, 1–64 | `4` | VMs reconciled in parallel. |
| `reconcile.resync_secs` | integer, 5–3600 | `30` | Seconds between full checks of every VM. |
| `reconcile.on_host_boot` | `"resume"` or `"stop"` | `"resume"` | After a host reboot, start the VMs that were running (`resume`) or leave them stopped. |
| `reconcile.host_shutdown_grace_secs` | integer, 0–300 | `60` | At host shutdown, how long a VM gets after its power button before it is stopped. |
| `console.log_max_bytes` | integer, 1 MiB–256 MiB | `16777216` | A VM's console log is rotated past this size; one old log is kept. |

### `metering`

See [Usage](../user-guide/usage.md) for what is measured.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `metering.enabled` | bool | `true` | Sample resource usage. Turning it off and on again doesn't double count. |
| `metering.sample_secs` | integer | `30` | Seconds between samples: 10–300, and must divide 300. |
| `metering.storage_secs` | integer | `900` | Seconds between disk space measurements (`sample_secs`–86400). |
| `metering.close_grace_secs` | integer | `120` | An hour's figures are final this long after it ends (`sample_secs`–3600). |
| `metering.retention_days` | integer, 1–3650 | `90` | Days of hourly figures kept. |
| `metering.retention_daily_days` | integer, 1–3650 | `1825` | Days of daily figures kept. |
| `metering.retention_rate_days` | integer, 35–400 | `100` | Days of 5-minute rate figures (bandwidth, I/O, CPU) kept. |
| `metering.billing_timezone` | string | `"UTC"` | Billing months are calendar months in this zone: `UTC`, `±HH:00`, or an IANA zone with a fixed whole-hour offset. |

### `cluster`

Read only on hosts that are, or are about to become, part of a cluster.
See [Clusters](clusters.md).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `cluster.listen` | `"addr:port"` | `"0.0.0.0:8842"` | The node API, between nodes. |
| `cluster.advertise` | `"addr:port"` | `listen`, with this host's first non-loopback address | Where other nodes reach this one. |
| `cluster.node_grace_secs` | integer, 5–3600 | `40` | How long a node may be silent before it counts as unreachable. Every node, servers included, reports to the leader every 5 seconds: keep this well above that. |
| `cluster.shared_local_accounts` | bool | `false` | Treat a local account name as the same person on every node (only if your hosts share one account database). |
| `cluster.raft.heartbeat_ms` | integer, 50–5000 | `250` | Raft heartbeat. |
| `cluster.raft.election_ms` | `[min, max]` | `[1000, 2000]` | Raft election timeout; `min` at least 3 heartbeats, `max` above `min`. |
| `cluster.raft.log_keep_entries` | integer, ≥ 100 | `10000` | Raft log entries kept once every server has applied them. |
| `cluster.scheduler.cpu_overcommit` | number, ≥ 1.0 | `4.0` | vCPUs placed per host CPU. |
| `cluster.scheduler.memory_overcommit` | number, ≥ 1.0 | `1.0` | Guest memory placed per MiB of host memory. |
| `cluster.node_reserved.cpus` | integer | `1` | CPUs per node kept for the host. |
| `cluster.node_reserved.memory_mib` | integer | `2048` | Memory per node kept for the host. |
| `cluster.node_reserved.cpu_list` | CPU list (`0-1,128-129`) | unset | CPUs no VM's dedicated vCPUs get, and shared VMs keep off once a node has dedicated VMs. Unset: the first core of each NUMA node, once a node has a dedicated VM. `""` reserves none. OVS-DPDK's PMD and lcore CPUs are always reserved. |
| `cluster.scheduler.dedicated_whole_cores` | boolean | `true` | Dedicated vCPUs take whole SMT cores (a sibling no other VM shares). `false`: single threads. |
| `cluster.detach_freeze_secs` | integer | `600` | How long a node's detach (with its VMs) may take before it is called off. |
| `cluster.import_plan_ttl_secs` | integer | `86400` | How long an import plan waits for approval. |
| `cluster.ca_rotation_grace_secs` | integer | `604800` | How long the old cluster CA stays trusted after a rotation. |
| `cluster.ui_relay` | bool | `true` | On an agent, relay the web UI to a server. |
| `cluster.ovn.bridge_mappings` | object | `{}` | Physical network name → this host's Glidex bridge. Per host. |

<!-- TODO(verify): cluster.role and cluster.tunnel_ip parse, but no code path reads them from the file (the role and tunnel address come from the join/init request); left out until confirmed. -->

The other `cluster.ovn` keys (`enabled`, `underlay_mtu`, `nat_supernet`,
`edge`, `snat_ct_zones`, `dns_servers`) are still accepted, but once the
cluster's OVN settings are saved (on the web UI's Cluster page) those
decide, and the file's are ignored with a warning in the log.

### Example

A host reached as `glidex.example.org`, with its own certificate and
single sign-on:

```json
{
  "server_name": "lab",
  "tls": { "cert": "/etc/glidex/tls/glidex.crt" },
  "auth": {
    "allowed_origins": ["https://glidex.example.org:5173"],
    "oidc": {
      "enabled": true,
      "issuer": "https://idp.example.org",
      "client_id": "glidex",
      "required_groups": ["glidex-users"],
      "group_teams": { "lab-staff": "staff" }
    }
  },
  "quotas": { "default": { "vms": 10, "vcpus": 32, "memory_mib": 65536 } }
}
```

`allowed_origins` here lets browsers call the API on port 8841 under
that name and gives OIDC its default `redirect_uri`; the web UI doesn't
need it. The TLS key and the OIDC client secret are not in the file:
see [Secrets](#secrets).

## Secrets

Secrets go to the control plane as systemd credentials, never in
`control-plane.json`, on the command line or in the environment. Keep
the source files `root:root`, mode `0600`; systemd copies each into a
directory only the service can read. Add them in a drop-in:

```bash
sudo systemctl edit glidex-control-plane
```

```ini
[Service]
LoadCredential=tls-key:/etc/glidex/tls/glidex.key
LoadCredential=oidc-client-secret:/etc/glidex/oidc-client-secret
```

| Credential | Used for |
|---|---|
| `tls-key` | The private key of a configured certificate (when `tls.key` is not set). |
| `oidc-client-secret` | The client secret of `auth.oidc`. |
| `secrets-kek` | The key that encrypts the secrets of login connectors made through the API. Without it the control plane keeps one in `secrets.kek` beside its database. |

Then `sudo systemctl restart glidex-control-plane`.

## Files and directories

Under the installed services:

| Path | What |
|---|---|
| `/etc/glidex/control-plane.json` | This file. |
| `/var/lib/glidex-control-plane/.glidex/` | Data directory: `glidex.db` (the database), `secrets.kek`, `images/`, `disks/`, firmware, `tls/` (the self-signed certificate). |
| `/run/glidex-cp/` | Run directory: `api.sock`, `ui.sock`, `tls.crt` (a copy of the API's certificate for local clients), and one private directory per running VM. |
| `/run/glidex/` | `glidex-netd`'s sockets. |
| `/run/glidex-authd/auth.sock` | `glidex-authd`'s socket. |
| `/var/lib/glidex-ui/tls/` | The web UI's self-signed certificate. |
| `/srv/glidex/` | The datastore root: registered [datastores](datastores.md) are directories under it. The installer creates it (`root:root`, mode 0755). |

The data directory is `~/.glidex` of the user the control plane runs as;
for the service that user is `glidex`, whose home is
`/var/lib/glidex-control-plane`. The service can only write inside that
home, so image and disk directories, and files you give the API by path,
must be inside it or readable by `glidex` outside `/home`, `/root`,
`/run/user` and `/tmp`.

A control plane you start by hand uses your own `~/.glidex` and the run
directory `$XDG_RUNTIME_DIR/glidex` (else `/tmp/glidex-<uid>`).

## Outbound connections

Besides what [Installation](installation.md#requirements) needs, the
control plane makes these outbound HTTPS connections:

| To | When |
|---|---|
| Image and firmware URLs, and the catalog's mirrors | Pulling images. |
| Your OIDC provider, Google, JumpCloud | Logins through those connectors. |
| `oauth2.googleapis.com`, `admin.googleapis.com` | A [directory sync from Google Workspace](authentication.md#directory-sync-from-google-workspace), from the cluster's leader: allow it from every server. |
| `snapshot.debian.org` | The first kernel extraction on a host: the pinned extractor packages, about 35 MB, kept afterwards. Not needed with `GLIDEX_KERNEL_EXTRACTION=off`. |

## Environment variables

### Control plane

| Variable | Default | Meaning |
|---|---|---|
| `GLIDEX_CONFIG` | `/etc/glidex/control-plane.json` | The configuration file. |
| `GLIDEX_LISTEN` | `listen` from the file | Comma-separated `addr:port` list; replaces `listen`. |
| `GLIDEX_RUN_DIR` | `$XDG_RUNTIME_DIR/glidex`, else `/tmp/glidex-<uid>` | Run directory. Under systemd it is `/run/glidex-cp` (`RuntimeDirectory=`), which wins. |
| `GLIDEX_IMAGE_DIR` | `<data dir>/images` | Where downloaded images are kept. |
| `GLIDEX_DISK_DIR` | `<data dir>/disks` | Where disks are kept. |
| `GLIDEX_MAX_IMAGE_SIZE` | `64G` | Largest image download, with a `K`, `M`, `G` or `T` suffix (binary units). |
| `GLIDEX_DEFAULT_ROOT_GIB` | `10` | Root disk size, in GiB, when a request gives none. |
| `GLIDEX_MAX_DOWNLOADS` | `2` | Image downloads at the same time. |
| `GLIDEX_ALLOW_PRIVATE_IMAGE_URLS` | unset | `1` allows image downloads from private and local addresses. |
| `GLIDEX_NETD_RUN_DIR` | `/run/glidex` | Where to find `glidex-netd`'s sockets. |
| `GLIDEX_DATASTORE_ROOT` | `/srv/glidex` | The directory registered datastores must be under. The installed units only let the control plane and the VMs write under `/srv/glidex`: if you change it, add `ReadWritePaths=<new root>` in drop-ins for both `glidex-control-plane` and `glidex-vm@` (`sudo systemctl edit glidex-vm@`), on every node. |
| `GLIDEX_KERNEL_EXTRACTION` | on | `off` never extracts kernels from disk images: catalog pulls bring only the disk image, `kernel_from` and `extract-kernel` are refused (`400 invalid_image`), and a VM following its guest's kernel keeps the one it has. For a host that can't download the extractor. See [Kernel boot](../user-guide/kernel-boot.md#from-a-cloud-image). |
| `GLIDEX_MAX_EXTRACTIONS` | `1` | Kernel extractions at the same time on this node. |
| `GLIDEX_MAX_KERNEL_SIZE` | `128M` | Largest kernel of a kernel image, with a `K`, `M`, `G` or `T` suffix (binary units). |
| `GLIDEX_MAX_INITRD_SIZE` | `1G` | Largest initramfs of a kernel image, likewise. |
| `GLIDEX_OIDC_CLIENT_SECRET_FILE` | unset | File with the OIDC client secret, used when there is no `oidc-client-secret` credential (for a control plane run by hand). |
| `RUST_LOG` | `glidex_control_plane=info,tower_http=info` | Log filter. |

Set them for the service in a drop-in (`sudo systemctl edit
glidex-control-plane`) with `Environment=NAME=value` lines.

`gxctl` reads its own variables (`GLIDEX_SOCKET`, `GLIDEX_TOKEN`,
`GLIDEX_CA_CERT`, `GLIDEX_PROFILE`, `GLIDEX_PROJECT`); see the [gxctl
reference](../reference/gxctl.md). In `gxctl`'s environment
`GLIDEX_CONFIG` names its login profile file
(`~/.config/glidex/config.json` by default), not the control plane's
configuration.

### Web UI

`glidex-ui` has no configuration file. The installed unit sets
`GLIDEX_UI_DIR` and `GLIDEX_API_SOCKET`; change the others in a drop-in
(`sudo systemctl edit glidex-ui`).

| Variable | Default | Meaning |
|---|---|---|
| `GLIDEX_UI_LISTEN` | `0.0.0.0:5173,[::]:5173` | Comma-separated addresses to listen on. |
| `GLIDEX_UI_HOSTS` | this host's names and addresses | Allowed `Host` values, `host[:port]`, comma-separated; other requests get `421`. Set it when users reach the UI under another name. The ts.net names of `GLIDEX_UI_TLS_TAILSCALE` are allowed either way. |
| `GLIDEX_UI_TLS` | `auto` | `auto` (HTTPS) or `off` (plain HTTP, loopback addresses only). |
| `GLIDEX_UI_TLS_CERT` | a self-signed certificate | PEM certificate chain. |
| `GLIDEX_UI_TLS_KEY` | the `ui-tls-key` credential | PEM private key. Prefer the credential. |
| `GLIDEX_UI_TLS_TAILSCALE` | `off` | `on`: also serve the node's Tailscale certificate to its ts.net names ([TLS](tls.md#tailscale-certificates)). |
| `GLIDEX_UI_TAILSCALE_SOCKET` | `/run/tailscale/tailscaled.sock` | `tailscaled`'s local API socket. |
| `GLIDEX_UI_DIR` | `/usr/local/share/glidex/ui` (unit) | The built UI. |
| `GLIDEX_API_SOCKET` | `/run/glidex-cp/ui.sock` | The control plane's UI socket, used when it exists. |
| `GLIDEX_API_URL` | `https://127.0.0.1:8841` | The control plane over TCP, when there is no socket (development). |
| `GLIDEX_API_CA_CERT` | the local control plane's certificate | PEM file to trust for `GLIDEX_API_URL`. |
| `RUST_LOG` | `info` | Log filter. |

For example, to serve the UI on one address under a DNS alias:

```ini
[Service]
Environment=GLIDEX_UI_LISTEN=192.0.2.10:5173
Environment=GLIDEX_UI_HOSTS=glidex.example.org,192.0.2.10
```

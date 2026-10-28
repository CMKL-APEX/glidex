# Repository and code organization

This page maps the repository: what each crate is, which process runs
where, how the control plane is split into modules, and where the
protos, the web UI, the packaging and the docs live. For how these
pieces work together at run time, see [Architecture](architecture.md).

Links to source files point at `main` on GitHub.

## Top level

```text
glidex/
├── Cargo.toml        workspace: 15 crates, edition 2024, MIT OR Apache-2.0
├── rustfmt.toml      rustfmt's defaults (edition only)
├── crates/           all Rust code, the web UI and its e2e suite
├── spec/             design specs (contributors)
├── docs/
│   ├── public/       this book (mdBook)
│   ├── internal/     book config that builds spec/ as a book
│   └── site/         landing page of the published site
├── packaging/        systemd units, polkit rule, PAM file, config example
├── scripts/          CI lint and dev/test helpers
├── LICENSES/         MIT and Apache-2.0 texts, COPYRIGHT
└── .github/workflows CI: rust.yml, docs.yml
```

## Crates

Every crate is in `crates/` and listed in the workspace
[`Cargo.toml`](https://github.com/CMKL-APEX/glidex/blob/main/Cargo.toml).

| Crate | Kind | What it is | Used by |
|---|---|---|---|
| `glidex-control-plane` | lib + bins `glidex-control-plane`, `gxctl` | The server: REST and RPC API, auth and Cedar authorization, the store, controllers, hypervisor drivers, scheduler, clustering, metering. Also the `gxctl` CLI. | Run as a service; its tests |
| `glidex-api` | lib | The RPC API: protos, generated messages, services and clients, and the operation table. | Control plane (server), `gxctl` (client); the UI through generated TypeScript |
| `glidex-vm-shim` | lib + bin | Per-VM supervisor: parent of the hypervisor, owner of its console (PTY, console socket, log) and exit record. The library half holds the file formats (`launch.json`, `instance.json`) and the `shim.sock` client. | Started by the control plane, one per VM |
| `glidex-hv-client` | lib | Blocking clients for Cloud Hypervisor's HTTP API and QEMU's QMP. | Shim and control plane |
| `glidex-ovs` | lib | Everything Glidex does to the host network: OVS install, bridges, uplinks, NAT, VM ports, VFIO and SR-IOV, meters. Depends on no other Glidex crate. | `glidex-netd`, `glidex-ovn`, `glidex-install`, control plane |
| `glidex-ovn` | lib | OVN northbound database as Glidex uses it: a desired state and a `sync` that touches only rows Glidex owns. | Control plane (cluster networks) |
| `glidex-netd` | lib + bin | Root network helper daemon wrapping `glidex-ovs`; the library is its wire protocol and client. | Control plane (client), installer |
| `glidex-stord` | lib + bin | Root storage helper for LVM-thin datastores; protocol and client in the library. | Control plane |
| `glidex-authd` | lib + bin | Root PAM helper that checks local passwords; protocol and client in the library. | Control plane |
| `glidex-auth` | lib | Core authentication, kept free of Glidex's own records so other services can reuse it: secrets (random values, base62, SHA-256, constant-time compare), session and token rules, a Cedar policy engine generic over a `Model` (schema, policies, entity types, break-glass rule), and OIDC login (code flow with PKCE, device grant, JWKS). | Control plane (`auth/`, `authz.rs`, `directory/oidc.rs` specialize it for Glidex), `glidex-authd` |
| `glidex-tls` | lib | HTTPS listeners, self-signed certificates, reloadable certificates chosen by SNI, client trust. | Control plane, `gxctl`, `glidex-ui`, installer |
| `glidex-raftstore` | lib | A replicated ReDB store on openraft: transactions recorded as write sets, applied as bytes on every member; an in-process test cluster. | Control plane (`store/`, `cluster/raft/`) |
| `glidex-ui` | bin + web app | The Rust binary serves the built UI and proxies `/api` to the control plane; `ui/` is the Vite + React app; `e2e/` its Playwright suite. | Run as a service |
| `glidex-install` | bin | Installer and uninstaller: packages, hypervisors, firmware, OVS, users and groups, units, the UI build. | Run by hand |
| `glidex-e2e` | test crate | Helpers and the nested cluster test that installs Glidex into VMs of the host's own Glidex. | `cargo test -p glidex-e2e` |

Each library's crate-level doc (`//!` in `src/lib.rs`) is the place to
start reading it.

## Processes

```text
             browser                     gxctl / scripts
                │ :5173                        │ :8841 or api.sock
                ▼                              ▼
        glidex-ui (glidex-ui) ── ui.sock ─▶ glidex-control-plane (glidex)
                                           │   │   │        ▲
                    ┌──────────────────────┘   │   └──┐     │ :8842, mutual TLS
                    ▼ unix sockets             ▼      ▼     ▼
   glidex-netd · glidex-stord · glidex-authd   systemd   other nodes' control planes
          (root helpers)                       │
                                               ▼
                               glidex-vm@<id>.service (glidex)
                                 glidex-vm-shim ─▶ cloud-hypervisor | qemu
```

| Binary | Runs as | Started by | Notes |
|---|---|---|---|
| `glidex-control-plane` | `glidex` | `glidex-control-plane.service` | REST and RPC on `:8841`, `/run/glidex-cp/api.sock` (local users) and `/run/glidex-cp/ui.sock` (the UI); node API on `:8842` in a cluster. A few flags run a one-off recovery and exit (`--force-new-cluster`, `--leave-removed`, `--detach-offline`). |
| `glidex-vm-shim` | `glidex` | `glidex-vm@<id>.service`, in `glidex-vms.slice`, started and stopped by the control plane through systemd (allowed by a polkit rule) | `--vm <id> --dir /run/glidex-cp/vms/<id>`. Kernel extraction jobs use the same template (`glidex-vm@extract-<uuid>`). |
| `glidex-ui` | `glidex-ui` | `glidex-ui.service` | `:5173`. `--dev` runs the Vite dev server instead. |
| `glidex-netd` | root | `glidex-netd.service` | Socket under `/run/glidex`. `glidex-netd --boot-devices` also runs once per boot from `glidex-vfio-boot.service`. |
| `glidex-stord` | root | `glidex-stord.service` | Socket under `/run/glidex-stord`; idle until a volume group is allowed. |
| `glidex-authd` | root | `glidex-authd.socket` (socket activation) | `/run/glidex-authd/auth.sock`, group `glidex`. |
| `gxctl` | you | you | `api.sock` on the host; HTTPS with a token elsewhere. |
| `glidex-install` | you, with `sudo` | `cargo run -p glidex-install` | Fills in the `.in` units (service user, binary paths) and installs them. |

The unit files are in [`packaging/`](#packaging). The [Operator
guide](../operator-guide/README.md#components) describes the same units
from the operator's side.

## glidex-api: protos and generated code

The proto files are the source of truth for every API type
([`spec/grpc.md`](https://github.com/CMKL-APEX/glidex/blob/main/spec/grpc.md)
G-D3).

```text
crates/glidex-api/
├── build.rs        protox → checks → operations.rs, fields.rs → connectrpc-build
├── buf.yaml        buf lint (STANDARD) and breaking (FILE) rules
├── buf.gen.yaml    TypeScript for the UI → ../glidex-ui/ui/src/gen
├── proto/glidex/<area>/v1/<area>.proto
│                   access, auth, cluster, common, directory, host,
│                   network, storage, system, tenancy, usage, vm, watch
├── proto/glidex/api/v1/options.proto   (glidex.api.v1.op), (glidex.api.v1.secret)
├── proto/glidex/api/v1/errors.proto, types.proto
├── proto/google/rpc/error_details.proto   vendored ErrorInfo
└── src/lib.rs      generated modules, OPERATIONS, operation(), unknown_field()
```

What
[`build.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-api/build.rs)
does on every `cargo build`, with no `protoc` or `buf` needed:

1. Compiles every `.proto` under `proto/` with `protox`.
2. Fails the build if a rule is broken:
   - every RPC has a `(glidex.api.v1.op)` option with an action, a kind
     (`KIND_READ` or `KIND_WRITE`) and, if any, a REST twin in the form
     `"<METHOD> /path"`;
   - every field's JSON name equals its proto name (`json_name`, G-D11),
     so REST stays `snake_case`;
   - no response message reaches a field marked
     `[(glidex.api.v1.secret) = true]`.
3. Writes `operations.rs` (`glidex_api::OPERATIONS`: procedure, service,
   method, Cedar action, kind, REST twin, streaming) and `fields.rs`
   (every message's JSON field names, which the server uses to refuse
   unknown fields).
4. Writes a descriptor set (`glidex_api::DESCRIPTOR_SET`, for server
   reflection) and runs `connectrpc-build` for messages (`buffa` types),
   service traits and clients, included as `glidex_api::proto`.

The TypeScript for the UI is not generated by cargo: run `buf generate`
in `crates/glidex-api` (after `bun install` in `crates/glidex-ui/ui`).
Its output in `crates/glidex-ui/ui/src/gen/` is committed, and CI fails
when it is stale.

## glidex-control-plane

The control plane is one library (`src/lib.rs`) with two binaries:
`src/main.rs` (the server) and `src/bin/gxctl/` (the CLI). Its tests
are in `tests/`; the Cedar schema and shipped policies in `policies/`.

### Request path

```text
 HTTP request
   │  api/mod.rs      router: global layers (UI relay, authenticate, cluster gate)
   ├─▶ REST route     api/<area>.rs     decode JSON → call service → encode
   └─▶ RPC procedure  api/rpc/<area>.rs decode message → call service → encode
                         │
                         ▼
                      service/<area>.rs   Cedar checks, not-found vs forbidden,
                         │                quotas, names, the store write
                         ▼
                      state.rs (VmManager), store/, images/, datastore/, …
                         │  spec written; controllers woken
                         ▼
                      controller/*        observe the host, take the next step
```

### Module map

| Area | Modules | What is there |
|---|---|---|
| API adapters | `api/mod.rs`, `api/<area>.rs` | The router and the route table (`routes`: method, path, Cedar action, handler), the `Caller`, auth and audit layers, REST handlers. `api/errors.rs` maps internal errors to REST responses. |
| | `api/rpc/` | RPC adapters per proto package (`vm.rs`, `storage.rs`, …) and `unary`, which finds the operation, applies the listener rules, runs the service function and audits. |
| | `api/gate.rs`, `api/relay.rs` | In a cluster: reads after a barrier, writes forwarded to the leader; host-only requests relayed to the node they name. |
| | `api/op.rs`, `api/issuer.rs`, `api/scim.rs`, `api/trust.rs`, … | HTTP-only endpoints: a directory's OpenID provider, SCIM, trust between clusters and directories. |
| Service layer | `service/` | One transport-neutral function per operation (`service::storage::move_disk`, …). Every decision is made here, so REST and RPC can't drift apart. `ServiceError` carries the stable error code both transports send. |
| Admission | `state.rs` | `VmManager`: validates and writes desired state, then wakes the controllers. |
| Controllers | `controller/` | Level-triggered loops on one work queue (`queue.rs`): `vm.rs`, `disk.rs`, `image.rs`, `network.rs`, `ovn.rs`, `placement.rs`, `extract.rs`, `image_cache.rs`, `store_copies.rs`; `startup.rs` for adoption at start. |
| Store | `store/` | `db.rs`: Glidex's tables (`tables!`) on `glidex-raftstore`. `mod.rs`: the VM envelopes, events, `SCHEMA_VERSION` and `migrate`. |
| Cluster | `cluster/` | Membership, join tokens, node PKI and CA rotation, server and agent runtimes, import and departure; `cluster/raft/` wires `glidex-raftstore` to Glidex's schema. |
| Scheduling | `scheduler.rs`, `node.rs`, `topology.rs`, `pci.rs` | Where a VM runs (decided once, then sticky), nodes, NUMA topology, PCI devices and pools. |
| VM instances | `instance/`, `hypervisor/`, `paths.rs`, `devaccess.rs` | Running the shim (`instance/runner.rs`: a systemd unit, or detached for dev runs and tests), liveness; drivers turning a VM config into a Cloud Hypervisor or QEMU command line; per-VM runtime directories. |
| Images and disks | `images/`, `cloud_init.rs`, `credentials.rs` | Catalog, verified download, firmware, kernel extraction, disk create/resize/extend; cloud-init seeds; guest credentials. |
| Datastores | `datastore/` | Datastore records, the driver trait with file and LVM drivers, probing on each node. |
| Networking | `network.rs`, `ipam.rs`, `router.rs`, `ovn_settings.rs` | VM networks and the `glidex-netd` client, cluster IPAM, VPC routers, OVN settings. |
| Authentication | `auth/` | Principals from peer uid, sessions, tokens and passkeys; the bridge to Cedar. |
| Authorization | `authz.rs`, `policies/` | Glidex's Cedar `Model` (entities, roles, break-glass) over `glidex-auth`'s engine. `glidex.cedarschema` declares every action and its groups; `base.cedar` and `roles.cedar` are the shipped policies and role templates. |
| Tenancy and directories | `tenancy.rs`, `tenants.rs`, `directory/`, `trust.rs` | Projects and quotas; tenants and admissions; directory connectors (PAM, OIDC, presets), invitations, SCIM, sync, federation; trust between clusters and directories. |
| Metering | `metering/` | Samplers and sources, the hourly exactly-once ledger, rates, retention, queries. |
| Configuration | `config.rs`, `serve.rs`, `restart.rs` | `/etc/glidex/control-plane.json` (or `GLIDEX_CONFIG`), the listeners, restarting into another mode. |
| Shared types | `models.rs` | VM spec, status and phases, and other internal types. |

### gxctl

`src/bin/gxctl/main.rs` holds the interactive shell (rustyline) and the
dispatch; each area has its own file (`vm.rs`, `storage.rs`,
`network.rs`, `access.rs`, `cluster.rs`, `host.rs`, `directory.rs`,
`tenancy.rs`, `usage.rs`, `admin.rs`). `client.rs` has the REST client
and the Connect transport that carries the generated RPC clients over
gxctl's own connection; `config.rs` the login profiles; `console.rs`
the console attach loop. Only the global options are clap; commands
are parsed by hand.

### Tests

`tests/` holds one integration test binary per area (`vm_tests.rs`,
`storage_tests.rs`, `datastore_tests.rs`, `cluster_tests.rs`,
`rpc_tests.rs`, `tenancy_tests.rs`, …) with shared helpers in
`tests/common/`. `functional_tests.rs` boots real guests; most of it is
`#[ignore]`d. See [Contributing](contributing.md#tests).

## Web UI

```text
crates/glidex-ui/
├── src/main.rs        serve ui/dist (or GLIDEX_UI_DIR), proxy /api, TLS, Host allowlist; --dev
├── ui/                Vite + React + TypeScript + Tailwind
│   ├── src/App.tsx    routes → pages
│   ├── src/pages/     one file per page: Dashboard, VmDetail, Disks, Datastores, Cluster, …
│   ├── src/components/  forms, dialogs, cards (CreateVmForm, VmActions, PlacementPicker, …)
│   ├── src/api.ts     every API call; rpc() for RPCs, REST for the rest
│   ├── src/session.tsx  the kit's session layer with Glidex's adapter; capability checks (useCan)
│   ├── src/live.tsx   WatchService stream: live updates
│   └── src/gen/       generated by buf generate (committed; don't edit)
├── kit/               @glidex/ui-kit: Modal, TokenReveal, password form, SSO buttons,
│                      step-up dialog, session provider; takes an AuthApi adapter
└── e2e/               Playwright suite, run with bun
```

`api.ts` sends RPCs over gRPC-Web (`createGrpcWebTransport`, base
`/api`) with JSON messages, through `rpc(Service.method.x, req)`, which
returns the proto JSON the REST twin would answer. REST stays for what
has no RPC (health, login and logout, OIDC redirects, CSV exports, the
console WebSocket, `GET /auth/whoami`). The browser reaches the control
plane through `glidex-ui`, which forwards `/api` to `ui.sock`; that is
the only listener that takes gRPC-Web with a session cookie.

The UI is built with bun (`bun install`, `bun run build`), never npm.
`ui/` is the bun workspace root and lists `../kit` as a workspace, so
`bun install` in `ui/` covers both. The kit holds the login and session
pieces that aren't Glidex-specific, for reuse by other management UIs:
it never imports `api.ts` or `types.ts`; the app hands it an `AuthApi`
adapter (`methods`, `whoami`, `login`, `startSso`, `logout`).
Passkeys, accounts, roles and capability checks stay in `ui/`. See
[`kit/README.md`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-ui/kit/README.md).

## Packaging

[`packaging/`](https://github.com/CMKL-APEX/glidex/blob/main/packaging/)
holds what the installer puts on a host:

| File | What |
|---|---|
| `glidex-control-plane.service.in`, `glidex-vm@.service.in` | Control plane and per-VM units; `@USER@` and `@BIN@` filled in by the installer. |
| `glidex-vms.slice` | Slice for every VM unit. |
| `glidex-ui.service`, `glidex-netd.service`, `glidex-stord.service`, `glidex-authd.socket`, `glidex-authd.service`, `glidex-vfio-boot.service` | The other units. |
| `50-glidex-vm.rules.in` | Polkit rule: the control plane's user may start, stop and kill its own `glidex-vm@` units, nothing else. |
| `glidex.pam` | PAM service for `glidex-authd`. |
| `control-plane.json.example` | Every control plane setting with its default; a test keeps it loadable. |

## Scripts

| Script | What |
|---|---|
| `scripts/lint-store-writes.sh` | CI lint: database writes in the control plane go through the store (see [Contributing](contributing.md#conventions)). |
| `scripts/dev/ui-e2e.sh` | Run the UI e2e suite against a throwaway control plane. |
| `scripts/dev/ui-e2e-full.sh` | The same with nothing skipped (needs an image, KVM and `glidex-netd`). |
| `scripts/dev/fake-lan-setup.sh`, `fake-lan-teardown.sh` | A fake LAN in a network namespace for the uplink tests (root). |

## Specs and docs

- [`spec/`](https://github.com/CMKL-APEX/glidex/blob/main/spec/README.md):
  one file per area (`architecture.md`, `reconciliation.md`,
  `storage.md`, `clustering.md`, `grpc.md`, `security.md`,
  `tenancy.md`, …). Its README has a table of contents with each
  file's scope and status (design, draft, implemented). Code cites them
  by path, so they are never moved or renamed.
- `docs/public/`: this book. `docs/internal/`: the mdBook config that
  builds `spec/` as a book. `docs/site/`: the landing page.

## Where do I find…

| What | Where |
|---|---|
| A REST route and its Cedar action | `routes` in `crates/glidex-control-plane/src/api/mod.rs` (`.add("POST", "/disks/{id}/move", "moveDisk", …)`); handler in `api/<area>.rs` |
| An RPC | `crates/glidex-api/proto/glidex/<area>/v1/<area>.proto`; adapter in `api/rpc/<area>.rs` |
| The logic of an operation | `crates/glidex-control-plane/src/service/<area>.rs` |
| A Cedar action, its groups and resource type | `crates/glidex-control-plane/policies/glidex.cedarschema` |
| Who may do what by default | `policies/roles.cedar`, `policies/base.cedar` |
| A controller | `crates/glidex-control-plane/src/controller/<object>.rs` |
| A store table | `tables!` in `crates/glidex-control-plane/src/store/db.rs` |
| A schema migration | `migrate` and `SCHEMA_VERSION` in `crates/glidex-control-plane/src/store/mod.rs` |
| A configuration setting | `crates/glidex-control-plane/src/config.rs`; `packaging/control-plane.json.example` |
| A `gxctl` command | `crates/glidex-control-plane/src/bin/gxctl/<area>.rs` |
| A UI page | Route in `crates/glidex-ui/ui/src/App.tsx`; page in `ui/src/pages/` |
| A UI API call | `crates/glidex-ui/ui/src/api.ts` |
| A hypervisor command line | `crates/glidex-control-plane/src/hypervisor/cloud_hypervisor.rs`, `qemu.rs` |
| What happens on the host network | `crates/glidex-ovs/src/`, behind `glidex-netd` |
| A systemd unit | `packaging/` |
| A CI job | `.github/workflows/rust.yml`, `docs.yml` |
| The design behind any of these | The `spec/…` reference in the file's `//!` header |

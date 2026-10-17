# gRPC API (design)

Status: **design**; G0 is implemented (§15). Adds an RPC transport (gRPC, gRPC-Web and the
Connect protocol, served by `connectrpc`) next to the REST API of [rest-api.md](rest-api.md), sharing its authentication
([security.md](security.md) §5), Cedar authorization (§7), audit (§10)
and cluster forwarding ([clustering.md](clustering.md) §6.4). The first
services to use it are the tenancy services of
[tenancy.md](tenancy.md) §14.

## 1. Why, and what it is not

**Why.**
- Typed contracts, generated clients and a compatibility check in CI.
  Today REST request/response types are hand-written serde structs in
  `api/*.rs`, and `gxctl` mirrors them by hand.
- Server streaming for watch (today SSE, `api/watch.rs`), and later
  for console and events, without a second framing per stream.
- A stable surface for automation outside glidex (Terraform, SDKs).

**Not a replacement for REST.** REST/JSON stays, permanently, for curl,
scripts and automation that doesn't want generated clients, and for
protocols that are HTTP by definition (OIDC/OAuth callbacks, SCIM,
console WebSocket, `/health`). What changes is where its types come
from: REST bodies become the proto JSON form of the same messages, and
the hand-written serde types are removed family by family (G-D3, §9).
The web UI moves to gRPC-Web through Connect (G-D8, §11).

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| G-D1 | **One implementation, two adapters.** Every operation is a transport-neutral service function; REST and gRPC adapters only decode, call it and encode (tenancy.md §14.1 rules). | Authorization, quota and audit written once. Two copies of a Cedar check is exactly the kind of drift that becomes a cross-tenant bug. |
| G-D2 | **Same listeners, same port.** RPCs are served on `:8841`, `api.sock`, `ui.sock` and the cluster port, inside the existing axum router at their procedure paths (`/glidex.<area>.v1.<Service>/<Method>`); which protocols each listener accepts is in §6. | No new port to open, firewall or put in certificates; the TLS setup, certificate pinning and peer-uid identification are reused unchanged. |
| G-D3 | **Proto is the source of truth for every API type.** REST JSON is the proto3 JSON mapping of the same `buffa` messages, with `snake_case` names (G-D11). New operations start that way; existing hand-written request/response types in `models.rs` and `api/*.rs` are **removed** when their family is ported (§9), and the REST shape changes to the proto JSON form where they differ (§9.1). | Two type systems for one API drift apart; one definition serves REST, gRPC, gxctl and the UI. The cost is a documented, one-time shape change per family. |
| G-D4 | **Credentials per listener.** Native gRPC accepts `authorization: Bearer gxt_…` metadata, or peer uid on `api.sock`. Session cookies are accepted **only** for gRPC-Web from the `glidex-ui` peer on `ui.sock`, with the same Origin and CSRF checks as REST (§11). | Cookies are the browser's credential; keeping them on the one browser path keeps CSRF and Origin rules where browsers are. |
| G-D5 | **Stable error codes travel as `ErrorInfo.reason`.** | Clients branch on the same strings (`quota_exceeded`, `reauth_required`, …) on both transports. |
| G-D6 | **Read/write is declared per operation**, not inferred from the HTTP method. | Every gRPC call is a `POST`; cluster forwarding (§8) must still know a read from a write. |
| G-D8 | **Browser: Connect-ES over gRPC-Web.** The UI uses `@connectrpc/connect` + `@connectrpc/connect-web` with `createGrpcWebTransport`, and TypeScript messages from `@bufbuild/protoc-gen-es`. The server needs nothing extra: `connectrpc` speaks gRPC-Web itself. | gRPC-Web works over HTTP/1.1 without HTTP/2 trailers, so `glidex-ui` keeps proxying `/api` unchanged; the UI gets typed clients from the same protos as gxctl, and server streaming for watch. |
| G-D9 | **`buf` for lint, breaking checks and TypeScript generation.** Rust code is generated in `build.rs` by `connectrpc-build` from a descriptor set that `protox` (a Rust protobuf compiler) produces, so building the workspace needs neither `protoc` nor `buf`. | Contributors and packaging build with `cargo` alone; CI runs `buf lint`, `buf breaking` and checks the generated TypeScript is current. |
| G-D10 | **Runtime: `connectrpc`, not tonic.** One tower service answers Connect, gRPC and gRPC-Web, with binary or JSON messages; messages are `buffa` types, which carry their own proto3 JSON; health is `connectrpc-health`. Pinned to an exact version (`=0.9.x`), upgraded deliberately, like `openraft`. | Replaces `tonic`, `tonic-web`, `tonic-health`, `prost` and `pbjson` with one stack from the Connect project, the same protocol family as the browser client. It passes the ConnectRPC conformance suite; the cost is a pre-1.0 API that may still move. |
| G-D11 | **JSON names are the proto names.** Every field whose name has an underscore declares `json_name` equal to its name (`uint32 mem_size_mib = 3 [json_name = "mem_size_mib"];`); `build.rs` fails otherwise. | proto3 JSON defaults to `lowerCamelCase`; the REST API is `snake_case`. Declaring it in the proto (rather than rewriting descriptors at build time) keeps every generator, the TypeScript one included, and `buf curl` on the same names. |
| G-D7 | **Versioned packages** `glidex.<area>.v1`; breaking changes need a `v2` package served alongside `v1`. | Rolling upgrades (cluster-runbook.md) run mixed versions; field numbers and names are the compatibility contract. |

## 3. Stack

- `connectrpc` with the `axum` feature (server), and `client` +
  `client-tls` in gxctl. Messages are `buffa` / `buffa-types`; JSON
  through their generated serde impls.
- **Mounting.** `api::router` adds one route per service,
  `/glidex.<area>.v1.<Service>/{*method}`, served by the
  `connectrpc::Router`'s axum service. The global layers
  (`relay::ui_relay`, `authenticate`, `gate::cluster_gate`) therefore
  run for RPCs exactly as for REST; the per-route REST layers don't, and
  the RPC adapter does their job (§6).
- Connections: the listeners already serve HTTP/1.1 and HTTP/2 through
  hyper-util's auto builder (ALPN `h2`, `http/1.1` on TLS; HTTP/2
  prior knowledge also works on the Unix sockets), so native gRPC needs
  no listener change.
- `grpc.health.v1.Health` (`connectrpc-health`; its default features
  add zstd, a C build dependency only) on every listener, no auth, like
  `GET /health`, always answered by the server that receives it. Server reflection (`connectrpc-reflection`)
  on `api.sock` only (later; not in G0).
- Limits: 4 MiB per message (configurable, never above 16 MiB),
  deadlines from `grpc-timeout` / `connect-timeout-ms` capped at 5
  minutes for unary calls.

## 4. Proto layout

New crate `crates/glidex-api`: protos and the generated code, used by the
control plane (server) and `gxctl` (client), so both compile against
the same messages.

```
crates/glidex-api/
├── build.rs              # protox → checks → operation table → connectrpc-build
├── buf.yaml, buf.gen.yaml # lint/breaking rules; TS generation for glidex-ui
├── proto/
│   ├── glidex/api/v1/options.proto   # (glidex.api.v1.op), (glidex.api.v1.secret); build-time only
│   ├── glidex/api/v1/errors.proto    # ErrorDetails
│   ├── glidex/system/v1/system.proto # G0: SystemService.GetServerInfo, WhoAmI
│   ├── glidex/tenancy/v1/tenancy.proto # T0: TenantService, AdmissionService
│   ├── glidex/directory/v1/connectors.proto # T1: ConnectorService
│   ├── glidex/auth/v1/login.proto    # LoginService (public)
│   ├── glidex/common/v1/common.proto # shared: Arch, Hypervisor, Condition, Event
│   ├── glidex/storage/v1/storage.proto # G2: CredentialService, ImageService, DiskService
│   ├── glidex/vm/v1/vm.proto         # G2: VmService
│   ├── glidex/…/v1/*.proto           # tenancy, directory, auth; vm, storage, … as ported (§9)
│   └── google/rpc/error_details.proto # vendored ErrorInfo only (excluded from buf lint)
└── src/lib.rs            # generated modules + OPERATIONS
```

`build.rs`:
1. compiles every `.proto` with `protox` into one `FileDescriptorSet`
   (imports included);
2. checks it: every RPC has an `op` option with a `kind`; JSON names
   follow G-D11; no response message (transitively) has a field marked
   `secret`;
3. writes `operations.rs`: `pub static OPERATIONS: &[Operation]`
   (procedure path, Cedar action, kind, REST twin, streaming shape);
4. hands the descriptor set to `connectrpc-build`
   (`Config::descriptor_set`) for message types, service traits and
   clients.

Rules:
- Services are named `<Noun>Service` (buf `STANDARD` lint).
- Ids are `string` (UUIDs as today). Timestamps are
  `google.protobuf.Timestamp`; durations `google.protobuf.Duration`.
- Partial updates use `google.protobuf.FieldMask` (`update_mask`); the
  REST `PATCH` of the same operation maps present JSON fields to the
  mask.
- Optimistic concurrency uses a `version` field on the resource, as
  site policies already do.
- Lists: `page_size`, `page_token` → `next_page_token`.
- Secret fields appear only in request messages and are marked
  `[(glidex.api.v1.secret) = true]`; the audit layer never logs them.
- `buf lint` (`STANDARD`) and `buf breaking --against
  '.git#branch=main,subdir=crates/glidex-api/proto'` (`FILE`) run in
  CI; `buf generate` writes the UI's TypeScript into
  `crates/glidex-ui/ui/src/gen/`, committed, and CI fails if it is
  stale.

## 5. Operation table

Each RPC carries a method option:

```protobuf
rpc CreateTenant(CreateTenantRequest) returns (Tenant) {
  option (glidex.api.v1.op) = {
    action: "createTenant"   // Cedar action, or "public" / "authenticated"
    kind: WRITE              // READ | WRITE (G-D6)
    rest: "POST /tenants"    // the REST route of the same operation, or "" if none
  };
}
```

`build.rs` turns these into a static table
(`glidex_api::OPERATIONS: &[Operation]`). Tests:
- every RPC has an `op` with an action in the Cedar schema (the
  existing `every_route_maps_to_a_schema_action` test covers both
  tables);
- every op with a `rest` value has exactly that route registered in
  `api::routes`, with the same action;
- every REST route of a ported family (§9) has an RPC, except the
  HTTP-only list (§1).

## 6. Authentication and authorization

RPCs pass through the same `authenticate` layer as REST, so they get
the same `Principal` from the same credentials, the same `Origin`
check, and the same CSRF rule for cookies (every RPC is a `POST`, so
cookie-authenticated RPCs always need `x-glidex-csrf`).

On top of that, the RPC adapter (`src/api/rpc/`) enforces per listener:

| Listener | Protocols | Credentials |
|---|---|---|
| TCP `:8841` | gRPC, Connect | `authorization: Bearer gxt_…` |
| `api.sock` | gRPC, Connect | peer uid (security.md §5.2) |
| `ui.sock`, relayed UI (`Relay`) | gRPC-Web only | session cookie (+ CSRF) or bearer token |
| cluster `:8842` (forwarded) | as forwarded | `x-glidex-principal`, trusted only from a server certificate (clustering.md §12.3) |

A session cookie on any other combination is refused with
`UNAUTHENTICATED` (`reason: cookie_not_accepted`). Then the adapter
looks the procedure up in `OPERATIONS`, builds the `Call` (today's
`api::Caller`: principal, the operation's Cedar action, request id,
audit slot), runs the service function, and writes the audit entry
from the operation's `kind` and the `ServiceError` code (an RPC's HTTP
status says nothing about its outcome). Step-up denial is
`UNAUTHENTICATED` with reason `reauth_required`. Request ids come from
the layer as for REST and are returned in `x-request-id`.

## 7. Errors

| `ServiceError.code` (REST status) | gRPC status |
|---|---|
| `unauthenticated`, `login_failed`, `reauth_required`, `cookie_not_accepted` (401) | `UNAUTHENTICATED` |
| `forbidden`, `login_not_allowed`, `tls_required`, `ticket_required`, `tenant_suspended`, `protocol_not_allowed` (403) | `PERMISSION_DENIED` |
| `quota_exceeded`, `tenant_quota_exceeded` (403) | `RESOURCE_EXHAUSTED` |
| `not_found`, `oidc_disabled` (404) | `NOT_FOUND` |
| `conflict` (409: a stale `version`, a name already taken, a concurrent change) | `ABORTED` |
| other 409s: `would_lock_out`, `confirmation_required`, `in_use`, `managed_by_connector`, `sync_blocked`, `not_clustered`, `dpdk_not_running`, … (any code not listed falls back by REST status) | `FAILED_PRECONDITION` |
| `bad_request`, `invalid*` (400), `invalid_policy` (422) | `INVALID_ARGUMENT` |
| `unsupported_on_host` (422) | `FAILED_PRECONDITION` |
| `too_large` (413) | `RESOURCE_EXHAUSTED` |
| `rate_limited`, `slow_down` (429) | `RESOURCE_EXHAUSTED` (with `RetryInfo`) |
| `authorization_pending` (202, device poll) | `FAILED_PRECONDITION` (the RFC 8628 meaning, as `reason`) |
| `not_leader`, `use_a_server` (421) | `UNAVAILABLE` (client retries on another server) |
| `cluster_unavailable`, `leader_changed`, `unavailable`, `too_many_watchers`, `netd_unavailable`, … (503) | `UNAVAILABLE` |
| `idp_error`, `node_unreachable`, `netd_error`, `bad_gateway` (502) | `UNAVAILABLE` |
| `internal`, `persistence_error`, anything unmapped (500) | `INTERNAL` (generic message; details only in the audit log) |

The REST status stays what it is today; the table only adds the gRPC
side.

Every error carries `google.rpc.ErrorInfo { reason: <code>, domain:
"glidex" }` as a Connect/gRPC error detail and, when the REST error has
`details`, a `glidex.api.v1.ErrorDetails { json }` holding the same JSON
text. A test
walks every `ServiceError` code and fails if either mapping is
missing.

## 8. Cluster

- **Forwarding** (clustering.md §6.4): `gate::cluster_gate` decides
  read vs write from the operation's `kind` instead of `is_safe`. A
  unary write on a follower is forwarded whole over `:8842` (already
  HTTP/2) with `x-glidex-principal` and `forwarded_by` as today, plus
  the RPC protocol headers; the leader's reply comes back with its
  trailers (where native gRPC carries `grpc-status`). Streaming RPCs
  are not forwarded; they are reads served locally.
- **Reads** keep the ReadIndex wait; `x-glidex-consistency: local`
  metadata replaces the header of the same name.
- **Agents** forward gRPC calls to a server like REST calls; agent-local
  host operations (relay, `PreAuthorized`) are ported with the
  host-networking family (§9, G3), not before.
- Mixed versions: an RPC unknown to an older server returns
  `UNIMPLEMENTED`; `gxctl` falls back to REST for that family during an
  upgrade.

## 9. Families and order

| Family | Services | Milestone |
|---|---|---|
| Tenancy, directory, login | `tenancy.v1`, `directory.v1`, `auth.v1` | G1, built with tenancy.md T0–T1 |
| VMs, disks, credentials, images | `vm.v1`, `storage.v1` | G2 |
| Projects, teams, users, tokens, bindings, policies, audit, usage | `access.v1`, `usage.v1` | G2 |
| Networks, routers, shares, host networking, PCI, nodes, cluster | `network.v1`, `host.v1`, `cluster.v1` | G3 |
| Watch, VM events, console | `watch.v1` server stream; console bidi stream | G4 |

Porting a family means: move its handler logic into `src/service/`
(tenancy.md §14.1), keep the REST adapter's JSON identical (the
existing API tests are the check), add the gRPC adapter. A family is
ported whole, never half.

### 9.1 Removing the hand-written types

When a family is ported:
1. Its messages are written in proto, matching today's JSON where the
   proto3 JSON mapping allows it (same field names, `optional` for
   fields that are omitted today).
2. Where the mapping can't match, the proto form wins and the
   difference is listed in rest-api.md under the family. The known
   ones: 64-bit integers are JSON strings (so sizes and counts that fit
   use `uint32`; byte counts and timestamps-as-integers stay `uint64`
   and become strings), enum values are their full proto names
   (`PHASE_RUNNING`, where REST says `running` today), and
   `Timestamp` is RFC 3339. Unset fields are left out where REST sent
   `null`; every reader indexing JSON sees the same thing. A field REST
   always sends, even at its default (`"oidc": false`), is declared
   `optional` and always set, because proto3 JSON leaves out default
   values of fields without presence.
3. For **one release** the REST adapter also accepts the old request
   shape (the few value conversions above, done in the adapter before
   decoding), and responses carry `Glidex-Api-Shape: proto` so clients
   can tell. gxctl and the UI move to generated clients in the same
   change.
4. The hand-written types, their serde attributes and gxctl's mirrors
   are deleted in that change; the family's REST tests are rewritten
   against the generated types.

When every family is ported (G5), `models.rs` keeps only internal
store records, and nothing in `src/api/` defines a request or
response type.

**Watch** keeps the SSE semantics (`api/watch.rs`): a snapshot of
visible objects, `synced`, then `added`/`modified`/`deleted`, each
re-filtered by the caller's current rights; the stream ends after
`MAX_LIFETIME` and the client reconnects. **Console** moves to a bidi
stream only after the WebSocket path (tickets, shim relay) has an
equivalent test suite; until then it stays WebSocket.

## 10. gxctl

- `gxctl` uses the generated `connectrpc` clients from `glidex-api` for
  ported families, REST for the rest. They speak the **Connect protocol**
  (binary protobuf) through `client::RpcTransport`, which sends them over
  gxctl's own HTTP/1.1 connection: Connect's unary calls are plain POSTs,
  so `api.sock` with peer identity, the TLS trust ladder and pins, and the
  token all work unchanged. Streaming families will need gRPC over
  HTTP/2 (G4). The profile file, the TLS trust ladder, fingerprint pins and
  `token_command` ([gxctl-auth.md](gxctl-auth.md)) are unchanged: the
  gRPC channel is built from the same `glidex_tls::ClientTls`.
- Local `gxctl` talks gRPC over `api.sock` (h2c), so peer identity works
  as it does for REST.
- `GLIDEX_TRANSPORT=rest` forces REST, for debugging and mixed-version
  clusters.

## 11. Browser

- **Client.** `glidex-ui` uses Connect-ES clients generated by `buf
  generate` (`@bufbuild/protoc-gen-es`), with
  `createGrpcWebTransport({ baseUrl: "/api", credentials: "include" })`
  and an interceptor that sets `x-glidex-csrf`. Families not yet ported
  keep using the REST client.
- **Path.** Browser → `glidex-ui` (`/api/…`) → `ui.sock` → control
  plane. gRPC-Web puts trailers in the body, so the proxy only passes
  `application/grpc-web*` bodies through unbuffered, as it already does
  for SSE.
- **Server.** Nothing extra: `connectrpc` answers gRPC-Web. §6 limits
  `ui.sock` to it, and keeps it off the other listeners.
- **Browser protections stay** (security.md §5.6): the Origin check,
  CSRF for cookie-authenticated calls, no CORS (same origin through the
  UI). Console stays a WebSocket with tickets (§9).

## 12. Testing

- Operation table tests (§5), error-mapping test (§7), secret-field
  build check (§4).
- Tenancy API tests run on both transports (tenancy.md §17); each later
  family adds the same `{rest, grpc}` table when ported.
- Auth: cookies refused on native gRPC; gRPC-Web with a cookie
  accepted only on `ui.sock` with a valid Origin and `x-glidex-csrf`;
  bearer token on TCP; peer uid on
  `api.sock`; `x-glidex-principal` ignored except from a server
  certificate; step-up gives `UNAUTHENTICATED`/`reauth_required`.
- Cluster: a gRPC write on a follower reaches the leader once and is
  audited with `forwarded_by`; a READ op is never forwarded.
- Limits: oversize message → `RESOURCE_EXHAUSTED`; deadline honored.
- Compatibility: `buf lint`, `buf breaking` against `main`, and a
  stale-generated-TypeScript check in CI.
- Shape: each ported family's REST tests run against the proto JSON
  form and, for its transition release, the old request shape.

## 13. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **G0** scaffold | `glidex-api` crate (protox → `connectrpc-build`, build checks, operation table), `buf.yaml` + CI job, RPC routes in `api::router`, the RPC adapter (§6), `ServiceError` with both mappings, health, forwarding by `kind` with trailers, a first `SystemService`: `GetServerInfo` (REST twin `GET /auth/server-info`, same JSON, both served by one service function) and `WhoAmI` (RPC only; `GET /auth/whoami` moves onto its message with the access family in G2) | `System` round-trips over gRPC, Connect and REST; listener/credential, error-mapping and operation-table tests. Lands before tenancy.md T0. |
| **G1** tenancy | tenancy, directory, login services (tenancy.md §14) | Both-transport tenancy tests. |
| **G2** VMs, storage, access | as §9 | Existing REST suites unchanged; `{rest, grpc}` tables. |
| **G3** networking, host, cluster | as §9, including relay and `PreAuthorized` | Host tests on a real cluster. |
| **G4** streams and UI | watch, events as server streams; Connect-ES clients in the UI for every ported family; console after its test suite | Watch tests on both transports; UI e2e on gRPC-Web. |
| **G5** one type system | last hand-written REST types removed (§9.1); transition shapes dropped | No request/response type outside `glidex-api`; REST tests on proto JSON only. |

## 14. Resolved questions

1. **Browser path:** gRPC-Web through Connect-ES (G-D8).
2. **Proto tooling:** `buf` for lint, breaking checks and TypeScript;
   `protox` + `connectrpc-build` in `build.rs` for Rust (G-D9).
3. **Runtime:** `connectrpc` instead of tonic (G-D10).
4. **Hand-written REST types:** removed as each family is ported, REST
   moves to the proto JSON form with a one-release transition (G-D3,
   §9.1).

## 15. Implementation status

| Spec | Code |
|---|---|
| Protos, build checks (op option, G-D11 names, secrets), operation table (§4, §5) | `crates/glidex-api` (`build.rs`, `proto/`, `buf.yaml`) |
| Mounting, per-listener protocols and cookies, audit, error mapping (§3, §6, §7) | `crates/glidex-control-plane/src/api/rpc/` |
| Service layer (`Call`, `ServiceError`) | `crates/glidex-control-plane/src/service/` |
| `SystemService` (G0) | `service/system.rs`, `api/rpc/system.rs`; REST twin `api/access.rs::server_info` |
| `TenantService`, `AdmissionService` (T0) | `service/tenancy.rs`, `api/rpc/tenancy.rs`; REST twins `api/tenants.rs` |
| `LoginService` (G1's last service) | `service/login.rs`, `api/rpc/login.rs`; REST twins in `api/access.rs`; public operations get the request's listener and address from the adapter |
| `ConnectorService` (T1) | `service/directory.rs`, `api/rpc/directory.rs`; REST twins `api/connectors.rs`; `SetSecretRequest.value` is the first field marked `secret` |
| Storage family (G2): `CredentialService`, `ImageService`, `DiskService` | protos `glidex/storage/v1/storage.proto`, shared `glidex/common/v1/common.proto`; `service/storage.rs` (with `service/common.rs`); RPC `api/rpc/storage.rs`; REST twins `api/storage.rs` (`Proto`, `proto_body` in `api/mod.rs` for the shape header and the old request values); tests `tests/storage_tests.rs` |
| VM family (G2): `VmService` | proto `glidex/vm/v1/vm.proto`; `service/vm.rs`; RPC `api/rpc/vm.rs`; REST twins `api/vms.rs` (with the console WebSocket, `?view=full` and `/system/reconcile`, which stay REST); `PATCH` reads `{change, update_mask}` and, for one release, the old merge patch; tests `tests/vm_tests.rs` |
| Forwarding by `kind`, trailers (§8) | `api/gate.rs`, `cluster/net.rs` (`Reply.trailers`) |
| CI (G-D9) | `.github/workflows/rust.yml` job `proto` |
| Tests | `tests/rpc_tests.rs`; `cluster_tests.rs::rpc_reads_are_answered_by_the_server_that_takes_them`; unit tests in `api/rpc` and `glidex-api` |

Forwarding a **write** RPC with its trailers is covered since T0
(`cluster_tests.rs::rpc_writes_on_a_follower_reach_the_leader_with_their_trailers`,
which fails if the forward drops trailers), and gxctl's generated clients
over api.sock by `gxctl` `rpc_clients_over_the_unix_socket`. Not yet
exercised: reflection, the TypeScript generation check (starts with G4,
when the UI first uses Connect-ES).


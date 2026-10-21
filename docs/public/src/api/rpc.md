# RPC API

Every operation in the [REST API](rest.md) that has a proto message is
also an RPC. Same port, same credentials, same authorization and audit:
the RPC and its REST twin run the same code.

## Protocols

| Protocol | Where | Notes |
|---|---|---|
| gRPC | `:8841` (HTTP/2 over TLS), `api.sock` (HTTP/2 without TLS) | Binary protobuf |
| Connect | `:8841`, `api.sock` | Unary calls are plain `POST`s over HTTP/1.1 or HTTP/2, with `application/proto` or `application/json` bodies; easy to call with `curl`. Server streams work over HTTP/1.1 too; the console's bidirectional stream needs HTTP/2 |
| gRPC-Web | The web UI only | Refused on `:8841` and `api.sock` (`PERMISSION_DENIED`, reason `protocol_not_allowed`) |

All three are answered by the same server on the same port as REST;
there is no separate RPC port. Procedures live at
`/glidex.<area>.v1.<Service>/<Method>`, for example
`/glidex.vm.v1.VmService/ListVms`.

Methods are unary except three streams, below. Limits: 4 MiB per
message; a client deadline (`grpc-timeout`, `connect-timeout-ms`) on a
unary call is honored up to 5 minutes.

## Authentication

| Listener | Metadata |
|---|---|
| `:8841` | `authorization: Bearer <token>` |
| `api.sock` | None: you are the Unix user you connect as |

Session cookies are refused on RPCs outside the web UI
(`UNAUTHENTICATED`, reason `cookie_not_accepted`): use a token. Public
methods (`SystemService.GetServerInfo` and the `LoginService` methods)
need no credential. See [API overview](README.md#authentication).

A token from a workload federation's exchange is sent the same way. The
exchange itself, `POST /auth/exchange`, is REST only: it has no RPC
([Token exchange](rest.md#token-exchange-for-workloads)).

Optional metadata:

- `x-glidex-consistency: local`: in a cluster, read from the server you
  reached without waiting for it to catch up with the leader.

Responses carry `x-request-id`.

## Errors

Errors carry a gRPC status and, as an error detail, a
`google.rpc.ErrorInfo` with `domain: "glidex"`, `reason` set to the
same stable code the REST API returns in `error`
([codes](rest.md#errors)), and `metadata.http_status` set to the status
REST would have answered (`"422"`, say). When the REST error has
`details`, a `glidex.api.v1.ErrorDetails` detail holds them as JSON
text in `json`. Branch on `reason`, not on the message.

A Connect error with a JSON body looks like this (the detail's `value`
is the encoded `ErrorInfo`):

```json
{
  "code": "not_found",
  "message": "VM not found: <vm-id>",
  "details": [{"type": "google.rpc.ErrorInfo", "value": "<base64>"}]
}
```

| Status | Reasons |
|---|---|
| `UNAUTHENTICATED` | `unauthenticated`, `login_failed`, `reauth_required`, `cookie_not_accepted` |
| `PERMISSION_DENIED` | `forbidden`, `login_not_allowed`, `tls_required`, `ticket_required`, `tenant_suspended`, `origin_not_allowed`, `csrf_failed`, `protocol_not_allowed`, `not_admitted`, … |
| `RESOURCE_EXHAUSTED` | `quota_exceeded`, `tenant_quota_exceeded`, `insufficient_storage`, `too_large`, `rate_limited`, `slow_down` |
| `NOT_FOUND` | `not_found`, `oidc_disabled` |
| `ABORTED` | `conflict` (a name taken, a concurrent change): read again and retry |
| `FAILED_PRECONDITION` | Other `409` codes (`ambiguous`, `would_lock_out`, `confirmation_required`, `managed_by_connector`, `not_provisioned`, `not_pulled`, `no_source`, `sync_running`, …), `unsupported_on_host` |
| `INVALID_ARGUMENT` | `400` and `422` codes: `bad_request`, `invalid`, `invalid_*`, `issuer_unreachable` |
| `UNAVAILABLE` | `not_leader`, `use_a_server`, `leader_changed`, `cluster_unavailable`, and `502`/`503` codes (`netd_unavailable`, `node_unreachable`, `too_many_watchers`, `console_unavailable`, …) |
| `INTERNAL` | `internal`, `persistence_error` and other `500` codes. The message is generic and names the request id; the details are in the server log |
| `UNIMPLEMENTED` | A method this server doesn't have (an older version during an upgrade) |

`precondition_failed` (a VM changed since you read it: a stale
`expected_resource_version`) has no status of its own yet and arrives as
`INTERNAL`, with a generic message. Its `ErrorInfo` still has the reason,
`http_status` `"412"` and, in `ErrorDetails`, the current
`resource_version`: check the reason before treating `INTERNAL` as a
server fault.

## Streams

| Method | Kind | REST twin |
|---|---|---|
| `WatchService.Watch` | Server stream | `GET /watch` |
| `WatchService.WatchEvents` | Server stream | None |
| `VmService.Console` | Bidirectional stream | The console WebSocket |

Opening a stream is authorized and audited like a unary call: a refusal
is the call's error, before any message. Over native gRPC that error
arrives with its status but without its `ErrorInfo`; use the Connect
protocol for streams if you need the reason. Streams are served by the
server you reach and are not forwarded to the leader.

**`Watch`** sends the same events as `GET /watch`
([Live changes](rest.md#live-changes-get-watch)). The request takes
`kinds` (`WATCH_KIND_VM`, `…_DISK`, `…_IMAGE`, `…_NETWORK` by default;
also `…_NODE`, `…_IMPORT`, `…_CLUSTER`, `…_POLICY`, `…_DATASTORE`) and `project`. Each
`WatchResponse` has a `type` (`WATCH_EVENT_TYPE_ADDED`, `…_MODIFIED`,
`…_DELETED`, `…_SYNCED`, `…_EXPIRED`), `kind`, `id` and the object in a
`oneof` (`vm`, `disk`, `image`, `network`, `node`, `import_plan`,
`cluster`, `policy`, `datastore`). After 15 s without an event the server sends
`WATCH_EVENT_TYPE_KEEPALIVE`, so proxies keep a quiet stream open;
ignore it. The stream ends after 5 minutes with `…_EXPIRED`: open a new
one for a fresh snapshot.

**`WatchEvents`** follows one VM, disk, image or network (`kind`, `id`):
the events it has (as its events list answers them), `…_SYNCED`, then
each new event as it happens, with keep-alives and expiry as on `Watch`.
It needs read access to the object.

**`Console`** is the VM's serial console both ways. The first request
must be `{open: {id}}`, within 30 s; then send keystrokes as
`{input: <bytes>}`. Responses are `{output: <bytes>}`: the replayed
console log, then the guest's output. Closing your side of the stream
ends the call. A browser session would need `open.ticket`
(`CreateConsoleTicket`), but browsers use the WebSocket: gRPC-Web can't
stream both ways. `gxctl` uses this RPC over the Connect protocol.

## Health

`grpc.health.v1.Health/Check` is served on every listener without a
credential, like `GET /health`. It is answered by the server you reach.

## Getting the protos

On the host, server reflection (`grpc.reflection.v1` and `v1alpha`) on
`api.sock` lets tools such as `grpcurl` and `buf curl` list and call
methods without the proto files. It is offered on `api.sock` only:
`:8841` answers `404`.

Elsewhere, take the protos from the repository:
[crates/glidex-api/proto](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-api/proto).
Packages are versioned (`v1`); a breaking change would come as a `v2`
package served next to `v1`. Field names and numbers are the
compatibility contract, and JSON names are the `snake_case` field names.

Generate clients with [buf](https://buf.build). A `buf.gen.yaml` for
TypeScript (Connect-ES), next to a checkout of the repository:

```yaml
version: v2
inputs:
  - directory: glidex/crates/glidex-api/proto
plugins:
  - remote: buf.build/bufbuild/es
    out: src/gen
    opt:
      - target=ts
```

```bash
git clone https://github.com/CMKL-APEX/glidex.git
buf generate
```

Other languages work the same way with their protobuf and gRPC or
Connect plugins (managed mode can set package options such as
`go_package`, which the protos don't declare).
<!-- TODO(verify): the buf.gen.yaml above, and the buf curl and remote grpcurl commands below, were not run while writing this page. Reflection on api.sock, Connect with curl and a Connect server stream were checked against a running server. -->

## Examples

Connect with `curl`, JSON in and out:

```bash
curl --cacert tls.crt \
  -H "Authorization: Bearer <token>" \
  -H 'Content-Type: application/json' \
  -d '{}' \
  https://host.example.org:8841/glidex.system.v1.SystemService/WhoAmI

curl --cacert tls.crt \
  -H "Authorization: Bearer <token>" \
  -H 'Content-Type: application/json' \
  -d '{"project": "lab"}' \
  https://host.example.org:8841/glidex.vm.v1.VmService/ListVms
```

On the host, through the socket:

```bash
curl --unix-socket /run/glidex-cp/api.sock \
  -H 'Content-Type: application/json' -d '{}' \
  http://localhost/glidex.system.v1.SystemService/WhoAmI
```

`grpcurl` on the host's socket, with reflection:

```bash
grpcurl -plaintext -unix /run/glidex-cp/api.sock list
grpcurl -plaintext -unix /run/glidex-cp/api.sock describe glidex.vm.v1.VmService
grpcurl -plaintext -unix -d '{}' \
  /run/glidex-cp/api.sock glidex.system.v1.SystemService/WhoAmI
```

Remotely, give the tools the protos. `buf curl`:

```bash
buf curl --schema glidex/crates/glidex-api/proto \
  --cacert tls.crt \
  -H "Authorization: Bearer <token>" \
  -d '{"id": "<vm-id>", "wait_secs": 60}' \
  https://host.example.org:8841/glidex.vm.v1.VmService/StartVm
```

`grpcurl`:

```bash
grpcurl -cacert tls.crt \
  -import-path glidex/crates/glidex-api/proto -proto glidex/vm/v1/vm.proto \
  -H 'authorization: Bearer <token>' \
  -d '{}' \
  host.example.org:8841 glidex.vm.v1.VmService/ListVms
```

Write requests take the same fields as the REST body; the REST query
parameters become fields too (`?wait=60` is `wait_secs: 60`,
`If-Match` is `expected_resource_version`).

## Services

Every method, by package, with the REST operation it shares. Methods are
unary unless marked as a stream. REST paths and RPC requests take the
same ids and names.

### `glidex.system.v1`

| Service | Method | REST twin |
|---|---|---|
| `SystemService` | `GetServerInfo` | `GET /auth/server-info` |
|  | `WhoAmI` | `GET /auth/whoami` |

### `glidex.auth.v1`

| Service | Method | REST twin |
|---|---|---|
| `LoginService` | `Discover` | `POST /auth/discover` |
|  | `StartDevice` | `POST /auth/c/{id}/device` |
|  | `PollDevice` | `POST /auth/c/{id}/device/poll` |
|  | `PasswordToken` | `POST /auth/token` |
|  | `InspectInvitation` | `POST /auth/invitation` |

### `glidex.vm.v1`

| Service | Method | REST twin |
|---|---|---|
| `VmService` | `ListVms` | `GET /vms` |
|  | `CreateVm` | `POST /vms` |
|  | `PreviewPlacement` | `POST /scheduler/preview` |
|  | `GetVm` | `GET /vms/{id}` |
|  | `UpdateVm` | `PATCH /vms/{id}` |
|  | `DeleteVm` | `DELETE /vms/{id}` |
|  | `StartVm` | `POST /vms/{id}/start` |
|  | `PlaceVm` | `POST /vms/{id}/place` |
|  | `StopVm` | `POST /vms/{id}/stop` |
|  | `PauseVm` | `POST /vms/{id}/pause` |
|  | `ListVmEvents` | `GET /vms/{id}/events` |
|  | `GetConsoleInfo` | `GET /vms/{id}/console` |
|  | `CreateConsoleTicket` | `POST /vms/{id}/console/ticket` |
|  | `GetConsoleLog` | `GET /vms/{id}/console/log` |
|  | `Console` (bidi stream) | `GET /vms/{id}/console/ws` |
|  | `AttachDevice` | `POST /vms/{id}/devices` |
|  | `DetachDevice` | `DELETE /vms/{id}/devices` |
|  | `AttachDisk` | `POST /vms/{id}/disks` |
|  | `DetachDisk` | `DELETE /vms/{id}/disks/{disk}` |

### `glidex.storage.v1`

| Service | Method | REST twin |
|---|---|---|
| `CredentialService` | `ListCredentials` | `GET /credentials` |
|  | `CreateCredential` | `POST /credentials` |
|  | `GetCredential` | `GET /credentials/{username}` |
|  | `UpdateCredential` | `PUT /credentials/{username}` |
|  | `DeleteCredential` | `DELETE /credentials/{username}` |
| `ImageService` | `GetImageCatalog` | `GET /images/catalog` |
|  | `GetFirmwareCatalog` | `GET /images/firmware-catalog` |
|  | `ListImages` | `GET /images` |
|  | `GetImage` | `GET /images/{id}` |
|  | `PullImage` | `POST /images` |
|  | `DeleteImage` | `DELETE /images/{id}` |
|  | `RetryImage` | `POST /images/{id}/retry` |
|  | `AddImageCopy` | `POST /images/{id}/copies` |
|  | `RemoveImageCopy` | `DELETE /images/{id}/copies/{node}` |
|  | `ListImageEvents` | `GET /images/{id}/events` |
|  | `ExtractVmKernel` | `POST /vms/{id}/extract-kernel` |
| `DiskService` | `ListDisks` | `GET /disks` |
|  | `GetDisk` | `GET /disks/{id}` |
|  | `CreateDisk` | `POST /disks` |
|  | `ResizeDisk` | `POST /disks/{id}/resize` |
|  | `ExtendRoot` | `POST /disks/{id}/extend-root` |
|  | `DeleteDisk` | `DELETE /disks/{id}` |
|  | `ListDiskEvents` | `GET /disks/{id}/events` |
|  | `MoveDisk` | `POST /disks/{id}/move` |
| `DatastoreService` | `ListDatastores` | `GET /datastores` |
|  | `GetDatastore` | `GET /datastores/{id}` |
|  | `CreateDatastore` | `POST /datastores` |
|  | `UpdateDatastore` | `PATCH /datastores/{id}` |
|  | `DeleteDatastore` | `DELETE /datastores/{id}` |

### `glidex.network.v1`

| Service | Method | REST twin |
|---|---|---|
| `NetworkService` | `ListNetworks` | `GET /networks` |
|  | `CreateNetwork` | `POST /networks` |
|  | `GetNetwork` | `GET /networks/{id}` |
|  | `DeleteNetwork` | `DELETE /networks/{id}` |
|  | `ListNetworkEvents` | `GET /networks/{id}/events` |
|  | `GetNetworkOvn` | `GET /networks/{id}/ovn` |
|  | `GrantNetwork` | `PUT /networks/{id}/grants` |
|  | `CreateProjectNetwork` | `POST /projects/{id}/networks` |
|  | `SetNetworkRouter` | `PUT /projects/{id}/networks/{network}/router` |
| `NetworkShareService` | `OfferNetworkShare` | `POST /networks/{id}/shares` |
|  | `UnshareNetwork` | `DELETE /networks/{id}/shares/{project}` |
|  | `ListNetworkShares` | `GET /projects/{id}/network-shares` |
|  | `AcceptNetworkShare` | `POST /projects/{id}/network-shares/{network}/accept` |
|  | `LeaveNetworkShare` | `DELETE /projects/{id}/network-shares/{network}` |
| `RouterService` | `ListRouters` | `GET /projects/{id}/routers` |
|  | `GetRouter` | `GET /projects/{id}/routers/{router}` |
|  | `CreateRouter` | `POST /projects/{id}/routers` |
|  | `DeleteRouter` | `DELETE /projects/{id}/routers/{router}` |

### `glidex.host.v1`

| Service | Method | REST twin |
|---|---|---|
| `HostNetworkService` | `GetOvsStatus` | `GET /ovs/status` |
|  | `InstallOvs` | `POST /ovs/install` |
|  | `InitDpdk` | `POST /ovs/dpdk-init` |
|  | `ListBridges` | `GET /ovs/bridges` |
|  | `CreateBridge` | `POST /ovs/bridges` |
|  | `DeleteBridge` | `DELETE /ovs/bridges/{name}` |
|  | `ListUplinks` | `GET /ovs/bridges/{name}/uplinks` |
|  | `EnsureUplink` | `POST /ovs/bridges/{name}/uplinks` |
|  | `DeleteUplink` | `DELETE /ovs/bridges/{name}/uplinks/{uplink}` |
|  | `CommitUplink` | `POST /ovs/bridges/{name}/uplinks/{uplink}/commit` |
|  | `ReapplyUplink` | `POST /ovs/bridges/{name}/uplinks/{uplink}/reapply` |
| `PciService` | `ListPciDevices` | `GET /pci-devices` |
| `TopologyService` | `GetHostTopology` | `GET /host/topology` |

### `glidex.usage.v1`

| Service | Method | REST twin |
|---|---|---|
| `UsageService` | `GetUsage` | `GET /usage` |
|  | `GetProjectUsage` | `GET /projects/{id}/usage` |
|  | `GetVmUsage` | `GET /vms/{id}/usage` |
|  | `GetDiskUsage` | `GET /disks/{id}/usage` |
|  | `GetBandwidthReport` | `GET /usage/bandwidth` |
|  | `GetDiskIoReport` | `GET /usage/disk-io` |
|  | `GetComputeReport` | `GET /usage/compute` |
| `MetricsService` | `GetVmBandwidth` | `GET /vms/{id}/bandwidth` |
|  | `GetVmIo` | `GET /vms/{id}/io` |
|  | `GetVmCompute` | `GET /vms/{id}/compute` |
|  | `GetVmStats` | `GET /vms/{id}/stats` |
|  | `GetDiskIo` | `GET /disks/{id}/io` |
|  | `GetNetworkBandwidth` | `GET /networks/{id}/bandwidth` |
|  | `GetNetworkStats` | `GET /networks/{id}/stats` |

### `glidex.access.v1`

| Service | Method | REST twin |
|---|---|---|
| `ProjectService` | `ListProjects` | `GET /projects` |
|  | `CreateProject` | `POST /projects` |
|  | `GetProject` | `GET /projects/{id}` |
|  | `UpdateProject` | `PATCH /projects/{id}` |
|  | `DeleteProject` | `DELETE /projects/{id}` |
|  | `ListProjectBindings` | `GET /projects/{id}/bindings` |
|  | `AddProjectBinding` | `POST /projects/{id}/bindings` |
|  | `RemoveProjectBinding` | `DELETE /projects/{id}/bindings/{link}` |
| `UserService` | `ListUsers` | `GET /users` |
|  | `CreateUser` | `POST /users` |
|  | `UpdateUser` | `PATCH /users/{id}` |
|  | `DeleteUser` | `DELETE /users/{id}` |
|  | `LinkIdentity` | `POST /users/{id}/identities` |
|  | `UnlinkIdentity` | `DELETE /users/{id}/identities/{key}` |
|  | `UpdateMe` | `PATCH /users/me` |
|  | `GetMySshKeys` | `GET /users/me/ssh-keys` |
| `TeamService` | `ListTeams` | `GET /teams` |
|  | `CreateTeam` | `POST /teams` |
|  | `UpdateTeam` | `PATCH /teams/{id}` |
|  | `DeleteTeam` | `DELETE /teams/{id}` |
|  | `AddTeamMember` | `PUT /teams/{id}/members/{user}` |
|  | `RemoveTeamMember` | `DELETE /teams/{id}/members/{user}` |
| `TokenService` | `ListTokens` | `GET /tokens` |
|  | `CreateToken` | `POST /tokens` |
|  | `RevokeToken` | `DELETE /tokens/{id}` |
| `SystemBindingService` | `ListSystemBindings` | `GET /system/bindings` |
|  | `AddSystemBinding` | `POST /system/bindings` |
|  | `RemoveSystemBinding` | `DELETE /system/bindings/{link}` |
| `PolicyService` | `ListPolicies` | `GET /authz/policies` |
|  | `GetPolicy` | `GET /authz/policies/{id}` |
|  | `ListPolicyVersions` | `GET /authz/policies/{id}/versions` |
|  | `PutPolicy` | `PUT /authz/policies/{id}` |
|  | `DeletePolicy` | `DELETE /authz/policies/{id}` |
|  | `ValidatePolicy` | `POST /authz/validate` |
|  | `SimulatePolicy` | `POST /authz/simulate` |
|  | `ReloadPolicies` | `POST /authz/reload` |
| `AuditService` | `ReadAudit` | `GET /audit` |
| `AuthzService` | `Check` | `POST /authz/check` |
|  | `Allowed` | `POST /authz/allowed` |

### `glidex.tenancy.v1`

| Service | Method | REST twin |
|---|---|---|
| `TenantService` | `ListTenants` | `GET /tenants` |
|  | `GetTenant` | `GET /tenants/{id}` |
|  | `CreateTenant` | `POST /tenants` |
|  | `UpdateTenant` | `PATCH /tenants/{id}` |
|  | `SuspendTenant` | `POST /tenants/{id}/suspend` |
|  | `ResumeTenant` | `POST /tenants/{id}/resume` |
|  | `DeleteTenant` | `DELETE /tenants/{id}` |
|  | `ListMembers` | `GET /tenants/{id}/members` |
|  | `AddMember` | `POST /tenants/{id}/members` |
|  | `RemoveMember` | `DELETE /tenants/{id}/members/{principal}` |
|  | `ListBindings` | `GET /tenants/{id}/bindings` |
|  | `AddBinding` | `POST /tenants/{id}/bindings` |
|  | `RemoveBinding` | `DELETE /tenants/{id}/bindings/{link}` |
|  | `ListDevicePools` | `GET /tenants/{id}/device-pools` |
|  | `SetDevicePools` | `PUT /tenants/{id}/device-pools` |
| `AdmissionService` | `ListAdmissions` | `GET /admissions` |
|  | `SetAdmission` | `PUT /admissions/{tenant}` |
|  | `RemoveAdmission` | `DELETE /admissions/{tenant}` |

### `glidex.directory.v1`

| Service | Method | REST twin |
|---|---|---|
| `ConnectorService` | `ListConnectors` | `GET /connectors` |
|  | `GetConnector` | `GET /connectors/{id}` |
|  | `CreateConnector` | `POST /connectors` |
|  | `UpdateConnector` | `PATCH /connectors/{id}` |
|  | `DeleteConnector` | `DELETE /connectors/{id}` |
|  | `SetSecret` | `PUT /connectors/{id}/secrets/{field}` |
|  | `TestConnector` | `POST /connectors/{id}/test` |
|  | `SyncConnector` | `POST /connectors/{id}/sync` |
|  | `ListScimTokens` | `GET /connectors/{id}/scim-tokens` |
|  | `CreateScimToken` | `POST /connectors/{id}/scim-tokens` |
|  | `RevokeScimToken` | `DELETE /connectors/{id}/scim-tokens/{token}` |
| `FederationService` | `ListFederations` | `GET /tenants/{id}/federations` |
|  | `GetFederation` | `GET /tenants/{id}/federations/{federation}` |
|  | `CreateFederation` | `POST /tenants/{id}/federations` |
|  | `UpdateFederation` | `PATCH /tenants/{id}/federations/{federation}` |
|  | `DeleteFederation` | `DELETE /tenants/{id}/federations/{federation}` |
| `InvitationService` | `ListInvitations` | `GET /tenants/{id}/invitations` |
|  | `CreateInvitation` | `POST /tenants/{id}/invitations` |
|  | `RevokeInvitation` | `DELETE /invitations/{id}` |
| `SigningKeyService` | `ListSigningKeys` | `GET /directory/keys` |
|  | `RotateSigningKey` | `POST /directory/keys/rotate` |

### `glidex.cluster.v1`

| Service | Method | REST twin |
|---|---|---|
| `ClusterService` | `InitCluster` | `POST /cluster/init` |
|  | `JoinCluster` | `POST /cluster/join` |
|  | `ImportJoin` | `POST /cluster/import` |
|  | `RejoinCluster` | `POST /cluster/rejoin` |
|  | `GetClusterStatus` | `GET /cluster/status` |
|  | `CreateJoinToken` | `POST /cluster/join-tokens` |
|  | `PromoteNodes` | `POST /cluster/promote` |
|  | `DissolveCluster` | `POST /cluster/dissolve` |
|  | `LeaveCluster` | `POST /cluster/leave` |
|  | `RotateCa` | `POST /cluster/rotate-ca` |
|  | `RotateSecretsKey` | `POST /cluster/rotate-secrets-key` |
|  | `GetOvnSettings` | `GET /cluster/settings/ovn` |
|  | `PutOvnSettings` | `PUT /cluster/settings/ovn` |
|  | `GetEdgeDefaults` | `GET /cluster/settings/ovn/edge-defaults` |
|  | `GetOvnOverview` | `GET /cluster/ovn` |
|  | `GetOvnTunnels` | `GET /cluster/ovn/tunnels` |
| `NodeService` | `ListNodes` | `GET /nodes` |
|  | `GetNode` | `GET /nodes/{id}` |
|  | `DrainNode` | `POST /nodes/{id}/drain` |
|  | `UndrainNode` | `POST /nodes/{id}/undrain` |
|  | `ConfigureNodeOvn` | `PUT /nodes/{id}/ovn` |
|  | `DeleteSpareImageFiles` | `POST /nodes/{id}/spare-image-files/delete` |
|  | `RemoveNode` | `POST /nodes/{id}/remove` |
|  | `ForgetNode` | `POST /nodes/{id}/forget` |
|  | `PurgeNode` | `POST /nodes/{id}/purge` |
|  | `CreateRejoinToken` | `POST /nodes/{id}/rejoin-token` |
|  | `DetachNode` | `POST /nodes/{id}/detach` |
|  | `AbortDetach` | `POST /nodes/{id}/detach` with `abort` |
|  | `ForgetDeparted` | `POST /nodes/{id}/detach` with `departed` |
|  | `ListDepartures` | `GET /departures` |
|  | `GetNodeTopology` | `GET /nodes/{id}/topology` |
|  | `PrepareDevice` | `POST /nodes/{id}/devices/{address}/prepare` |
|  | `ReleaseDevice` | `POST /nodes/{id}/devices/{address}/release` |
|  | `CordonDevice` | `POST /nodes/{id}/devices/{address}/cordon` |
|  | `UncordonDevice` | `POST /nodes/{id}/devices/{address}/uncordon` |
|  | `SetSriovVfs` | `POST /nodes/{id}/devices/{address}/sriov` |
|  | `ListPciPools` | `GET /pci-pools` |
| `ImportService` | `ListImports` | `GET /imports` |
|  | `GetImport` | `GET /imports/{plan}` |
|  | `ApproveImport` | `POST /imports/{plan}/approve` |
|  | `RejectImport` | `POST /imports/{plan}/reject` |

### `glidex.watch.v1`

| Service | Method | REST twin |
|---|---|---|
| `WatchService` | `Watch` (server stream) | `GET /watch` |
|  | `WatchEvents` (server stream) | None |


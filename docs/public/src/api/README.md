# API overview

Glidex has one API with two transports:

- **REST**: JSON over HTTPS, for `curl`, scripts and anything that
  doesn't want generated code. See [REST API](rest.md).
- **RPC**: gRPC and the Connect protocol, with typed clients generated
  from the protos. See [RPC API](rpc.md).

Both run the same operation, with the same authorization, quotas and
audit entry, and almost every operation exists on both. The streams do
too: `GET /watch` (server-sent events) is `WatchService.Watch`, and the
console WebSocket is `VmService.Console`.

- **Only on REST**: `/health`, `/auth/methods`, browser login and
  logout, OIDC and connector login redirects, the browser's accounts
  (`/auth/accounts`), the [passkey ceremonies](rest.md#passkeys), the
  [token exchange](rest.md#token-exchange-for-workloads), a directory's
  OpenID endpoints (`/d/{directory}/…`), CSV exports, the cluster
  snapshot, `/system/reconcile`, the [SCIM
  endpoints](rest.md#scim-20-for-identity-providers) identity providers
  call, and the [endpoints](rest.md#trust-between-organizations)
  clusters and directories of different organizations call on each
  other.
- **Only on RPC**: the gRPC health service, server reflection (on
  `api.sock`) and `WatchService.WatchEvents` (one object's events as
  they happen).

The web UI and `gxctl` use this same API.

## Which one to use

| You want | Use |
|---|---|
| A quick call from a shell, or a script in any language | REST |
| A typed client (Go, TypeScript, Python, Rust, …) with compatibility checks | RPC |
| Live change events | Either: server-sent events, or a server stream |
| The serial console | A WebSocket from a browser; `VmService.Console` from any HTTP/2 client |
| Cluster and node management | Either, or [`gxctl`](../reference/gxctl.md) |

Where an operation exists on both, its request and response are the same
message: the REST body is the proto3 JSON form of the RPC message, with
`snake_case` field names, and REST errors carry the same codes as RPC
errors.

## Endpoints

| Listener | Address | Serves | Credential |
|---|---|---|---|
| HTTPS | `https://host.example.org:8841` (every address of the host by default) | REST, gRPC, Connect | Bearer token |
| Unix socket | `/run/glidex-cp/api.sock` | REST, gRPC, Connect, gRPC server reflection | Your Unix user |
| Web UI | `https://host.example.org:5173` | The browser app, which proxies its own API calls | Session cookie |

Port 8842 is the cluster port, used between nodes over mutual TLS; it is
not a client endpoint. In a cluster every server answers the API, and
writes are forwarded to the leader for you. A cluster agent has no API
of its own: point clients at a server.

The control plane serves HTTPS with a self-signed certificate unless
one is configured, and prints its SHA-256 fingerprint at start. Copy
`/run/glidex-cp/tls.crt` from the host and give it to your client
(`curl --cacert`, `GLIDEX_CA_CERT` for `gxctl`), or configure a
certificate from your CA. See [TLS](../operator-guide/tls.md).

## Authentication

| Where | How you are identified |
|---|---|
| HTTPS (`:8841`) | `Authorization: Bearer <token>` |
| `api.sock` | By the Unix user you connect as (peer uid); no credential is sent. Members of `glidex-users` are ordinary users; root and members of `glidex-admin` are administrators. Anyone else gets `403` |
| Web UI | A session cookie set at login; writes also carry the `X-Glidex-CSRF` header. Cookies are for the browser only: other clients use a token |

Get a token with `gxctl token create <name>` (the secret is shown
once), with `gxctl auth login`, or from the web UI. See
[Logging in](../user-guide/logging-in.md) and
[Authentication](../operator-guide/authentication.md).

A few operations need no credential: `GET /health`, `GET /auth/methods`,
`GET /auth/server-info`, the login endpoints and a directory's public
OpenID endpoints. The SCIM endpoints take only their connector's SCIM
token, and the endpoints between organizations a signed peer request. Everything else is
authorized against the caller's roles and the site's Cedar policies
([Access control](../operator-guide/access-control.md)). A missing
permission is `403 forbidden`; a resource in a project you can't read
is `404 not_found`; some sensitive operations ask for a recent login
(`401 reauth_required`).

Every response carries an `x-request-id` header. Quote it when you
report a problem: the audit log and the server log record it.

## Reference

The endpoint and method tables in [REST API](rest.md) and
[RPC API](rpc.md) are compiled from the server's route table and the
protos. A reference generated from the protos (an OpenAPI document for
REST; services, methods and messages per package for RPC) will replace
them. Until then the protos are the authority on message fields:
[crates/glidex-api/proto](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-api/proto).

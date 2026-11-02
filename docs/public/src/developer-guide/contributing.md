# Contributing

This page covers building and running Glidex from source, the test
layers, what CI checks, the conventions the code and docs follow, a
walk-through of adding an API operation, and the pull request flow.

## What you need

| For | You need |
|---|---|
| Building and `cargo test` | Linux, a current stable Rust (the workspace is edition 2024), a C linker. The tests also use `dosfstools`, `mtools`, `qemu-utils`, `gdisk` and `cloud-guest-utils` (what CI installs). |
| The web UI | [bun](https://bun.sh). The project uses bun, not npm, for the UI, its e2e suite and the generated TypeScript. |
| Protos | Nothing for the Rust side (`build.rs` compiles them). [`buf`](https://buf.build) for lint, breaking checks and the UI's TypeScript. |
| Real VMs | KVM, Cloud Hypervisor and/or QEMU, the UEFI firmware, a cloud image; `glidex-netd` and Open vSwitch for networked VMs. A host set up by `glidex-install` has all of these. |
| The docs | [mdBook](https://rust-lang.github.io/mdBook/) (`cargo install mdbook --locked`; CI uses 0.5.4). |

## Building and running from source

```bash
cargo build                      # every crate
cargo build -p glidex-control-plane --bin gxctl
```

On a host where the installer runs Glidex as services, stop them first:
they use the same ports.

```bash
sudo systemctl stop glidex-control-plane glidex-ui
```

### A control plane

A control plane you start yourself keeps its data in `~/.glidex` and its
sockets in `$XDG_RUNTIME_DIR/glidex` (`api.sock`, `ui.sock`). It runs
VMs detached, with the `glidex-vm-shim` binary found next to its own, so
build both:

```bash
cargo build --bin glidex-control-plane --bin glidex-vm-shim
cargo run --bin glidex-control-plane     # API on https://localhost:8841
```

It reads `/etc/glidex/control-plane.json`, or the file in
`GLIDEX_CONFIG`; a missing file means the defaults. On an installed
host, give your run its own file, so you are its administrator and its
UI user:

```bash
mkdir -p ~/glidex-dev
cat > ~/glidex-dev/control-plane.json <<EOF
{
  "listen": ["127.0.0.1:8841"],
  "admin_group": "$(id -gn)",
  "users_group": "$(id -gn)",
  "ui_user": "$(id -un)"
}
EOF
GLIDEX_CONFIG=~/glidex-dev/control-plane.json cargo run --bin glidex-control-plane
```

`admin_group` makes members of that group break-glass administrators on
`api.sock`; `ui_user` is the only user `ui.sock` accepts. Every setting
and its default is in
[`packaging/control-plane.json.example`](https://github.com/CMKL-APEX/glidex/blob/main/packaging/control-plane.json.example)
and [Configuration](../operator-guide/configuration.md). The UI e2e
suite's
[`start-control-plane.sh`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-ui/e2e/scripts/start-control-plane.sh)
is a complete example of a scratch instance with its own home, run
directory and datastore root.

<!-- TODO(verify): run the recipe above on a fresh installed host (data in ~/.glidex vs the scratch HOME, networking through glidex-netd). -->

### The web UI

```bash
(cd crates/glidex-ui/ui && bun install)
GLIDEX_API_SOCKET=$XDG_RUNTIME_DIR/glidex/ui.sock \
  cargo run -p glidex-ui -- --dev        # Vite with hot reload on https://localhost:5173
```

`--dev` runs `bun run dev` in `crates/glidex-ui/ui` with the UI's
certificate, and points the Vite proxy at the control plane. The UI's
RPCs are gRPC-Web, which the control plane takes only on `ui.sock`, so
the dev server should reach it there: without `GLIDEX_API_SOCKET` it
uses `/run/glidex-cp/ui.sock` if it exists, else TCP, where RPCs need a
bearer token.

To serve a production build instead: `bun run build` in
`crates/glidex-ui/ui` (it type-checks with `tsc -b`, then writes
`ui/dist`), then `cargo run -p glidex-ui`.

### gxctl

```bash
cargo run --bin gxctl                    # interactive shell; `help` lists commands
cargo run --bin gxctl -- list            # one command
```

On the host, `gxctl` finds a hand-started control plane's `api.sock` in
`$XDG_RUNTIME_DIR/glidex` and identifies you by your Unix account. See
the [gxctl reference](../reference/gxctl.md).

## Tests

| Layer | Where | Needs | Runs in CI |
|---|---|---|---|
| Unit tests | `#[cfg(test)]` modules in each crate | Nothing | Yes |
| Integration tests | `crates/*/tests/*.rs` | The disk tools above | Yes |
| Functional tests | `#[ignore]`d tests, mostly `crates/glidex-control-plane/tests/functional_tests.rs` | KVM, hypervisors, an image; some need `glidex-netd`, OVS, OVN, LVM | No |
| UI e2e | `crates/glidex-ui/e2e` (Playwright) | bun, Chromium; KVM and an image for the boot tests | No |
| Nested cluster e2e | `crates/glidex-e2e/tests/nested_cluster.rs` | A Glidex host with nested KVM, about 9 GiB of memory | No |
| Metadata endpoint e2e | `crates/glidex-e2e/tests/metadata.rs` | A Glidex host installed from your workspace (`glidex-metad`, netd's rules) with a NAT network | No |

### Unit and integration tests

```bash
cargo test                                        # the whole workspace, no VMs
cargo test -p glidex-control-plane --test datastore_tests   # one test binary
cargo test -p glidex-raftstore                    # one crate
GLIDEX_CHECK_WRITE_SETS=1 cargo test              # as CI runs it
```

`GLIDEX_CHECK_WRITE_SETS=1` replays every database a test opens from its
write sets and compares the result, which catches a write that a
replicated store couldn't reproduce.

The control plane's integration tests start the whole router in process
and call it as clients do; a test file's harness has `rest(…)` and
`grpc(…)` helpers so one test can check both transports. Networking
tests run against an in-process `glidex-netd` with a recording executor
(no root, no OVS). `cluster_tests.rs` runs three control planes on one
machine over real mutual TLS.

### Functional tests

These boot real guests. Run them one at a time:

```bash
GLIDEX_TEST_IMAGE=~/images/ubuntu-cloudimg.img \
  cargo test -p glidex-control-plane --test functional_tests -- --ignored --test-threads=1
```

| Variable or step | For |
|---|---|
| `GLIDEX_TEST_IMAGE` | A UEFI-bootable cloud image; most boot tests. |
| `GLIDEX_TEST_KERNEL`, `GLIDEX_TEST_INITRD` | Direct kernel boot tests. |
| The `glidex` group | Tests that use the host's `glidex-netd`. The installer keeps you out of that group; run them with `sudo -u $USER -g glidex env PATH=$PATH HOME=$HOME cargo test …`. |
| `GLIDEX_TEST_DPDK=1` | vhost-user tests, with OVS-DPDK. |
| `sudo scripts/dev/fake-lan-setup.sh`, `GLIDEX_TEST_LAN=1` | Uplink tests, on a fake LAN in a network namespace. Undo with `fake-lan-teardown.sh`. |

Other ignored tests, each documented in its file's header:

```bash
cargo test -p glidex-control-plane --test real_ovn -- --ignored           # ovsdb-server, ovn-nbctl
GLIDEX_TEST_NB=unix:/var/run/ovn/ovnnb_db.sock \
  cargo test -p glidex-ovn -- --ignored                                    # a throwaway OVN
GLIDEX_STORD_SOCKET=/run/glidex-stord/stord.sock GLIDEX_STORD_VG=<vg> GLIDEX_STORD_POOL=<pool> \
  cargo test -p glidex-stord --test real_lvm -- --ignored                  # a throwaway volume group
cargo test -p glidex-control-plane --test systemd_runner_tests -- --ignored  # user systemd, KVM, GLIDEX_TEST_IMAGE
```

`glidex-vm-shim`'s tests run against an idle Cloud Hypervisor and skip
themselves when it isn't installed.

### UI end-to-end tests

```bash
scripts/dev/ui-e2e.sh -i ~/images/ubuntu-cloudimg.img       # both hypervisors
scripts/dev/ui-e2e.sh -H ch                                 # the ui project plus one hypervisor
scripts/dev/ui-e2e.sh -- --headed -g "create form"          # extra Playwright arguments
scripts/dev/ui-e2e-full.sh -i ~/images/ubuntu-cloudimg.img  # fails if any test skips
```

The script installs the suite's dependencies and Playwright's Chromium
with bun, starts a throwaway control plane on `:8851`, the Vite dev
server on `:5174` and a production `glidex-ui` on `:5175`, and runs the
suite. Tests that need KVM, an image or `glidex-netd` skip when those
are missing. Only one run goes at a time on a machine: the script
refuses a second. Set `E2E_API_PORT` (and `E2E_UI_PORT`,
`E2E_UI_SERVER_PORT`) when something else holds those ports. Build
first (`cargo build -p glidex-control-plane -p glidex-ui`): the harness
times out while cargo builds. On a host with an install, run it in the
`glidex` group so the boot tests reach `glidex-netd`:

```bash
sudo -n -g glidex -u $USER env HOME=$HOME PATH=$PATH GLIDEX_E2E_GROUP=glidex-admin \
  scripts/dev/ui-e2e.sh -H ch -- tests/<spec>.spec.ts
```

Without the script, from `crates/glidex-ui/e2e`:

```bash
(cd ../ui && bun install) && bun install && bunx playwright install chromium
GLIDEX_TEST_IMAGE=~/images/ubuntu-cloudimg.img bun run test   # or test:ch / test:qemu
bun run typecheck
```

See the suite's
[README](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-ui/e2e/README.md)
for how it signs in and what each server is for.

### Nested cluster test

```bash
cargo test -p glidex-e2e --test nested_cluster -- --ignored --nocapture
```

It makes two VMs with this host's Glidex, installs Glidex from your
workspace into both (`glidex-install --prebuilt`), forms a cluster with
OVN, runs a guest on each and checks they reach each other over the
Geneve tunnel. It takes 10 to 20 minutes.

The metadata endpoint test runs against this host's own Glidex, which
must be installed from your workspace and serve a NAT network. From
inside a guest it reads `/v1/instance`, fetches an identity token and a
Glidex token, and checks that a request without `Metadata-Flavor: Glidex`
is refused:

```bash
cargo test -p glidex-e2e --test metadata -- --ignored --nocapture
```

## CI

Two workflows run on every pull request to `main` and every push to
`main`.

[`rust.yml`](https://github.com/CMKL-APEX/glidex/blob/main/.github/workflows/rust.yml):

| Job | Checks |
|---|---|
| `build` | `scripts/lint-store-writes.sh`; `cargo build`; `GLIDEX_CHECK_WRITE_SETS=1 cargo test`. Times out after 45 minutes. |
| `fmt` | `cargo fmt --all --check` |
| `proto` | In `crates/glidex-api`: `buf lint`, `buf format --exit-code`; `buf generate` leaves `crates/glidex-ui/ui/src/gen` unchanged; `buf breaking` against `origin/main`. |

[`docs.yml`](https://github.com/CMKL-APEX/glidex/blob/main/.github/workflows/docs.yml):

| Job | Checks |
|---|---|
| `public` | `mdbook build docs/public`, assembled with `docs/site` into the site artifact. |
| `deploy` | On pushes to `main` only: publishes the site to the `gh-pages` branch. |
| `internal` | `mdbook build docs/internal` (the specs); `cargo doc --workspace --no-deps --document-private-items` with rustdoc warnings as errors. |

CI does not run clippy, the ignored tests or the UI e2e suite: run the
ones your change touches before you push.

### Checks before pushing

Run these on every commit of a stack, not only its top, since each
stacked pull request is merged on its own (the same list as
[`AGENTS.md`](https://github.com/CMKL-APEX/glidex/blob/main/AGENTS.md)):

```bash
cargo fmt --all --check
cargo check --workspace --tests
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" \
  cargo doc --workspace --no-deps --document-private-items
GLIDEX_CHECK_WRITE_SETS=1 cargo test --workspace --no-fail-fast
scripts/lint-store-writes.sh
```

And when they apply:

| Changed | Also run |
|---|---|
| A `.proto` | In `crates/glidex-api`: `buf lint`, `buf format --exit-code`, `buf generate` (commit the regenerated `crates/glidex-ui/ui/src/gen`), `buf breaking --against '../../.git#branch=main,subdir=crates/glidex-api'`. |
| The web UI | `bun install` (after a package change) and `bun x tsc -b --noEmit` in `crates/glidex-ui/ui`; the UI e2e for the pages touched. |
| Docs | `mdbook build docs/public` (and `docs/internal` for `spec/`). |
| VM, disk, network or host paths | The `#[ignore]`d functional tests that cover them, on a host. |

The cluster tests are timing-sensitive: a failed `build` job may be
rerun once; a second failure is real.

## Conventions

### Specs first

A change to behavior, an invariant or a contract between components
starts in [`spec/`](https://github.com/CMKL-APEX/glidex/blob/main/spec/README.md).
Update the spec that owns it in the same pull request, or land a spec
pull request first. In specs:

- File paths are relative to the repository root; code references use
  `path:line`.
- A claim about runtime behavior that the source doesn't make obvious
  gets a `Why:` or `Invariant:` note.
- Decisions are numbered within their spec (`D6`, `G-D11`) and
  milestones named (`S5`, `T6`), so code and commits can cite them.

Code cites the spec it implements: a module's `//!` header names the
spec section (`//! The scheduler (spec/14-clustering.md §9.1)`), and a
comment that depends on a decision names it. Specs are never moved or
renamed, because code cites them by path.

### Rust

- Formatting is rustfmt's defaults: `cargo fmt --all`.
- Keep clippy clean: `cargo clippy --workspace --all-targets -- -D warnings`.
  When a lint must be allowed, allow it narrowly with the reason in a
  comment, as `api/mod.rs` does for `clippy::result_large_err`.
- Keep rustdoc clean: CI builds it with `-D warnings`.
- Every write to the control plane's database goes through the store
  (`Db::begin` / `Db::write` in `crates/glidex-control-plane/src/store/`),
  so every write has a write set a replicated store can propose.
  `scripts/lint-store-writes.sh` fails the build if `begin_write`
  appears anywhere else in the crate.
- Never block a Tokio worker on a lock held across a store write (the
  write lane, the network store's mutex, the image cache, the directory
  key lock) or on a slow blocking call (netd, stord, hyperstor). In a
  cluster a write holds the lane until Raft commits, and Raft needs the
  runtime's I/O. Wait through `glidex_raftstore::wait` (`lock`, `read`,
  `write`, `blocking`), which uses `block_in_place` on a worker.
- Operations are decided in `service/`, never in an adapter: REST
  handlers and RPC adapters only decode, call one service function and
  encode.

### Protos and JSON

- Packages are versioned, `glidex.<area>.v1`. A breaking change needs a
  new package served next to the old one; `buf breaking` (`FILE` rules)
  enforces it against `main`.
- `buf lint` with the `STANDARD` rules: services are `<Noun>Service`.
- Every RPC carries `option (glidex.api.v1.op)`; `build.rs` refuses a
  method without one.
- Every field whose name has an underscore declares
  `[json_name = "<name>"]`, so REST JSON stays `snake_case`;
  `build.rs` refuses it otherwise.
- Secrets are request-only fields marked
  `[(glidex.api.v1.secret) = true]`; `build.rs` refuses a response that
  reaches one, and the audit never logs them.
- REST bodies are the proto3 JSON form of the same messages: enum
  values take their full names (`HYPERVISOR_QEMU`), and an unknown
  field is refused with `422` and code `invalid`.
- After changing a proto, regenerate the UI's TypeScript and commit it:

  ```bash
  (cd crates/glidex-ui/ui && bun install)
  (cd crates/glidex-api && buf generate)
  ```

The full rules are in
[`spec/18-grpc.md`](https://github.com/CMKL-APEX/glidex/blob/main/spec/18-grpc.md)
§4.

### Docs voice

The public book is plain, terse and concrete: short sentences in the
present tense, sentence-case headings, tables for anything with more
than two attributes, a command for every task. It describes what Glidex
does, not how it might evolve. It doesn't depend on the specs (only this
developer guide links to them, always by full GitHub URL), and it
contains no security-sensitive details: no internal host names,
addresses, credentials or unreleased plans. Use placeholders such as
`host.example.org`, `<token>` and documentation addresses
(`192.0.2.0/24`). Benchmark figures name their source, date and
hardware.

## Adding an API operation end to end

This walk-through follows an existing operation, `MoveDisk` (`POST
/disks/{id}/move`), through every layer. To add your own, change the
same places.

### 1. Spec

Describe the operation in the spec that owns its area: what it does,
who may call it, what state it writes and which controller acts on it.
`MoveDisk` is in
[`spec/24-storage.md`](https://github.com/CMKL-APEX/glidex/blob/main/spec/24-storage.md)
§8.3.

### 2. Proto

Add the RPC and its messages to the area's proto, here
`crates/glidex-api/proto/glidex/storage/v1/storage.proto`:

```protobuf
// Move the disk to another datastore, offline (spec/24-storage.md §8.3):
// the node that has it copies it there, then removes the old file.
rpc MoveDisk(MoveDiskRequest) returns (MoveDiskResponse) {
  option (glidex.api.v1.op) = {
    action: "moveDisk"
    kind: KIND_WRITE
    rest: "POST /disks/{id}/move"
  };
}
```

```protobuf
message MoveDiskRequest {
  string id = 1;
  // Datastore id or name.
  string datastore = 2;
  optional string node = 3;
  optional bool flatten = 4;
  // Wait this long (at most 300 s) for the move to finish.
  optional uint32 wait_secs = 5 [json_name = "wait_secs"];
}
```

The option says three things:

- `action`: the Cedar action checked for the call, or `public` /
  `authenticated`.
- `kind`: `KIND_READ` operations are served by any server after a read
  barrier; `KIND_WRITE` operations run on the leader, and a follower
  forwards them.
- `rest`: the REST twin, exactly as registered in the route table, or
  empty for an RPC-only operation.

### 3. Build: the operation table

`cargo build -p glidex-api` regenerates the messages, the `DiskService`
trait and client, and `glidex_api::OPERATIONS`, which now has an entry
for `/glidex.storage.v1.DiskService/MoveDisk`. The build fails here if
the option is missing or malformed, or a `json_name` is wrong.

### 4. Service function

Write the operation once, in
[`service/storage.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/service/storage.rs).
It takes the `Call` (principal, Cedar action, audit) and the request
message, makes every decision, and returns a `ServiceResult` (comments
added):

```rust
pub async fn move_disk(call: &Call, req: pb::MoveDiskRequest) -> ServiceResult<DiskReply> {
    let d = visible_disk(call, &req.id).await?;               // not found vs forbidden
    let datastore = disk_datastore(call, Some(&req.datastore))?; // may the caller use it?
    call.manager()
        .move_disk(&d.id, &datastore, req.node.as_deref(), req.flatten.unwrap_or(false))
        .await
        .map_err(manager_err)?;
    disk_reply(call, &d.id, req.wait_secs).await
}
```

The manager checks the disk and records the move on it (`moving`); the disk controller
(`controller/disk.rs`) copies the disk and removes the old file, and
`wait_secs` lets the caller wait for it to settle.

### 5. REST adapter

Register the route with its Cedar action in `routes` in
[`api/mod.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/api/mod.rs):

```rust
.add(
    "POST",
    "/disks/{id}/move",
    "moveDisk",
    post(storage::move_disk),
)
```

and write the handler in `api/storage.rs`. It decodes the proto JSON
body (unknown fields refused), fills in path and query values, calls the
service function and encodes the reply:

```rust
pub async fn move_disk(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    Json(v): Json<Value>,
) -> Result<Response, ApiErr> {
    let mut req: pb::MoveDiskRequest = body(v)?;
    req.id = id;
    req.wait_secs = req.wait_secs.or(w.secs());
    let r = svc::move_disk(&c, req).await?;
    Ok(disk_reply(r, StatusCode::OK, StatusCode::ACCEPTED))
}
```

### 6. RPC adapter

Implement the generated trait method in `api/rpc/storage.rs`. Adapters
go through `unary`, which finds the operation in the table, applies the
listener's protocol and credential rules, builds the `Call`, runs the
service function and audits the result:

```rust
async fn move_disk(
    &self,
    ctx: RequestContext,
    req: ServiceRequest<'_, MoveDiskRequest>,
) -> ServiceResult<MoveDiskResponse> {
    disk_write!(self, ctx, req, svc::move_disk, MoveDiskResponse)
}
```

A new service (not just a new method) is also registered in the file's
`register` function.

### 7. Cedar

Declare the action in
[`policies/glidex.cedarschema`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/policies/glidex.cedarschema),
with its permission groups and the resource type it applies to:

```text
action deleteDisk, resizeDisk, extendRoot, moveDisk in ["disk.write", "tenant-scoped"]
  appliesTo { principal: [User, Token], resource: [Disk], context: Ctx };
```

The groups decide who gets it: `disk.write` is in `role.editor`, so
editors and every role above them may move disks with no change to
`roles.cedar`. Pick an existing group where one fits; a new group needs
a place in the role hierarchy at the top of the schema. Add `step-up`
for actions that need a recent login.

The test `every_route_maps_to_a_schema_action` in `api/mod.rs` checks
that every route and every RPC names an action the schema declares, and
that each RPC's REST twin is registered with the same action.

### 8. gxctl

Add the command in the area's file under `src/bin/gxctl/`, here
`storage.rs` (`disk move <disk> --datastore D [--node N] [--flatten]`),
calling the generated client:

```rust
let req = MoveDiskRequest {
    id: name.to_string(),
    datastore: ds.to_string(),
    node: flag_value(args, "--node").map(String::from),
    flatten: has_flag(args, "--flatten").then_some(true),
    wait_secs: Some(300),
    ..Default::default()
};
match svc.move_disk(req).await.map(|r| r.into_owned()) { … }
```

Update the usage string with it, and the subcommand list the shell's
Tab completion offers in `main.rs`.

### 9. Web UI

Regenerate the TypeScript (`buf generate`, above), then add a function
to `crates/glidex-ui/ui/src/api.ts` that calls the RPC with its proto
JSON request:

```ts
export const moveDisk = (id: string, datastore: string, flatten?: boolean) =>
  diskWrite(rpc(DiskService.method.moveDisk, { id, datastore, flatten: flatten || undefined, wait_secs: WAIT_SECS }));
```

Use it from the page (`pages/Disks.tsx`). Show or hide the control with
`useCan` from `session.tsx`, asking for the action on the right
resource type. A new host-wide capability also goes in
`HOST_ACTION_RESOURCE` there; `tests/ui_capabilities.rs` checks each one
against the schema's `appliesTo`.

### 10. Tests

- An integration test in the area's file (`datastore_tests.rs` has
  `disks_move_between_datastores`): the REST call, and the RPC through
  the harness's `grpc(…)` helper for anything the two transports could
  answer differently. Cover the forbidden and not-found cases.
- Unit tests next to the controller logic.
- A UI e2e test in `crates/glidex-ui/e2e/tests/` if the UI gained a
  flow.
- A functional test if it changes what happens to a real VM.

### 11. Docs

- The user-facing behavior in the user or operator guide (for disk
  moves: [Images and disks](../user-guide/images-and-disks.md) and
  [Datastores](../operator-guide/datastores.md)).
- The endpoint in [REST API](../api/rest.md), the RPC in [RPC
  API](../api/rpc.md), and the command in the [gxctl
  reference](../reference/gxctl.md). These are written by hand from the
  route table and the protos.
- The spec's implementation status, if it tracks one.

## Updating the docs

| Change | Where |
|---|---|
| How to use or operate a feature | `docs/public/src/` (this book) |
| Why the code is shaped this way, an invariant, a decision | `spec/` |
| The architecture or decisions, for contributors | `docs/public/src/developer-guide/` |
| A type or function | Its rustdoc comment |

When a design in `spec/` ships, write its user-facing part in the book
and link from the spec to the book, not the other way round. Pages in
the book are listed in `docs/public/src/SUMMARY.md`; the build doesn't
create missing pages, so add a page there when you add the file.

```bash
mdbook serve docs/public          # live preview on http://localhost:3000
mdbook build docs/public          # -> target/docs/public
mdbook build docs/internal        # the specs -> target/docs/internal
```

## Pull requests

1. Branch from `main`. Branch names say what kind of change it is:
   `feat/…`, `fix/…`, `spec/…`, `docs/…`, `refactor/…`.
2. Keep each commit one change, with a short summary line and a body
   that says what changed and why. Reference spec sections and
   decisions (`spec/24-storage.md §8.3`, `D71`).
3. A design change goes in a `spec/…` pull request, reviewed before or
   with the code that implements it. Say so when a pull request is
   stacked on another.
4. Describe the pull request with a summary, the changes and how you
   tested them, including any ignored or e2e tests you ran by hand.
5. CI must pass: build and tests, `fmt`, `proto` and the docs builds.
6. Several people and coding agents work on Glidex at once.
   [`AGENTS.md`](https://github.com/CMKL-APEX/glidex/blob/main/AGENTS.md)
   has the working rules they share (a worktree each, the checks on
   every commit of a stack, how stacks are merged), and
   [`spec/progress.md`](https://github.com/CMKL-APEX/glidex/blob/main/spec/progress.md)
   what is built, claimed and next; a pull request that starts or
   finishes a milestone updates it.

Contributions are dual-licensed MIT OR Apache-2.0, as the [README](https://github.com/CMKL-APEX/glidex/blob/main/README.md#contribution)
says.

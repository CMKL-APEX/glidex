# Developer guide

This guide is for people who want to change Glidex: fix a bug, add an
API operation, port a feature to clusters, or just find their way
around the code. It assumes you know Rust and have used Linux VMs; it
doesn't assume you know KVM, Open vSwitch or Raft.

To install and run Glidex, read the [Operator guide](../operator-guide/README.md)
instead; to use it, the [User guide](../user-guide/README.md).

## Glidex from a contributor's angle

Glidex is a Rust workspace of 16 crates. At its centre is one binary,
`glidex-control-plane`: an axum server that answers a REST API and an
RPC API (gRPC, gRPC-Web and Connect) through one transport-neutral
service layer, authorizes every call with Cedar, writes desired state
to a ReDB database, and runs level-triggered controllers that move the
host toward it. Each VM runs under its own small supervisor,
`glidex-vm-shim`, in its own systemd unit, so the control plane can
restart without touching guests. Privileged work (Open vSwitch and NAT,
LVM, PAM) is done by small root helpers that take requests only from
the control plane; guests reach a metadata endpoint, `glidex-metad`, for
their identity tokens. Several control planes form a cluster by replicating
their database with Raft; cluster networks run on OVN. The web UI
(React, served by `glidex-ui`) and the `gxctl` CLI are clients of the
same API, with types generated from the same protos.

## Pages

| Page | What it covers |
|---|---|
| [Repository and code organization](organization.md) | Every crate, which binary runs where, the control plane's modules, the protos and generated code, the web UI, and where to find things. |
| [Architecture](architecture.md) | Processes, layers and how a request and a VM flow through them. |
| [Design decisions and trade-offs](design-decisions.md) | Why Glidex is shaped the way it is, and what each choice costs. |
| [Algorithms](algorithms.md) | Reconciliation, scheduling, replication and the other non-obvious parts. |
| [Performance and benchmarks](benchmarks.md) | Measured figures, with their source, date and hardware. |
| [Glidex compared](comparison.md) | How Glidex relates to other VM managers. |
| [Contributing](contributing.md) | Building from source, tests, CI, conventions, adding an API operation end to end, and the pull request flow. |

## Three sets of documentation

| Set | Where | Read it for |
|---|---|---|
| This book | [`docs/public/src`](https://github.com/CMKL-APEX/glidex/blob/main/docs/public/src/SUMMARY.md), published at <https://cmkl-apex.github.io/glidex/docs/> | What Glidex does and how to use it; for contributors, this guide. |
| Design specs | [`spec/`](https://github.com/CMKL-APEX/glidex/blob/main/spec/README.md) | Invariants, contracts between components, decisions with their reasons, milestones. The authority on *why*; code and protos cite them as `spec/<file>.md §n`. |
| API reference (rustdoc) | Built locally with `cargo doc` | Every crate's types and functions, private items included. Module docs (`//!`) name the spec section each module implements. |

When to read which:

- **Before you change behavior**, read the spec that owns it. A comment
  such as `// spec/24-storage.md §8.3` or a decision number (`D6`, `G-D11`)
  points at the reasoning you must keep or update.
- **To learn what a feature does from the outside**, read this book's
  user and operator guides. They describe shipped behavior only.
- **To find your way in a module**, read its rustdoc: start at the
  `//!` header of the file.

Build the specs as a book with `mdbook build docs/internal` (output in
`target/docs/internal`), and the API reference with:

```bash
RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" \
  cargo doc --workspace --no-deps --document-private-items   # -> target/doc
```

## First day

1. Read [Concepts](../concepts.md) and the [Operator guide
   overview](../operator-guide/README.md): the objects and the systemd
   units you will be changing.
2. Install Glidex on a test host or VM with KVM ([Quick
   start](../quick-start.md)), create a NAT network, pull an image and
   boot a VM. You will want a working host for the functional tests.
3. Read [Repository and code organization](organization.md), then
   [Architecture](architecture.md).
4. Read the specs' [README](https://github.com/CMKL-APEX/glidex/blob/main/spec/README.md),
   then [`spec/00-architecture.md`](https://github.com/CMKL-APEX/glidex/blob/main/spec/00-architecture.md)
   and [`spec/12-reconciliation.md`](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md):
   desired state, the controllers and the shim.
5. Read [`spec/18-grpc.md`](https://github.com/CMKL-APEX/glidex/blob/main/spec/18-grpc.md)
   §2–§5 for how an API operation is declared once and served twice.
6. Build and test: `cargo build`, then `cargo test` ([Contributing](contributing.md#tests)).
7. Run a control plane and the UI from source and trace one request,
   for example `POST /disks/{id}/move`, from the UI's `api.ts` through
   `api/storage.rs` and `service/storage.rs` to the disk controller
   ([Adding an API operation](contributing.md#adding-an-api-operation-end-to-end)
   walks the same path).

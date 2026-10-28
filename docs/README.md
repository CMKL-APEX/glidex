# Documentation

Glidex has two sets of documentation, built separately:

| Set | Audience | Source | Contents |
|---|---|---|---|
| **Public** | People who install, use and operate Glidex | [`docs/public/`](public/) | Quick start, user guide, operator guide, public API reference |
| **Internal** | Contributors | [`spec/`](../spec/) (book config in [`docs/internal/`](internal/)) | Design specs, invariants, and the internal API reference (rustdoc of every crate, the full RPC surface) |

## What goes where

- **Public** pages describe *what Glidex does and how to use it*: tasks,
  commands, configuration, supported behavior. They must not depend on
  internal pages, and must not contain security-sensitive details
  (internal hostnames, credentials, attack-surface analysis, unreleased
  plans).
- **Internal** pages describe *why the code is shaped the way it is*:
  invariants, contracts between components, design decisions and
  milestones. They stay in `spec/` because code and protos cite them as
  `spec/<file>.md` (hundreds of references); don't move or rename them.
- When a design in `spec/` ships, write the user-facing part in
  `docs/public/` and link from the spec to it, not the other way round.
- The **developer guide** (`docs/public/src/developer-guide/`) is public
  too, but for contributors: it explains the architecture, decisions,
  algorithms and benchmark figures, and is the one part of the public
  book that may point into the specs and source, always as full GitHub
  URLs (`https://github.com/CMKL-APEX/glidex/blob/main/...`), never as
  relative links. Benchmark figures always name their source, date and
  hardware.

## Building

Both books use [mdBook](https://rust-lang.github.io/mdBook/)
(`cargo install mdbook --locked`).

```sh
mdbook build docs/public      # -> target/docs/public
mdbook build docs/internal    # -> target/docs/internal
mdbook serve docs/public      # live preview on http://localhost:3000

# Internal API reference: every crate, private items included;
# CI fails on rustdoc warnings.
RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" \
  cargo doc --workspace --no-deps --document-private-items   # -> target/doc
```

CI builds all three on every pull request
([`.github/workflows/docs.yml`](../.github/workflows/docs.yml)) and keeps
the output as workflow artifacts. On every push to `main` it publishes the
public site to the `gh-pages` branch: the landing page in
[`docs/site/`](site/) at the root and the public book under `docs/`
(<https://cmkl-apex.github.io/glidex/>, once GitHub Pages serves that
branch). The internal set is never published there; it must only ever go
to an access-controlled host.

## Still to do

- Public API reference: the endpoint and RPC tables in `docs/public/src/api/`
  are compiled by hand from the route table and the protos. Generate them
  instead (`protoc-gen-doc` through `buf generate` for RPC; an OpenAPI
  document from `glidex-api`'s `build.rs` for REST), limited to the
  operations we declare public.
- The `gxctl` reference is hand-written too: only gxctl's global options
  are clap; commands are parsed by hand, so there is nothing to generate
  it from yet.
- Resolve the `<!-- TODO(verify) -->` comments in `docs/public/src`.
- Publishing the internal set to an access-controlled host.

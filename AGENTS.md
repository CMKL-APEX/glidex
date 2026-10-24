# Working on Glidex (for coding agents and the people steering them)

Glidex is a control plane for KVM virtual machines (Cloud Hypervisor and
QEMU) on one host or a cluster, with its own directory, tenancy, Cedar
authorization, networking (OVS/OVN), storage and a web UI. Several
sessions, human or agent, work on it at once. This file is how they work
the same way and don't get in each other's way.

Read first, and don't repeat them here:

| For | Read |
|---|---|
| Building, running, the test layers, conventions, a worked example of adding an API operation | [docs/public/src/developer-guide/contributing.md](docs/public/src/developer-guide/contributing.md) |
| What each crate and module is | [developer-guide/organization.md](docs/public/src/developer-guide/organization.md) |
| How the pieces fit at run time, and why they are shaped so | [developer-guide/architecture.md](docs/public/src/developer-guide/architecture.md), [design-decisions.md](docs/public/src/developer-guide/design-decisions.md) |
| The design of an area: invariants, decisions, milestones, what is built | [spec/](spec/README.md), one file per area |
| What is built, in flight and next, across all areas | **[spec/progress.md](spec/progress.md)** |

## Ground rules

- **No secrets anywhere they can leak.** Never put a password, token,
  key, cookie or OAuth secret in the repository, a commit message, a PR,
  an issue, a chat, a command line or a log. Secrets go in through
  write-only UI fields, `--from-file`, or standard input; scripts keep
  non-secret settings in a 0600 file outside the repository. A secret
  that was shown is compromised: say so, and have it revoked. Real-IdP
  and real-host results ([spec/idp-checks.md](spec/idp-checks.md)) name
  no accounts and quote no tokens. Don't print a credential while
  checking one: pass it through files and stdin.
- **Ask before anything shared or hard to undo.** Merging a PR,
  installing on a host, changing a cluster's membership, deleting data,
  or anything visible to other people needs the go-ahead of whoever
  steers the session, each time; an approval covers what it named.
- **Report what happened.** A failing test, a skipped step or a check
  that couldn't run is said plainly, with its output. A flake is a flake
  only after a rerun passes; say which.

## How a change is made

1. **Find the work.** [spec/progress.md](spec/progress.md) lists each
   area's next milestone and what is in flight. Before starting, check
   that nobody has it: `gh pr list`, `git branch -r`, and the board's
   *In flight* table.
2. **Spec first** for anything that adds behaviour, an API or a
   decision. In the area's spec: the decisions, numbered on from the
   spec's last (`D71` follows `D70`; ids are per spec), the task list
   for the milestone, and open questions. Ask the person steering to
   settle open questions before code; record their answer as a decision.
   A bug fix or a refactor needs no spec change unless it changes a
   documented behaviour.
3. **Claim it.** Open the spec PR (or, for a small change, the first
   code PR) early; it adds the row to the board's *In flight* table.
4. **Tasks as stacked PRs.** One task, one PR, each building on the last
   (`spec/<area>-<topic>` → `feat/<area>-<task>` → …), each passing the
   checks below on its own. Name branches `spec/…`, `feat/…`, `fix/…`,
   `docs/…`, `refactor/…`.
5. **Each code PR updates the spec's status section** (a table of
   *Spec | Code*, *Tests* and *Not yet* rows; the section's name is on
   the board) and, when a milestone changes state, its row on the board.
   Docs for users and operators (`docs/public/`) change in the same PR as
   the behaviour.
6. **Real checks.** What only a real host or a real identity provider can
   show is written as a check in [spec/idp-checks.md](spec/idp-checks.md)
   (or the area's spec) and run on the hosts when the person steering
   agrees; the result row gives the date, the build and pass/fail.
7. **Finish.** When the PRs merge: the board's *In flight* row goes, the
   milestone's row moves on, branches and worktrees are deleted.

Commits: an imperative summary line under ~72 characters, then a body
that says what changed and why. PR descriptions: a summary, the tests
and what they cover, and what is not done yet.

## Checks before pushing

Run these on every commit of a stack, not only its top (a stacked PR is
merged on its own):

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
| A `.proto` | In `crates/glidex-api`: `buf lint`, `buf format --exit-code`, `buf generate` (commit the regenerated `crates/glidex-ui/ui/src/gen`), `buf breaking --against '../../.git#branch=main,subdir=crates/glidex-api'`. Changes are additive; a proto comment's `<id>` must be in backquotes (rustdoc rejects it bare). |
| The web UI | `bun install` (after a package change) and `bun x tsc -b --noEmit` in `crates/glidex-ui/ui`; the UI e2e for the pages touched (below). The project uses **bun, never npm**. |
| Docs | `mdbook build docs/public` (and `docs/internal` for spec/). |
| VM, disk, network or host paths | The `#[ignore]`d functional tests that cover them, on a host (contributing.md). |

CI runs fmt, the proto checks, build and tests, the docs and CodeQL; it
doesn't run clippy, so you do. The cluster tests are timing-sensitive:
a failed `build` job may be rerun once; a second failure is real.

UI e2e, on a host with the install (one run at a time per machine; the
script refuses a second):

```bash
cargo build -p glidex-control-plane -p glidex-ui   # first: the harness times out while building
sudo -n -g glidex -u $USER env HOME=$HOME PATH=$PATH GLIDEX_E2E_GROUP=glidex-admin \
  scripts/dev/ui-e2e.sh -H ch -- tests/<spec>.spec.ts
```

## Sessions working side by side

- **Your own worktree, always.** Shared checkouts (such as `~/glidex` on
  a dev host) belong to whoever is using them. Start with
  `git worktree add ../glidex-<topic> -b <branch> origin/main`, work and
  build there, and remove it when its PRs have merged.
- **Your own target directory.** Each worktree builds into its own
  `target/`. Never point `CARGO_TARGET_DIR` at another checkout's: cargo
  then mixes builds and runs the other checkout's binaries.
- **Small builds.** Dev hosts are shared: build with `-j2`. Disks are
  tight: delete `target/*/incremental` of a worktree you are done with,
  and the worktree itself after merging.
- **Stop only what you started.** Stop a background run by its own task
  or PID. Never `pkill`/`killall` by name (`pkill cargo` kills other
  sessions' builds), and remember `pgrep -f` over ssh matches its own
  shell.
- **Hosts are shared state.** The board's *Hosts* section says what each
  test host is and runs. Install or reconfigure one only when asked, and
  update that section in your PR when you do.
- **When main moves.** Rebase the stack onto `origin/main` (`git rebase
  --onto origin/main <old base> <top>`), take each commit's checks again
  (regenerate protos; `bun install` if the UI's packages changed), and
  force-push with `--force-with-lease`. A merged dependency of your
  stack may make a commit of yours redundant: drop it.
- **Merging, when asked.** In stack order: retarget the next PR to
  `main`, let CI run on it, rerun a failed `build` once, merge with a
  merge commit (not a squash), then delete the merged branches.

## Writing

- Specs and docs: plain words, short sentences, the reader's task first;
  say what the code does, not what it might do. A spec's status says
  only what is built and tested; a check not run is "not run yet".
- Code: match the surrounding code's comment density, naming and idiom.
  Comments say why, not what.
- Don't infer anyone's pronouns from a name; use *they*.

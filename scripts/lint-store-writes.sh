#!/usr/bin/env bash
# spec/14-clustering.md §17 C0: every change to the control-plane database goes
# through `Db::begin` / `Db::write` (crates/glidex-control-plane/src/store/),
# so every write has a write set that a replicated store can propose.
# glidex-netd's own database is host-local and is not covered, nor is the
# Raft log (src/cluster/raft/log_store.rs): it lives in its own file,
# raft/log.redb (§6.2), and is what writes are replicated through, so it
# can't itself go through a write set.
set -euo pipefail
cd "$(dirname "$0")/.."
bad=$(grep -rn "begin_write" crates/glidex-control-plane --include='*.rs' | grep -v -e '/src/store/' -e '/src/cluster/raft/log_store.rs:' || true)
if [ -n "$bad" ]; then
    echo "begin_write used outside crates/glidex-control-plane/src/store/:" >&2
    echo "$bad" >&2
    exit 1
fi

//! What the Raft log's writes are made of (spec/14-clustering.md §6.7, C9):
//! entries and bytes per table and origin, rewrites of an unchanged value,
//! and which fields each write of a record changed. Reads a copy of
//! `raft/log.redb` and reports only table and field names, counts and
//! sizes: never a key or a value.

use crate::cluster::raft::{LogStore, TypeConfig};
use crate::store::{Op, TableId};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

/// The fields a VM write may change and still be only bookkeeping (D27).
const VM_BOOKKEEPING: &[&str] = &[
    "meta.resource_version",
    "status.conditions",
    "status.last_reconciled_at",
];
/// D27: a VM's bookkeeping is written at most this often.
const VM_STAMP_SECS: u64 = 600;
/// Field paths are reported this deep (`status.conditions`, not the
/// condition's own fields), and a name that isn't an identifier shows as
/// `*`: a map keyed by names or ids gives none of them away.
const PATH_DEPTH: usize = 2;
/// Field sets reported per table.
const TOP_CHANGES: usize = 8;

#[derive(Default)]
struct TableTally {
    puts: u64,
    deletes: u64,
    bytes: u64,
    keys: HashSet<u64>,
    unchanged: u64,
    first_seen: u64,
    changes: HashMap<String, (u64, u64)>,
}

/// The running count over a log's entries, in index order.
#[derive(Default)]
pub struct Tally {
    first: Option<u64>,
    last: Option<u64>,
    entries: u64,
    bytes: u64,
    blank: u64,
    membership: u64,
    origins: BTreeMap<String, (u64, u64)>,
    tables: BTreeMap<TableId, TableTally>,
    /// The last value written at each key (by its hash) seen so far.
    values: HashMap<(TableId, u64), Vec<u8>>,
    vm_bookkeeping: u64,
    vm_bookkeeping_early: u64,
    vm_conditions: HashMap<String, (u64, u64)>,
    times: Option<(u64, u64)>,
}

fn hash(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn segment(name: &str) -> &str {
    let ident = !name.is_empty()
        && name.len() <= 40
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ident { name } else { "*" }
}

/// The field paths that differ between `a` and `b`, at most `PATH_DEPTH` deep.
fn changed_paths(a: &Value, b: &Value, prefix: &str, depth: usize, out: &mut BTreeSet<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) if depth < PATH_DEPTH => {
            let names: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for n in names {
                let (va, vb) = (
                    x.get(n).unwrap_or(&Value::Null),
                    y.get(n).unwrap_or(&Value::Null),
                );
                if va != vb {
                    let p = match prefix {
                        "" => segment(n).to_string(),
                        _ => format!("{prefix}.{}", segment(n)),
                    };
                    changed_paths(va, vb, &p, depth + 1, out);
                }
            }
        }
        _ if a != b => {
            out.insert(if prefix.is_empty() {
                "(value)".into()
            } else {
                prefix.into()
            });
        }
        _ => {}
    }
}

/// A condition's kind, as the code names them (`Ready`), or `*`.
fn kind_name(k: &str) -> &str {
    let ident = !k.is_empty()
        && k.len() <= 40
        && k.starts_with(|c: char| c.is_ascii_uppercase())
        && k.chars().all(|c| c.is_ascii_alphanumeric());
    if ident { k } else { "*" }
}

/// Which of a VM's conditions, by kind, changed and in which field:
/// `Ready.message`, `RestartRequired added`.
fn condition_changes(a: &Value, b: &Value, out: &mut BTreeSet<String>) {
    let by_kind = |v: &Value| -> BTreeMap<String, Value> {
        v.pointer("/status/conditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| Some((kind_name(c.get("kind")?.as_str()?).to_string(), c.clone())))
            .collect()
    };
    let (x, y) = (by_kind(a), by_kind(b));
    for k in x.keys().chain(y.keys()).collect::<BTreeSet<_>>() {
        match (x.get(k), y.get(k)) {
            (Some(_), None) => {
                out.insert(format!("{k} removed"));
            }
            (None, Some(_)) => {
                out.insert(format!("{k} added"));
            }
            (Some(p), Some(q)) if p != q => {
                let mut fields = BTreeSet::new();
                changed_paths(p, q, "", PATH_DEPTH - 1, &mut fields);
                for f in fields {
                    out.insert(format!("{k}.{f}"));
                }
            }
            _ => {}
        }
    }
}

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    path.iter().try_fold(v, |v, p| v.get(p))?.as_u64()
}

/// Times found in the values, for the span the log covers: VMs' last
/// reconciliation, and the ledger cursors' merge time (spec/13-metering.md).
fn note_time(times: &mut Option<(u64, u64)>, table: TableId, key: &[u8], v: &Value) {
    let t = match table {
        TableId::Vms => u64_at(v, &["status", "last_reconciled_at"]),
        TableId::LedgerInbox if key.starts_with(b"cursor/") => v.get(2).and_then(Value::as_u64),
        _ => None,
    };
    if let Some(t) = t.filter(|&t| t > 0) {
        let (lo, hi) = times.get_or_insert((t, t));
        *lo = (*lo).min(t);
        *hi = (*hi).max(t);
    }
}

impl Tally {
    pub fn add(&mut self, index: u64, size: usize, entry: &openraft::Entry<TypeConfig>) {
        self.first.get_or_insert(index);
        self.last = Some(index);
        self.entries += 1;
        self.bytes += size as u64;
        let ws = match &entry.payload {
            openraft::EntryPayload::Normal(ws) => ws,
            openraft::EntryPayload::Blank => return self.blank += 1,
            openraft::EntryPayload::Membership(_) => return self.membership += 1,
        };
        let o = self
            .origins
            .entry(format!("{:?}", ws.origin).to_lowercase())
            .or_default();
        o.0 += 1;
        o.1 += size as u64;
        for op in &ws.ops {
            self.op(op);
        }
    }

    fn op(&mut self, op: &Op) {
        let table = op.table();
        let key = hash(op.key());
        let t = self.tables.entry(table).or_default();
        t.keys.insert(key);
        let value = match op {
            Op::Delete { key: k, .. } => {
                t.deletes += 1;
                t.bytes += k.len() as u64;
                self.values.remove(&(table, key));
                return;
            }
            Op::Put { key: k, value, .. } => {
                t.puts += 1;
                t.bytes += (k.len() + value.len()) as u64;
                value
            }
        };
        let new: Option<Value> = serde_json::from_slice(value).ok();
        if let Some(v) = &new {
            note_time(&mut self.times, table, op.key(), v);
        }
        let Some(old) = self.values.insert((table, key), value.clone()) else {
            t.first_seen += 1;
            return;
        };
        if old == *value {
            t.unchanged += 1;
            return;
        }
        let mut paths = BTreeSet::new();
        match (serde_json::from_slice::<Value>(&old).ok(), &new) {
            (Some(a), Some(b)) => {
                changed_paths(&a, b, "", 0, &mut paths);
                if table == TableId::Vms && paths.contains("status.conditions") {
                    let mut conds = BTreeSet::new();
                    condition_changes(&a, b, &mut conds);
                    let c = self
                        .vm_conditions
                        .entry(conds.into_iter().collect::<Vec<_>>().join(" "))
                        .or_default();
                    c.0 += 1;
                    c.1 += value.len() as u64;
                }
            }
            _ => {
                paths.insert("(bytes)".to_string());
            }
        }
        if table == TableId::Vms && paths.iter().all(|p| VM_BOOKKEEPING.contains(&p.as_str())) {
            self.vm_bookkeeping += 1;
            let at = |v: &Value| u64_at(v, &["status", "last_reconciled_at"]).unwrap_or(0);
            let before = serde_json::from_slice::<Value>(&old)
                .map(|v| at(&v))
                .unwrap_or(0);
            let after = new.as_ref().map(at).unwrap_or(0);
            if after.saturating_sub(before) < VM_STAMP_SECS {
                self.vm_bookkeeping_early += 1;
            }
        }
        let c = t
            .changes
            .entry(paths.into_iter().collect::<Vec<_>>().join(" "))
            .or_default();
        c.0 += 1;
        c.1 += value.len() as u64;
    }

    pub fn report(&self) -> Report {
        let op_bytes: u64 = self.tables.values().map(|t| t.bytes).sum();
        let mut tables: Vec<TableRow> = self
            .tables
            .iter()
            .map(|(id, t)| {
                let changes = top_changes(&t.changes);
                TableRow {
                    table: id.name(),
                    puts: t.puts,
                    deletes: t.deletes,
                    bytes: t.bytes,
                    share: if op_bytes == 0 {
                        0.0
                    } else {
                        t.bytes as f64 / op_bytes as f64
                    },
                    keys: t.keys.len() as u64,
                    unchanged: t.unchanged,
                    first_seen: t.first_seen,
                    changes,
                }
            })
            .collect();
        tables.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.table.cmp(b.table)));
        Report {
            first_index: self.first,
            last_index: self.last,
            entries: self.entries,
            bytes: self.bytes,
            blank: self.blank,
            membership: self.membership,
            span_secs: self.times.map(|(lo, hi)| hi - lo),
            origins: self
                .origins
                .iter()
                .map(|(o, &(entries, bytes))| OriginRow {
                    origin: o.clone(),
                    entries,
                    bytes,
                })
                .collect(),
            tables,
            vm_bookkeeping: self.vm_bookkeeping,
            vm_bookkeeping_early: self.vm_bookkeeping_early,
            vm_conditions: top_changes(&self.vm_conditions),
        }
    }
}

fn top_changes(m: &HashMap<String, (u64, u64)>) -> Vec<ChangeRow> {
    let mut v: Vec<ChangeRow> = m
        .iter()
        .map(|(f, &(writes, bytes))| ChangeRow {
            fields: f.clone(),
            writes,
            bytes,
        })
        .collect();
    v.sort_by(|a, b| b.writes.cmp(&a.writes).then(a.fields.cmp(&b.fields)));
    v.truncate(TOP_CHANGES);
    v
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub first_index: Option<u64>,
    pub last_index: Option<u64>,
    /// Entries and their stored size.
    pub entries: u64,
    pub bytes: u64,
    pub blank: u64,
    pub membership: u64,
    /// From the earliest to the latest time found in the values; `None`
    /// when there were none.
    pub span_secs: Option<u64>,
    pub origins: Vec<OriginRow>,
    /// Largest first, by the bytes of their operations' keys and values.
    pub tables: Vec<TableRow>,
    /// VM writes that changed only bookkeeping (D27), and of those, the
    /// ones less than 10 minutes after the record's last reconciliation.
    pub vm_bookkeeping: u64,
    pub vm_bookkeeping_early: u64,
    /// VM writes that changed `status.conditions`: which condition, by
    /// kind, and which of its fields.
    pub vm_conditions: Vec<ChangeRow>,
}

#[derive(Debug, Serialize)]
pub struct OriginRow {
    pub origin: String,
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct TableRow {
    pub table: &'static str,
    pub puts: u64,
    pub deletes: u64,
    pub bytes: u64,
    pub share: f64,
    /// Distinct keys written: for `rate_5m`, subjects × hours.
    pub keys: u64,
    /// Puts of exactly the value the key already held.
    pub unchanged: u64,
    /// Puts to a key not written earlier in the log, so not compared.
    pub first_seen: u64,
    /// The field sets the other puts changed, most frequent first.
    pub changes: Vec<ChangeRow>,
}

#[derive(Debug, Serialize)]
pub struct ChangeRow {
    pub fields: String,
    pub writes: u64,
    pub bytes: u64,
}

/// Read the log file at `path` from index `from` on.
pub fn measure(path: &Path, from: u64) -> Result<Report, String> {
    let mut tally = Tally::default();
    LogStore::scan_file(path, from, |i, size, e| tally.add(i, size, &e))
        .map_err(|e| e.to_string())?;
    Ok(tally.report())
}

fn per_day(n: u64, span: Option<u64>) -> String {
    match span {
        Some(s) if s >= 3600 => format!("{:.0}/day", n as f64 * 86_400.0 / s as f64),
        _ => "-".into(),
    }
}

impl Report {
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let span = self.span_secs;
        let _ = writeln!(
            s,
            "Raft log, entries {} to {}: {} entries ({}), {} bytes ({}); {} blank, {} membership",
            self.first_index.map_or("-".into(), |i| i.to_string()),
            self.last_index.map_or("-".into(), |i| i.to_string()),
            self.entries,
            per_day(self.entries, span),
            self.bytes,
            per_day(self.bytes, span),
            self.blank,
            self.membership,
        );
        let _ = writeln!(
            s,
            "Time covered (from the values): {}",
            span.map_or("unknown".into(), |t| format!("{:.1} h", t as f64 / 3600.0))
        );
        let _ = writeln!(s, "\n{:<12} {:>9} {:>12}", "origin", "entries", "bytes");
        for o in &self.origins {
            let _ = writeln!(s, "{:<12} {:>9} {:>12}", o.origin, o.entries, o.bytes);
        }
        let _ = writeln!(
            s,
            "\n{:<22} {:>8} {:>7} {:>11} {:>6} {:>6} {:>9} {:>9} {:>10}",
            "table",
            "puts",
            "deletes",
            "bytes",
            "share",
            "keys",
            "per key",
            "unchanged",
            "first seen"
        );
        for t in &self.tables {
            let per_key = if t.keys == 0 {
                0.0
            } else {
                (t.puts + t.deletes) as f64 / t.keys as f64
            };
            let _ = writeln!(
                s,
                "{:<22} {:>8} {:>7} {:>11} {:>5.1}% {:>6} {:>9.1} {:>9} {:>10}",
                t.table,
                t.puts,
                t.deletes,
                t.bytes,
                t.share * 100.0,
                t.keys,
                per_key,
                t.unchanged,
                t.first_seen
            );
        }
        for t in self
            .tables
            .iter()
            .filter(|t| t.share >= 0.01 && !t.changes.is_empty())
        {
            let _ = writeln!(s, "\n{}: fields changed", t.table);
            for c in &t.changes {
                let _ = writeln!(
                    s,
                    "  {:>8} writes {:>11} bytes  {}",
                    c.writes, c.bytes, c.fields
                );
            }
        }
        let _ = writeln!(
            s,
            "\nVM writes of bookkeeping only (D27): {}, of which within 10 minutes of the last: {}",
            self.vm_bookkeeping, self.vm_bookkeeping_early
        );
        if !self.vm_conditions.is_empty() {
            let _ = writeln!(s, "\nvms: conditions changed");
            for c in &self.vm_conditions {
                let _ = writeln!(
                    s,
                    "  {:>8} writes {:>11} bytes  {}",
                    c.writes, c.bytes, c.fields
                );
            }
        }
        s
    }
}

/// `glidex-control-plane --store-writes <log.redb> [--since <index>] [--json]`.
pub fn run(args: &[String]) -> i32 {
    let mut path = None;
    let mut since = 0;
    let mut json = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--since" => match it.next().and_then(|v| v.parse().ok()) {
                Some(i) => since = i,
                None => {
                    eprintln!("--since needs a log index");
                    return 2;
                }
            },
            p if path.is_none() && !p.starts_with("--") => path = Some(p.to_string()),
            other => {
                eprintln!("unexpected argument {other:?}");
                return 2;
            }
        }
    }
    let Some(path) = path else {
        eprintln!(
            "usage: glidex-control-plane --store-writes <copy of raft/log.redb> [--since <index>] [--json]"
        );
        return 2;
    };
    match measure(Path::new(&path), since) {
        Ok(r) if json => {
            println!("{}", serde_json::to_string_pretty(&r).expect("serializes"));
            0
        }
        Ok(r) => {
            print!("{}", r.render());
            0
        }
        Err(e) => {
            eprintln!("reading {path}: {e} (run it on a copy: a running server holds the file)");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Origin, WRITE_SET_FORMAT, WriteSet};
    use serde_json::json;

    fn put(table: TableId, key: &str, v: Value) -> Op {
        Op::Put {
            table,
            key: key.as_bytes().to_vec(),
            value: serde_json::to_vec(&v).unwrap(),
        }
    }

    fn entry(i: u64, origin: Origin, ops: Vec<Op>) -> openraft::Entry<TypeConfig> {
        openraft::Entry {
            log_id: openraft::LogId::new(openraft::CommittedLeaderId::new(1, 1), i),
            payload: openraft::EntryPayload::Normal(WriteSet {
                format: WRITE_SET_FORMAT,
                origin,
                ops,
            }),
        }
    }

    fn vm(rv: u64, phase: &str, at: u64, cond: &str) -> Value {
        json!({"meta": {"id": "vm-1", "resource_version": rv}, "spec": {"name": "x", "labels": {"owner@example.com": "1"}},
               "status": {"phase": phase, "last_reconciled_at": at,
                          "conditions": [{"kind": "Ready", "message": cond}]}})
    }

    #[test]
    fn counts_unchanged_rewrites_fields_and_bookkeeping() {
        let mut t = Tally::default();
        let slot = json!({"slots": [1, 2, 3]});
        let ops = [
            vec![put(TableId::Rate5m, "0000003600/p/vm-1", slot.clone())],
            vec![put(TableId::Rate5m, "0000003600/p/vm-1", slot.clone())],
            vec![put(TableId::Vms, "vm-1", vm(1, "Running", 1000, "a"))],
            // Bookkeeping only, 60 s later: early.
            vec![put(TableId::Vms, "vm-1", vm(2, "Running", 1060, "b"))],
            // Bookkeeping only, 10 minutes after the last: on time.
            vec![put(TableId::Vms, "vm-1", vm(3, "Running", 1660, "c"))],
            // Acted on.
            vec![put(TableId::Vms, "vm-1", vm(4, "Stopped", 1700, "c"))],
        ];
        for (i, ops) in ops.into_iter().enumerate() {
            let e = entry(i as u64 + 1, Origin::Controller, ops);
            t.add(i as u64 + 1, serde_json::to_vec(&e).unwrap().len(), &e);
        }
        let blank = openraft::Entry::<TypeConfig> {
            log_id: openraft::LogId::new(openraft::CommittedLeaderId::new(2, 1), 7),
            payload: openraft::EntryPayload::Blank,
        };
        t.add(7, 10, &blank);
        let r = t.report();
        assert_eq!(
            (r.first_index, r.last_index, r.entries, r.blank),
            (Some(1), Some(7), 7, 1)
        );
        assert_eq!(r.span_secs, Some(700));
        let rate = r.tables.iter().find(|t| t.table == "rate_5m").unwrap();
        assert_eq!(
            (rate.puts, rate.keys, rate.unchanged, rate.first_seen),
            (2, 1, 1, 1)
        );
        let vms = r.tables.iter().find(|t| t.table == "vms").unwrap();
        assert_eq!((vms.puts, vms.first_seen, vms.unchanged), (4, 1, 0));
        let fields: Vec<(&str, u64)> = vms
            .changes
            .iter()
            .map(|c| (c.fields.as_str(), c.writes))
            .collect();
        assert_eq!(
            fields,
            vec![
                (
                    "meta.resource_version status.conditions status.last_reconciled_at",
                    2
                ),
                (
                    "meta.resource_version status.last_reconciled_at status.phase",
                    1
                ),
            ]
        );
        assert_eq!((r.vm_bookkeeping, r.vm_bookkeeping_early), (2, 1));
        let conds: Vec<(&str, u64)> = r
            .vm_conditions
            .iter()
            .map(|c| (c.fields.as_str(), c.writes))
            .collect();
        assert_eq!(conds, vec![("Ready.message", 2)]);
        assert_eq!(r.origins[0].origin, "controller");
        // Nothing of a key or a value in the report.
        let text = r.render() + &serde_json::to_string(&r).unwrap();
        for fixture in ["vm-1", "0000003600", "Running", "example.com"] {
            assert!(
                !text.contains(fixture),
                "a fixture's key or value in the report"
            );
        }
    }

    #[test]
    fn field_names_that_are_data_are_masked() {
        let mut out = BTreeSet::new();
        changed_paths(
            &json!({"members": {"a@example.com": 1}, "n": 1}),
            &json!({"members": {"a@example.com": 2}, "n": 1}),
            "",
            0,
            &mut out,
        );
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec!["members.*"]);
        let mut out = BTreeSet::new();
        changed_paths(&json!(1), &json!(2), "", 0, &mut out);
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec!["(value)"]);
    }
}

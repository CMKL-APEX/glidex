//! The usage ledger (spec/metering.md §6, §7): cursors, open-hour
//! accumulators and immutable hourly rows.
//!
//! A sampling round collects observations into a [`Round`] in memory and
//! commits it with [`Ledger::commit`], which writes every accumulated
//! delta *and* the cursors that produced them in one transaction
//! (**Invariant (exactly once, D8)**): a failed commit changes nothing,
//! and the next round recomputes from the old cursors.
//!
//! Times are unix **milliseconds** inside the ledger; hours are keyed by
//! their start in unix **seconds** (§7.1).

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use thiserror::Error;

const CURSORS: TableDefinition<&str, &[u8]> = TableDefinition::new("meter_cursors");
const OPEN: TableDefinition<&str, &[u8]> = TableDefinition::new("meter_open");
const HOURLY: TableDefinition<&str, &[u8]> = TableDefinition::new("usage_hourly");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meter_meta");
const RATE5: TableDefinition<&str, &[u8]> = TableDefinition::new("rate_5m");
const DAILY: TableDefinition<&str, &[u8]> = TableDefinition::new("usage_daily");
const MONTHLY_RATES: TableDefinition<&str, &[u8]> = TableDefinition::new("usage_monthly_rates");

/// `meter_meta`: when metering first ran (unix ms), written once.
const META_STARTED_AT: &str = "started_at";
/// `meter_meta`: every hour ending at or before this (unix s) is closed.
const META_CLOSED_THROUGH: &str = "closed_through";
/// `meter_meta`: end of the last committed round (unix ms).
const META_LAST_ROUND: &str = "last_round";

pub const HOUR_MS: u64 = 3_600_000;
/// The 95th-percentile slot (§8.5.1).
pub const SLOT_MS: u64 = 300_000;
pub const SLOTS_PER_HOUR: usize = 12;

/// Meters kept per 5-minute slot as well as per hour (§8.5.1, §8.6,
/// §8.7).
pub fn is_slotted(meter: &str) -> bool {
    matches!(
        meter,
        "cpu.used"
            | "cpu.alloc"
            | "mem.used"
            | "mem.alloc"
            | "net.rx_bytes"
            | "net.tx_bytes"
            | "net.ext_rx_bytes"
            | "net.ext_tx_bytes"
            | "bridge.bytes"
            | "bridge.ext_rx_bytes"
            | "bridge.ext_tx_bytes"
            | "disk.read_ops"
            | "disk.write_ops"
            | "disk.read_bytes"
            | "disk.write_bytes"
            | "disk.read_time_ns"
            | "disk.write_time_ns"
    )
}

/// One subject's twelve 5-minute slots of one hour (`rate_5m`, §7.1):
/// per-slot deltas, and which slots it was sampled in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRow {
    pub subject: Option<Subject>,
    /// Bit i: the subject was sampled during slot i (it existed and ran).
    pub present: u16,
    /// Bit i: slot i holds a delta spread over a gap (§6.3).
    #[serde(default)]
    pub interpolated: u16,
    pub series: BTreeMap<String, [u64; SLOTS_PER_HOUR]>,
}

impl SlotRow {
    fn merge(&mut self, other: SlotRow) {
        self.present |= other.present;
        self.interpolated |= other.interpolated;
        for (m, v) in other.series {
            let e = self.series.entry(m).or_insert([0; SLOTS_PER_HOUR]);
            for (a, b) in e.iter_mut().zip(v) {
                *a = a.saturating_add(b);
            }
        }
        if other.subject.is_some() {
            self.subject = other.subject;
        }
    }
}

/// Project key segment for host-level subjects (images, host networks).
const NO_PROJECT: &str = "-";

#[derive(Debug, Error)]
pub enum MeteringError {
    #[error("metering storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for MeteringError {
            fn from(e: $t) -> Self {
                MeteringError::Storage(e.to_string())
            }
        }
    )*};
}
storage_from!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    serde_json::Error
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    Vm,
    Disk,
    Nic,
    Network,
    Image,
}

impl SubjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Vm => "vm",
            SubjectKind::Disk => "disk",
            SubjectKind::Nic => "nic",
            SubjectKind::Network => "network",
            SubjectKind::Image => "image",
        }
    }
}

/// What a usage row is about. `name` and `project` are snapshots taken
/// when the subject was sampled (D13): later renames and deletions don't
/// rewrite history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub kind: SubjectKind,
    pub id: String,
    pub name: String,
    /// Owning project id; `None` for host-level subjects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nic: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

impl Subject {
    pub fn new(kind: SubjectKind, id: impl Into<String>, name: impl Into<String>, project: Option<String>) -> Self {
        Self { kind, id: id.into(), name: name.into(), project, vm_id: None, nic: None, network: None }
    }

    fn key(&self) -> String {
        format!("{}/{}", self.kind.as_str(), self.id)
    }

    fn project_key(&self) -> &str {
        self.project.as_deref().unwrap_or(NO_PROJECT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "flag")]
pub enum Flag {
    /// A delta or level was spread over a gap longer than the sampling
    /// interval (§6.3, §6.4).
    Interpolated,
    /// A counter went backwards under the same reset key (§6.2).
    Reset,
    /// Usage was lost, e.g. at a host reboot (§6.4).
    Incomplete { lost_secs: u64 },
    /// CPU and memory came from `/proc` instead of a cgroup (§5.1).
    SourceProc,
}

/// `*_peak` and `*.peak` meters keep the maximum; every other meter is a
/// sum (§2).
pub fn is_max_meter(meter: &str) -> bool {
    meter.ends_with("_peak") || meter.ends_with(".peak")
}

/// One closed (or, when read back with [`Ledger::scan`], still open)
/// hour of one subject (§7.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// UTC hour start, unix seconds.
    pub hour: u64,
    pub subject: Subject,
    /// 0 for the closing row, >0 for adjustments (§6.5).
    pub seq: u16,
    pub meters: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub flags: BTreeSet<Flag>,
    /// Unix seconds; 0 for a provisional (open) hour.
    pub written_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    /// Identifies one run of an upstream counter (§6.2); empty for gauges.
    reset_key: String,
    /// Counter: last raw reading. Gauge: level held since `at`.
    value: u64,
    at: u64,
    /// Gauge: integrated `level × ms` not yet a whole unit (< 1000),
    /// carried to the next interval so nothing is lost to rounding.
    #[serde(default, skip_serializing_if = "is_zero")]
    rem: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Where a counter seen for the first time started from (§6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The counter was 0 at this time (unix ms), e.g. an instance's
    /// launch. Counted in full if that is after metering started;
    /// otherwise only a baseline is taken (no backfill, §15.1).
    ZeroAt(u64),
    /// Unknown: take a baseline, count from the next reading.
    Unknown,
}

/// Ledger settings that affect how deltas are attributed.
#[derive(Debug, Clone, Copy)]
pub struct LedgerSettings {
    /// A delta spread over a longer span than this (ms) is flagged
    /// `interpolated`, and yields no peak (§8.5).
    pub gap_ms: u64,
    /// An hour is closed this long (ms) after it ends (§6.5).
    pub close_grace_ms: u64,
}

impl LedgerSettings {
    pub fn from_secs(sample_secs: u64, close_grace_secs: u64) -> Self {
        Self { gap_ms: 2 * sample_secs * 1000, close_grace_ms: close_grace_secs * 1000 }
    }
}

/// What a counter observation contributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delta {
    pub amount: u64,
    pub from: u64,
    pub to: u64,
    /// Spread over a gap; not usable as a rate sample.
    pub interpolated: bool,
}

impl Delta {
    /// `amount × factor / seconds`, or `None` when the interval is empty
    /// or spans a gap (peaks ignore those, §8.5).
    pub fn rate(&self, factor: u64) -> Option<u64> {
        let ms = self.to.checked_sub(self.from)?;
        if ms == 0 || self.interpolated {
            return None;
        }
        Some(((self.amount as u128 * factor as u128 * 1000) / ms as u128) as u64)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct HourAcc {
    subject: Option<Subject>,
    meters: BTreeMap<String, u64>,
    #[serde(default)]
    flags: BTreeSet<Flag>,
}

impl HourAcc {
    fn add(&mut self, meter: &str, amount: u64) {
        let v = self.meters.entry(meter.to_string()).or_insert(0);
        if is_max_meter(meter) {
            *v = (*v).max(amount);
        } else {
            *v = v.saturating_add(amount);
        }
    }
}

/// Observations of one sampling round, applied to cursors in memory and
/// written by [`Ledger::commit`].
pub struct Round {
    settings: LedgerSettings,
    started_at: u64,
    cursors: HashMap<String, Option<Cursor>>,
    /// Subjects (`kind/id`) whose cursors are all dropped at commit.
    forgotten: BTreeSet<String>,
    acc: BTreeMap<(u64, String), HourAcc>,
    slots: BTreeMap<(u64, String), SlotRow>,
    /// Rates seen this round, for live stats (§9.4); not stored.
    live: BTreeMap<String, (Subject, BTreeMap<String, f64>)>,
    db: Arc<Database>,
    now: u64,
}

impl Round {
    fn cursor(&mut self, key: &str) -> Result<Option<Cursor>, MeteringError> {
        if let Some(c) = self.cursors.get(key) {
            return Ok(c.clone());
        }
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CURSORS)?;
        let c = match table.get(key)? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        };
        self.cursors.insert(key.to_string(), c.clone());
        Ok(c)
    }

    fn acc(&mut self, hour: u64, subject: &Subject) -> &mut HourAcc {
        let acc = self.acc.entry((hour, subject.key())).or_default();
        acc.subject = Some(subject.clone());
        acc
    }

    /// Spread `amount` of `meter` over `[from, to]` into hour buckets in
    /// proportion to time (§6.3). The parts add up to `amount` exactly.
    fn slot_row(&mut self, slot_start: u64, subject: &Subject) -> (&mut SlotRow, usize) {
        let row = self.slots.entry((hour_of(slot_start), subject.key())).or_default();
        row.subject = Some(subject.clone());
        (row, ((slot_start % HOUR_MS) / SLOT_MS) as usize)
    }

    /// The subject was sampled over `(from, to]`: mark those slots.
    fn mark_present(&mut self, subject: &Subject, from: u64, to: u64, interpolated: bool) {
        let mut t = from.min(to) / SLOT_MS * SLOT_MS;
        loop {
            // A slot counts if any of (from, to] falls in it.
            if t + SLOT_MS > from || t == to / SLOT_MS * SLOT_MS {
                let (row, i) = self.slot_row(t, subject);
                row.present |= 1 << i;
                if interpolated {
                    row.interpolated |= 1 << i;
                }
            }
            if t + SLOT_MS > to.saturating_sub(1) {
                break;
            }
            t += SLOT_MS;
        }
    }

    fn spread(&mut self, subject: &Subject, meter: &str, amount: u64, from: u64, to: u64, flag: Option<Flag>) {
        if is_slotted(meter) {
            for (slot, part) in split_by(amount, from, to, SLOT_MS) {
                let (row, i) = self.slot_row(slot, subject);
                row.series.entry(meter.to_string()).or_insert([0; SLOTS_PER_HOUR])[i] += part;
            }
        }
        for (hour, part) in split_hours(amount, from, to) {
            let acc = self.acc(hour, subject);
            acc.add(meter, part);
            if let Some(f) = &flag {
                acc.flags.insert(f.clone());
            }
        }
    }

    /// A cumulative counter reading (§6.2). Returns the delta it
    /// contributed, if any.
    pub fn counter(
        &mut self,
        subject: &Subject,
        meter: &str,
        reset_key: &str,
        value: u64,
        at: u64,
        origin: Origin,
    ) -> Result<Option<Delta>, MeteringError> {
        let key = format!("{}/{}", subject.key(), meter);
        self.counter_at_key(key, subject, meter, reset_key, value, at, origin)
    }

    /// One of several upstream counters that add up to one meter (e.g.
    /// every port of a bridge into `bridge.bytes`): each `part` keeps its
    /// own cursor and reset key.
    #[allow(clippy::too_many_arguments)]
    pub fn counter_part(
        &mut self,
        subject: &Subject,
        meter: &str,
        part: &str,
        reset_key: &str,
        value: u64,
        at: u64,
        origin: Origin,
    ) -> Result<Option<Delta>, MeteringError> {
        let key = format!("{}/{}#{}", subject.key(), meter, part);
        self.counter_at_key(key, subject, meter, reset_key, value, at, origin)
    }

    /// Drop the cursors of a meter's parts that are not in `keep` (ports
    /// that are gone).
    pub fn prune_parts(&mut self, subject: &Subject, meter: &str, keep: &BTreeSet<String>) -> Result<(), MeteringError> {
        let prefix = format!("{}/{}#", subject.key(), meter);
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CURSORS)?;
        let stored: Vec<String> = table
            .range(prefix.as_str()..format!("{prefix}~").as_str())?
            .map(|r| r.map(|(k, _)| k.value().to_string()))
            .collect::<Result<_, _>>()?;
        let known = self.cursors.keys().filter(|k| k.starts_with(&prefix)).cloned();
        let all: BTreeSet<String> = stored.into_iter().chain(known).collect();
        for k in all {
            if !keep.contains(&k[prefix.len()..]) {
                self.cursors.insert(k, None);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn counter_at_key(
        &mut self,
        key: String,
        subject: &Subject,
        meter: &str,
        reset_key: &str,
        value: u64,
        at: u64,
        origin: Origin,
    ) -> Result<Option<Delta>, MeteringError> {
        let prev = self.cursor(&key)?;
        let (amount, from, reset) = match &prev {
            Some(c) if c.reset_key == reset_key && value >= c.value => (value - c.value, c.at, false),
            Some(c) if c.reset_key == reset_key => (value, c.at, true),
            Some(c) => match origin {
                // A new run of the counter: it counted from 0 since `t`.
                Origin::ZeroAt(t) => (value, t.max(c.at), false),
                Origin::Unknown => (value, c.at, false),
            },
            None => match origin {
                Origin::ZeroAt(t) if t >= self.started_at => (value, t, false),
                // Baseline: usage from before metering is not billed.
                _ => (0, at, false),
            },
        };
        self.cursors.insert(key, Some(Cursor { reset_key: reset_key.to_string(), value, at, rem: 0 }));
        if is_slotted(meter) {
            let from = if prev.is_some() { from.min(at) } else { at };
            self.mark_present(subject, from, at, at - from > self.settings.gap_ms);
        }
        if amount == 0 && prev.is_none() {
            return Ok(None);
        }
        let from = from.min(at);
        let interpolated = at - from > self.settings.gap_ms;
        let flag = if reset {
            Some(Flag::Reset)
        } else if interpolated {
            Some(Flag::Interpolated)
        } else {
            None
        };
        self.spread(subject, meter, amount, from, at, flag);
        Ok(Some(Delta { amount, from, to: at, interpolated }))
    }

    /// The last value of a counter run that has ended (an exit snapshot,
    /// D12): counts the tail since the cursor, under the old reset key.
    ///
    /// `run` matches the cursor's reset key exactly or as its prefix
    /// (`<run>/…`), so an exit record that knows only the instance finds
    /// a key like `<instance>/<shim start time>`. Calling it again adds
    /// nothing: it is consumed once.
    pub fn counter_final(&mut self, subject: &Subject, meter: &str, run: &str, value: u64, at: u64) -> Result<(), MeteringError> {
        let key = format!("{}/{}", subject.key(), meter);
        match self.cursor(&key)? {
            Some(c) if (c.reset_key == run || c.reset_key.starts_with(&format!("{run}/"))) && value >= c.value => {
                if value > c.value {
                    let from = c.at.min(at);
                    let flag = (at - from > self.settings.gap_ms).then_some(Flag::Interpolated);
                    self.spread(subject, meter, value - c.value, from, at, flag);
                }
                self.cursors.insert(key, Some(Cursor { value, at: at.max(c.at), ..c }));
            }
            // Already consumed, superseded, or never seen: nothing to add.
            _ => {}
        }
        Ok(())
    }

    /// The final value of a counter run at its exit (D12). If the run was
    /// seen, this is [`Self::counter_final`]. If it never was (it started
    /// and ended while the meter was down), it counts in full from
    /// `launched` (unix ms), or only as a baseline if that was before
    /// metering started. Returns whether the run was unseen and counted.
    pub fn counter_exit(&mut self, subject: &Subject, meter: &str, run: &str, value: u64, at: u64, launched: u64) -> Result<bool, MeteringError> {
        let key = format!("{}/{}", subject.key(), meter);
        if let Some(c) = self.cursor(&key)? {
            if c.reset_key == run || c.reset_key.starts_with(&format!("{run}/")) {
                self.counter_final(subject, meter, run, value, at)?;
                return Ok(false);
            }
        }
        let counted = launched > 0 && launched >= self.started_at && launched <= at;
        if counted && value > 0 {
            let flag = (at - launched > self.settings.gap_ms).then_some(Flag::Interpolated);
            self.spread(subject, meter, value, launched, at, flag);
        }
        self.cursors.insert(key, Some(Cursor { reset_key: run.to_string(), value, at, rem: 0 }));
        Ok(counted)
    }

    /// A gauge level (§2): the level held since the previous reading is
    /// integrated over the time since (`level × seconds`), then `level`
    /// is held from `at`.
    pub fn gauge(&mut self, subject: &Subject, meter: &str, level: u64, at: u64) -> Result<(), MeteringError> {
        self.integrate(subject, meter, at)?;
        let key = format!("{}/{}", subject.key(), meter);
        let run = self.cursors.get(&key).cloned().flatten().map(|c| c.reset_key).unwrap_or_default();
        self.set_gauge(&key, &run, level, at);
        Ok(())
    }

    /// End a gauge at `at` (the subject stopped or went away).
    pub fn gauge_end(&mut self, subject: &Subject, meter: &str, at: u64) -> Result<(), MeteringError> {
        self.integrate(subject, meter, at)?;
        self.cursors.insert(format!("{}/{}", subject.key(), meter), None);
        Ok(())
    }

    /// A gauge whose level changed to `level` at `since` (a known
    /// transition time, e.g. `status.phase_since`), read at `at`: the
    /// old level is integrated up to `since`, the new one from there.
    /// A `since` the cursor has already passed is ignored.
    pub fn gauge_since(&mut self, subject: &Subject, meter: &str, level: u64, since: u64, at: u64) -> Result<(), MeteringError> {
        let key = format!("{}/{}", subject.key(), meter);
        if let Some(c) = self.cursor(&key)? {
            if since > c.at && since < at && c.value != level {
                self.gauge(subject, meter, level, since)?;
            }
        }
        self.gauge(subject, meter, level, at)
    }

    /// A gauge that belongs to one *run* of its subject (an instance):
    /// `run` identifies it, `run_start` is when it began (unix ms).
    ///
    /// - Same run as the cursor: like [`Self::gauge_since`].
    /// - A different run: the old one is ended at `prev_end` (its exit
    ///   time, if known and later than the last reading), and the new one
    ///   accrues from `max(run_start, since)`, never from before metering
    ///   started. Nothing accrues across the gap between the two, so a
    ///   VM that stopped and relaunched while the control plane was down
    ///   is not charged for the time it was off.
    /// - First sight: as a new run with no predecessor.
    #[allow(clippy::too_many_arguments)]
    pub fn gauge_run(
        &mut self,
        subject: &Subject,
        meter: &str,
        run: &str,
        run_start: u64,
        level: u64,
        since: u64,
        at: u64,
        prev_end: Option<u64>,
    ) -> Result<(), MeteringError> {
        let key = format!("{}/{}", subject.key(), meter);
        match self.cursor(&key)? {
            Some(c) if c.reset_key == run => self.gauge_since(subject, meter, level, since, at),
            prev => {
                if let Some(c) = prev {
                    let end = prev_end.filter(|&e| e > c.at).unwrap_or(c.at);
                    self.integrate(subject, meter, end)?;
                }
                let start = run_start.max(since).max(self.started_at).min(at);
                self.set_gauge(&key, run, level, start);
                self.integrate(subject, meter, at)?;
                self.set_gauge(&key, run, level, at);
                Ok(())
            }
        }
    }

    /// Hold `level` from `at`, keeping the rounding remainder of the run.
    fn set_gauge(&mut self, key: &str, run: &str, level: u64, at: u64) {
        let rem = match self.cursors.get(key) {
            Some(Some(c)) if c.reset_key == run => c.rem,
            _ => 0,
        };
        self.cursors.insert(key.to_string(), Some(Cursor { reset_key: run.to_string(), value: level, at, rem }));
    }

    /// Remember that `run` of the subject was seen (a cursor with no
    /// meter of its own).
    pub fn mark_run(&mut self, subject: &Subject, run: &str) {
        self.cursors.insert(format!("{}/_run", subject.key()), Some(Cursor { reset_key: run.to_string(), value: 0, at: self.now, rem: 0 }));
    }

    /// The run last marked with [`Self::mark_run`].
    pub fn marked_run(&mut self, subject: &Subject) -> Result<Option<String>, MeteringError> {
        Ok(self.cursor(&format!("{}/_run", subject.key()))?.map(|c| c.reset_key))
    }

    /// The run a gauge's cursor belongs to, if any.
    pub fn gauge_run_of(&mut self, subject: &Subject, meter: &str) -> Result<Option<String>, MeteringError> {
        Ok(self.cursor(&format!("{}/{}", subject.key(), meter))?.map(|c| c.reset_key))
    }

    /// End a gauge at the time it really ended, if that is after the
    /// last reading (`end` may be earlier than `now`, e.g. an exit time).
    pub fn gauge_end_at(&mut self, subject: &Subject, meter: &str, end: u64) -> Result<(), MeteringError> {
        let key = format!("{}/{}", subject.key(), meter);
        if self.cursor(&key)?.is_some() {
            self.gauge_end(subject, meter, end)?;
        }
        Ok(())
    }

    fn integrate(&mut self, subject: &Subject, meter: &str, at: u64) -> Result<(), MeteringError> {
        let key = format!("{}/{}", subject.key(), meter);
        if let Some(c) = self.cursor(&key)? {
            if at > c.at {
                // level × ms, plus what the last interval left over.
                let total = c.value as u128 * (at - c.at) as u128 + c.rem as u128;
                let amount = (total / 1000) as u64;
                let flag = (at - c.at > self.settings.gap_ms).then_some(Flag::Interpolated);
                self.spread(subject, meter, amount, c.at, at, flag);
                let rem = (total % 1000) as u64;
                self.cursors.insert(key, Some(Cursor { at, rem, ..c }));
            }
        }
        Ok(())
    }

    /// A `_peak` meter value seen at `at` (§2: max kind).
    pub fn max(&mut self, subject: &Subject, meter: &str, value: u64, at: u64) {
        debug_assert!(is_max_meter(meter), "{meter} is not a max meter");
        self.acc(hour_of(at), subject).add(meter, value);
    }

    /// Attach a flag to a subject's hour (e.g. `Incomplete`, `SourceProc`).
    pub fn flag(&mut self, subject: &Subject, at: u64, flag: Flag) {
        self.acc(hour_of(at), subject).flags.insert(flag);
    }

    /// Forget every cursor of a subject that is gone for good (`kind/id`,
    /// as from [`Ledger::cursor_subjects`]). Nothing is integrated.
    pub fn forget_subject(&mut self, subject_key: &str) {
        let prefix = format!("{subject_key}/");
        self.cursors.retain(|k, _| !k.starts_with(&prefix));
        self.forgotten.insert(subject_key.to_string());
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    /// Note a live rate of a subject (e.g. `rx_mbps`) for `GET …/stats`.
    pub fn live(&mut self, subject: &Subject, name: &str, value: f64) {
        let e = self.live.entry(subject.key()).or_insert_with(|| (subject.clone(), BTreeMap::new()));
        e.0 = subject.clone();
        e.1.insert(name.to_string(), (value * 1000.0).round() / 1000.0);
    }

    /// The live rates noted this round (taken before commit).
    pub fn take_live(&mut self) -> BTreeMap<String, (Subject, BTreeMap<String, f64>)> {
        std::mem::take(&mut self.live)
    }
}

/// Split `amount` over `[from, to]` (ms) into periods of `period` ms:
/// `(period start ms, part)`. The parts always sum to `amount`.
pub fn split_by(amount: u64, from: u64, to: u64, period: u64) -> Vec<(u64, u64)> {
    if amount == 0 {
        return Vec::new();
    }
    let start = |t: u64| t / period * period;
    if to <= from || start(from) == start(to.saturating_sub(1)) {
        return vec![(start(if to > from { to - 1 } else { to }), amount)];
    }
    let total = (to - from) as u128;
    let (mut out, mut done, mut given, mut t) = (Vec::new(), 0u128, 0u64, from);
    while t < to {
        let end = (start(t) + period).min(to);
        done += (end - t) as u128;
        let cum = ((amount as u128 * done) / total) as u64;
        if cum > given {
            out.push((start(t), cum - given));
            given = cum;
        }
        t = end;
    }
    out
}

/// Start of the UTC hour containing `ms`, in unix seconds.
pub fn hour_of(ms: u64) -> u64 {
    ms / HOUR_MS * 3600
}

/// Split `amount` over `[from, to]` (ms) into `(hour, part)` by overlap.
/// The parts always sum to `amount`; an empty interval goes to `to`'s hour.
pub fn split_hours(amount: u64, from: u64, to: u64) -> Vec<(u64, u64)> {
    split_by(amount, from, to, HOUR_MS).into_iter().map(|(t, p)| (t / 1000, p)).collect()
}

/// Key ranges for time-prefixed keys `<t:010>/<project>/…` over `[from,
/// to)`: one range, or, for a few named projects, one per period and
/// project, so other projects' rows are never read (§15.6).
fn ranges(from: u64, to: u64, period: u64, projects: Option<&BTreeSet<String>>) -> Vec<(String, String)> {
    match projects {
        Some(ps) if ps.len() <= 64 => {
            let mut out = Vec::new();
            let mut t = from / period * period;
            while t < to {
                for p in ps {
                    out.push((format!("{t:010}/{p}/"), format!("{t:010}/{p}/~")));
                }
                t += period;
            }
            out
        }
        _ => vec![(format!("{:010}/", from / period * period), format!("{to:010}/"))],
    }
}

fn slot_key(hour: u64, subject: &Subject) -> String {
    format!("{hour:010}/{}/{}", subject.project_key(), subject.key())
}

fn hour_key(hour: u64, project: &str, subject_key: &str, seq: u16) -> String {
    format!("{hour:010}/{project}/{subject_key}/{seq:04}")
}

fn open_key(hour: u64, subject_key: &str) -> String {
    format!("{hour:010}/{subject_key}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct Ledger {
    db: Arc<Database>,
    settings: LedgerSettings,
}

impl Ledger {
    /// Open (creating if needed) the metering tables (§7.1). New tables
    /// only: `SCHEMA_VERSION` is unchanged.
    pub fn new(db: Arc<Database>, settings: LedgerSettings) -> Result<Self, MeteringError> {
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(CURSORS)?;
            let _ = txn.open_table(OPEN)?;
            let _ = txn.open_table(HOURLY)?;
            let _ = txn.open_table(META)?;
            let _ = txn.open_table(RATE5)?;
            let _ = txn.open_table(DAILY)?;
            let _ = txn.open_table(MONTHLY_RATES)?;
        }
        txn.commit()?;
        let l = Self { db, settings };
        l.rekey_slots()?;
        Ok(l)
    }

    fn meta_u64(&self, key: &str) -> Result<Option<u64>, MeteringError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(META)?;
        Ok(table.get(key)?.and_then(|v| std::str::from_utf8(v.value()).ok()?.parse().ok()))
    }

    /// When metering first ran (unix ms); set on the first call.
    pub fn started_at(&self) -> Result<u64, MeteringError> {
        if let Some(t) = self.meta_u64(META_STARTED_AT)? {
            return Ok(t);
        }
        let t = now_ms();
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(META)?;
            if table.get(META_STARTED_AT)?.is_none() {
                table.insert(META_STARTED_AT, t.to_string().as_bytes())?;
            }
        }
        txn.commit()?;
        Ok(self.meta_u64(META_STARTED_AT)?.unwrap_or(t))
    }

    /// Rows and key + value bytes per metering table (benchmarks).
    #[cfg(test)]
    pub(crate) fn table_sizes(&self) -> Vec<(&'static str, usize, usize)> {
        let txn = self.db.begin_read().unwrap();
        [("meter_cursors", CURSORS), ("meter_open", OPEN), ("usage_hourly", HOURLY), ("rate_5m", RATE5), ("usage_daily", DAILY)]
            .into_iter()
            .map(|(name, t)| {
                let t = txn.open_table(t).unwrap();
                let (mut n, mut bytes) = (0, 0);
                for r in t.iter().unwrap() {
                    let (k, v) = r.unwrap();
                    n += 1;
                    bytes += k.value().len() + v.value().len();
                }
                (name, n, bytes)
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn set_started_at_for_test(&self, ms: u64) {
        let txn = self.db.begin_write().unwrap();
        txn.open_table(META).unwrap().insert(META_STARTED_AT, ms.to_string().as_bytes()).unwrap();
        txn.commit().unwrap();
    }

    /// End of the last closed hour (unix s); rows before it are final.
    pub fn complete_through(&self) -> Result<u64, MeteringError> {
        Ok(self.meta_u64(META_CLOSED_THROUGH)?.unwrap_or(0))
    }

    pub fn last_round(&self) -> Result<Option<u64>, MeteringError> {
        self.meta_u64(META_LAST_ROUND)
    }

    /// Subjects (`kind/id`) of the given kinds that have any cursor.
    pub fn cursor_subjects(&self, kinds: &[SubjectKind]) -> Result<BTreeSet<String>, MeteringError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CURSORS)?;
        let mut out = BTreeSet::new();
        for kind in kinds {
            let prefix = format!("{}/", kind.as_str());
            for r in table.range(prefix.as_str()..format!("{}~", prefix).as_str())? {
                let (k, _) = r?;
                let mut parts = k.value().splitn(3, '/');
                if let (Some(kind), Some(id)) = (parts.next(), parts.next()) {
                    out.insert(format!("{kind}/{id}"));
                }
            }
        }
        Ok(out)
    }

    pub fn begin_round(&self, now: u64) -> Result<Round, MeteringError> {
        Ok(Round {
            settings: self.settings,
            started_at: self.started_at()?,
            cursors: HashMap::new(),
            forgotten: BTreeSet::new(),
            acc: BTreeMap::new(),
            slots: BTreeMap::new(),
            live: BTreeMap::new(),
            db: self.db.clone(),
            now,
        })
    }

    /// Write a round's accumulators and cursors in one transaction (D8),
    /// then close every hour that ended `close_grace` ago (§6.5).
    pub fn commit(&self, round: Round) -> Result<(), MeteringError> {
        let closed_through = self.complete_through()?;
        let close_to = hour_of(round.now.saturating_sub(self.settings.close_grace_ms));
        let written_at = round.now / 1000;
        let txn = self.db.begin_write()?;
        {
            let mut cursors = txn.open_table(CURSORS)?;
            for subject in &round.forgotten {
                let (from, to) = (format!("{subject}/"), format!("{subject}/~"));
                let keys: Vec<String> = cursors
                    .range(from.as_str()..to.as_str())?
                    .map(|r| r.map(|(k, _)| k.value().to_string()))
                    .collect::<Result<_, _>>()?;
                for k in keys {
                    cursors.remove(k.as_str())?;
                }
            }
            for (key, c) in &round.cursors {
                match c {
                    Some(c) => {
                        cursors.insert(key.as_str(), serde_json::to_vec(c)?.as_slice())?;
                    }
                    None => {
                        cursors.remove(key.as_str())?;
                    }
                }
            }
            let mut open = txn.open_table(OPEN)?;
            for ((hour, skey), acc) in round.acc {
                let k = open_key(hour, &skey);
                let mut merged: HourAcc = match open.get(k.as_str())? {
                    Some(v) => serde_json::from_slice(v.value())?,
                    None => HourAcc::default(),
                };
                for (m, v) in &acc.meters {
                    merged.add(m, *v);
                }
                merged.flags.extend(acc.flags);
                merged.subject = acc.subject;
                open.insert(k.as_str(), serde_json::to_vec(&merged)?.as_slice())?;
            }
            // Close: every open hour that ends at or before `close_to`.
            // An hour closed earlier gets an adjustment row (seq > 0).
            let due: Vec<(String, HourAcc)> = open
                .range(..open_key(close_to, "").as_str())?
                .map(|r| {
                    let (k, v) = r?;
                    Ok((k.value().to_string(), serde_json::from_slice(v.value())?))
                })
                .collect::<Result<_, MeteringError>>()?;
            let mut hourly = txn.open_table(HOURLY)?;
            for (k, acc) in due {
                open.remove(k.as_str())?;
                let Some(subject) = acc.subject else { continue };
                let hour: u64 = k.split('/').next().and_then(|h| h.parse().ok()).unwrap_or(0);
                let prefix = format!("{hour:010}/{}/{}/", subject.project_key(), subject.key());
                let seq = match hourly.range(prefix.as_str()..format!("{prefix}~").as_str())?.next_back() {
                    Some(r) => {
                        let (k, _) = r?;
                        k.value().rsplit('/').next().and_then(|s| s.parse::<u16>().ok()).map_or(0, |s| s + 1)
                    }
                    None => 0,
                };
                let row = UsageRecord { hour, subject, seq, meters: acc.meters, flags: acc.flags, written_at };
                let key = hour_key(hour, row.subject.project_key(), &row.subject.key(), seq);
                hourly.insert(key.as_str(), serde_json::to_vec(&row)?.as_slice())?;
            }
            let mut rate5 = txn.open_table(RATE5)?;
            for ((hour, _), row) in round.slots {
                let Some(subject) = &row.subject else { continue };
                let k = slot_key(hour, subject);
                let mut merged: SlotRow = match rate5.get(k.as_str())? {
                    Some(v) => serde_json::from_slice(v.value())?,
                    None => SlotRow::default(),
                };
                merged.merge(row);
                rate5.insert(k.as_str(), serde_json::to_vec(&merged)?.as_slice())?;
            }
            let mut meta = txn.open_table(META)?;
            meta.insert(META_LAST_ROUND, round.now.to_string().as_bytes())?;
            if close_to > closed_through {
                meta.insert(META_CLOSED_THROUGH, close_to.to_string().as_bytes())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Rows for hours in `[from, to)` (unix s), closed rows first, then
    /// the open (provisional, `written_at == 0`) accumulators.
    /// `projects`: only these project ids (`None`: all).
    pub fn scan(&self, from: u64, to: u64, projects: Option<&BTreeSet<String>>) -> Result<Vec<UsageRecord>, MeteringError> {
        let keep = |s: &Subject| projects.is_none_or(|p| s.project.as_ref().is_some_and(|id| p.contains(id)));
        let txn = self.db.begin_read()?;
        let mut out = Vec::new();
        let hourly = txn.open_table(HOURLY)?;
        for (a, b) in ranges(from, to, 3600, projects) {
            for r in hourly.range(a.as_str()..b.as_str())? {
                let (_, v) = r?;
                let row: UsageRecord = serde_json::from_slice(v.value())?;
                if row.hour >= from && keep(&row.subject) {
                    out.push(row);
                }
            }
        }
        // Days rolled up from hours past retention (§7.3), as day rows.
        let daily = txn.open_table(DAILY)?;
        if daily.first()?.is_some_and(|(k, _)| k.value() < format!("{to:010}/").as_str()) {
            // Day rows start at a whole hour (the billing zone's midnight).
            for (a, b) in ranges(from.saturating_sub(86400), to, 3600, projects) {
                for r in daily.range(a.as_str()..b.as_str())? {
                    let (_, v) = r?;
                    let row: UsageRecord = serde_json::from_slice(v.value())?;
                    if row.hour >= from && keep(&row.subject) {
                        out.push(row);
                    }
                }
            }
        }
        let open = txn.open_table(OPEN)?;
        for r in open.range(format!("{from:010}/").as_str()..format!("{to:010}/").as_str())? {
            let (k, v) = r?;
            let acc: HourAcc = serde_json::from_slice(v.value())?;
            let Some(subject) = acc.subject else { continue };
            if keep(&subject) {
                let hour = k.value().split('/').next().and_then(|h| h.parse().ok()).unwrap_or(0);
                out.push(UsageRecord { hour, subject, seq: 0, meters: acc.meters, flags: acc.flags, written_at: 0 });
            }
        }
        Ok(out)
    }
}

impl Ledger {
    /// Roll closed hourly rows of hours before `cutoff` (unix s) into one
    /// row per subject and day, and delete them (§7.3). Days start at
    /// midnight `offset_secs` from UTC (the billing time zone's), so
    /// billing months stay exact after the roll-up. Returns how many
    /// hourly rows went.
    pub fn roll_up_hours_before(&self, cutoff: u64, offset_secs: i64) -> Result<usize, MeteringError> {
        let day_of = |t: u64| ((t as i64 + offset_secs).div_euclid(86400) * 86400 - offset_secs).max(0) as u64;
        let cutoff = day_of(cutoff.min(self.complete_through()?));
        let txn = self.db.begin_write()?;
        let n;
        {
            let mut hourly = txn.open_table(HOURLY)?;
            let old: Vec<(String, UsageRecord)> = hourly
                .range(..format!("{cutoff:010}/").as_str())?
                .map(|r| {
                    let (k, v) = r?;
                    Ok((k.value().to_string(), serde_json::from_slice(v.value())?))
                })
                .collect::<Result<_, MeteringError>>()?;
            n = old.len();
            let mut days: BTreeMap<String, UsageRecord> = BTreeMap::new();
            for (_, r) in &old {
                let day = day_of(r.hour);
                let key = format!("{day:010}/{}/{}", r.subject.project_key(), r.subject.key());
                let d = days.entry(key).or_insert_with(|| UsageRecord {
                    hour: day,
                    subject: r.subject.clone(),
                    seq: 0,
                    meters: BTreeMap::new(),
                    flags: BTreeSet::new(),
                    written_at: r.written_at,
                });
                let mut acc = HourAcc { subject: None, meters: std::mem::take(&mut d.meters), flags: BTreeSet::new() };
                for (m, v) in &r.meters {
                    acc.add(m, *v);
                }
                d.meters = acc.meters;
                d.flags.extend(r.flags.iter().cloned());
                d.subject = r.subject.clone();
            }
            let mut daily = txn.open_table(DAILY)?;
            for (k, mut d) in days {
                // A day may already have a row (an adjustment rolled later).
                if let Some(prev) = daily.get(k.as_str())? {
                    let prev: UsageRecord = serde_json::from_slice(prev.value())?;
                    let mut acc = HourAcc { subject: None, meters: prev.meters, flags: BTreeSet::new() };
                    for (m, v) in &d.meters {
                        acc.add(m, *v);
                    }
                    d.meters = acc.meters;
                    d.flags.extend(prev.flags);
                }
                daily.insert(k.as_str(), serde_json::to_vec(&d)?.as_slice())?;
            }
            for (k, _) in old {
                hourly.remove(k.as_str())?;
            }
        }
        txn.commit()?;
        Ok(n)
    }

    /// Delete rows keyed by time before `cutoff` (unix s) from `table`.
    fn prune(&self, table: TableDefinition<&str, &[u8]>, cutoff: u64) -> Result<usize, MeteringError> {
        let txn = self.db.begin_write()?;
        let n;
        {
            let mut t = txn.open_table(table)?;
            let keys: Vec<String> = t
                .range(..format!("{cutoff:010}/").as_str())?
                .map(|r| r.map(|(k, _)| k.value().to_string()))
                .collect::<Result<_, _>>()?;
            n = keys.len();
            for k in keys {
                t.remove(k.as_str())?;
            }
        }
        txn.commit()?;
        Ok(n)
    }

    pub fn prune_slots_before(&self, cutoff: u64) -> Result<usize, MeteringError> {
        self.prune(RATE5, cutoff)
    }

    pub fn prune_daily_before(&self, cutoff: u64) -> Result<usize, MeteringError> {
        self.prune(DAILY, cutoff)
    }

    /// Final figures of a billing month (`usage_monthly_rates`, §8.5.1):
    /// `month_start` (unix s) keys them so pruning by time works.
    pub fn put_month_rates(&self, month_start: u64, key: &str, value: &serde_json::Value) -> Result<(), MeteringError> {
        let txn = self.db.begin_write()?;
        txn.open_table(MONTHLY_RATES)?.insert(format!("{month_start:010}/{key}").as_str(), serde_json::to_vec(value)?.as_slice())?;
        txn.commit()?;
        Ok(())
    }

    /// `(key, value)` of a month's final figures whose key starts with `prefix`.
    pub fn month_rates(&self, month_start: u64, prefix: &str) -> Result<Vec<(String, serde_json::Value)>, MeteringError> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(MONTHLY_RATES)?;
        let from = format!("{month_start:010}/{prefix}");
        let mut out = Vec::new();
        for r in t.range(from.as_str()..format!("{from}~").as_str())? {
            let (k, v) = r?;
            out.push((k.value()[11..].to_string(), serde_json::from_slice(v.value())?));
        }
        Ok(out)
    }

    pub fn prune_month_rates_before(&self, cutoff: u64) -> Result<usize, MeteringError> {
        self.prune(MONTHLY_RATES, cutoff)
    }

    /// A small value in `meter_meta` (finalized months, retention runs).
    pub fn meta(&self, key: &str) -> Result<Option<String>, MeteringError> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(META)?;
        Ok(t.get(key)?.map(|v| String::from_utf8_lossy(v.value()).into_owned()))
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), MeteringError> {
        let txn = self.db.begin_write()?;
        txn.open_table(META)?.insert(key, value.as_bytes())?;
        txn.commit()?;
        Ok(())
    }

    /// Slot rows for hours in `[from, to)` (unix s): `(hour, row)`, of
    /// `projects` only (`None`: all) and those `keep` accepts.
    pub fn scan_slots(
        &self,
        from: u64,
        to: u64,
        projects: Option<&BTreeSet<String>>,
        keep: impl Fn(&Subject) -> bool,
    ) -> Result<Vec<(u64, SlotRow)>, MeteringError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RATE5)?;
        let mut out = Vec::new();
        for (a, b) in ranges(from, to, 3600, projects) {
            for r in table.range(a.as_str()..b.as_str())? {
                let (k, v) = r?;
                let hour: u64 = k.value().split('/').next().and_then(|h| h.parse().ok()).unwrap_or(0);
                if hour < from {
                    continue;
                }
                let row: SlotRow = serde_json::from_slice(v.value())?;
                if row.subject.as_ref().is_some_and(&keep) {
                    out.push((hour, row));
                }
            }
        }
        Ok(out)
    }

    /// Slot rows written by M2 builds were keyed `<hour>/<kind>/<id>`:
    /// re-key them `<hour>/<project>/<kind>/<id>`. Idempotent.
    fn rekey_slots(&self) -> Result<usize, MeteringError> {
        let kinds = ["vm", "disk", "nic", "network", "image"];
        let txn = self.db.begin_write()?;
        let mut n = 0;
        {
            let mut t = txn.open_table(RATE5)?;
            let old: Vec<(String, SlotRow)> = t
                .iter()?
                .filter_map(|r| {
                    let (k, v) = r.ok()?;
                    let key = k.value().to_string();
                    let second = key.split('/').nth(1)?;
                    kinds.contains(&second).then(|| Some((key.clone(), serde_json::from_slice(v.value()).ok()?)))?
                })
                .collect();
            for (key, row) in old {
                let hour: u64 = key.split('/').next().and_then(|h| h.parse().ok()).unwrap_or(0);
                t.remove(key.as_str())?;
                if let Some(subject) = &row.subject {
                    t.insert(slot_key(hour, subject).as_str(), serde_json::to_vec(&row)?.as_slice())?;
                    n += 1;
                }
            }
        }
        txn.commit()?;
        Ok(n)
    }
}

/// Fold rows into one meter map: sums, or maxima for `_peak` meters.
pub fn combine<'a>(rows: impl IntoIterator<Item = &'a UsageRecord>) -> BTreeMap<String, u64> {
    let mut acc = HourAcc::default();
    for r in rows {
        for (m, v) in &r.meters {
            acc.add(m, *v);
        }
    }
    acc.meters
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = HOUR_MS;
    const T0: u64 = 1_791_000_000 / 3600 * H; // an hour boundary, in ms

    fn ledger() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.db")).unwrap());
        let l = Ledger::new(db, LedgerSettings::from_secs(30, 120)).unwrap();
        // Pretend metering started long ago, so ZeroAt origins count.
        l.set_started_at_for_test(0);
        (dir, l)
    }

    fn vm() -> Subject {
        Subject::new(SubjectKind::Vm, "vm-1", "web-1", Some("p1".into()))
    }

    fn rows(l: &Ledger) -> Vec<UsageRecord> {
        l.scan(0, u64::MAX / 10, None).unwrap()
    }

    fn total(l: &Ledger, meter: &str) -> u64 {
        rows(l).iter().map(|r| r.meters.get(meter).copied().unwrap_or(0)).sum()
    }

    /// xorshift: deterministic pseudo-random numbers for property tests.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn split_parts_sum_to_amount() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for _ in 0..10_000 {
            let from = T0 + rng.next() % (5 * H);
            let to = from + rng.next() % (4 * H);
            let amount = rng.next() % 1_000_000_000_000;
            let parts = split_hours(amount, from, to);
            assert_eq!(parts.iter().map(|p| p.1).sum::<u64>(), amount, "{from} {to} {amount}");
            let hours: Vec<u64> = parts.iter().map(|p| p.0).collect();
            assert!(hours.windows(2).all(|w| w[0] < w[1]));
            assert!(hours.iter().all(|&h| h * 1000 >= hour_of(from) * 1000 && h * 1000 <= to));
        }
    }

    #[test]
    fn slots_add_up_to_hours_and_mark_presence() {
        let (_d, l) = ledger();
        let mut s = vm();
        s.kind = SubjectKind::Nic;
        let mut rng = Rng(11);
        let mut t = T0;
        let mut v = 0;
        let mut r = l.begin_round(t).unwrap();
        r.counter(&s, "net.rx_bytes", "p", 0, t, Origin::ZeroAt(t)).unwrap();
        for _ in 0..200 {
            t += 30_000;
            v += rng.next() % 1_000_000;
            r.counter(&s, "net.rx_bytes", "p", v, t, Origin::Unknown).unwrap();
        }
        l.commit(r).unwrap();
        let slots = l.scan_slots(0, u64::MAX / 10, None, |_| true).unwrap();
        let slot_total: u64 = slots.iter().map(|(_, row)| row.series["net.rx_bytes"].iter().sum::<u64>()).sum();
        assert_eq!(slot_total, v, "slots add up to the hours");
        assert_eq!(slot_total, total(&l, "net.rx_bytes"));
        // 200 × 30 s = 100 min = 20 slots, all present, none interpolated.
        let present: u32 = slots.iter().map(|(_, row)| row.present.count_ones()).sum();
        assert_eq!(present, 20);
        assert!(slots.iter().all(|(_, row)| row.interpolated == 0));
        // Gauges are not slotted.
        assert!(slots.iter().all(|(_, row)| row.series.len() == 1));
    }

    #[test]
    fn m2_slot_keys_are_rekeyed_with_the_project() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        let s = vm();
        {
            let db = Arc::new(Database::create(&path).unwrap());
            let _ = Ledger::new(db.clone(), LedgerSettings::from_secs(30, 120)).unwrap();
            // A row as M2 wrote it: `<hour>/<kind>/<id>`.
            let row = SlotRow { subject: Some(s.clone()), present: 1, interpolated: 0, series: BTreeMap::new() };
            let txn = db.begin_write().unwrap();
            txn.open_table(RATE5).unwrap().insert(format!("{:010}/vm/vm-1", T0 / 1000).as_str(), serde_json::to_vec(&row).unwrap().as_slice()).unwrap();
            txn.commit().unwrap();
        }
        let l = Ledger::new(Arc::new(Database::create(&path).unwrap()), LedgerSettings::from_secs(30, 120)).unwrap();
        let only: BTreeSet<String> = ["p1".to_string()].into();
        let rows = l.scan_slots(T0 / 1000, T0 / 1000 + 3600, Some(&only), |_| true).unwrap();
        assert_eq!(rows.len(), 1, "found under its project");
        assert_eq!(l.rekey_slots().unwrap(), 0, "idempotent");
    }

    #[test]
    fn old_hours_roll_up_into_days_and_still_scan() {
        let (_d, l) = ledger();
        let s = vm();
        let mut r = l.begin_round(T0).unwrap();
        r.counter(&s, "cpu.used", "i", 0, T0, Origin::ZeroAt(T0)).unwrap();
        // 30 hours of usage, 100 per hour, then everything closes.
        for h in 1..=30u64 {
            r.counter(&s, "cpu.used", "i", 100 * h, T0 + h * H, Origin::Unknown).unwrap();
        }
        r.max(&s, "mem.peak", 7, T0 + 2 * H);
        r.now = T0 + 40 * H;
        l.commit(r).unwrap();
        let before = combine(&rows(&l));
        let n = l.roll_up_hours_before((T0 + 40 * H) / 1000, 0).unwrap();
        assert!(n > 0);
        let after = rows(&l);
        // Whole UTC days before the cutoff are day rows; the rest stay hourly.
        let cutoff_day = l.complete_through().unwrap() / 86400 * 86400;
        assert!(after.iter().filter(|r| r.hour < cutoff_day).all(|r| r.hour % 86400 == 0));
        assert!(after.iter().any(|r| r.hour >= cutoff_day && r.hour % 86400 != 0));
        assert_eq!(combine(&after), before, "sums and maxima kept");
        let days = after.iter().filter(|r| r.hour < cutoff_day).count();
        assert_eq!(l.prune_daily_before(u64::MAX / 10).unwrap(), days);
    }

    #[test]
    fn split_is_proportional_and_stays_in_one_hour() {
        assert_eq!(split_hours(100, T0 + 10, T0 + 30_000), vec![(T0 / 1000, 100)]);
        // Ends exactly on the boundary: still the earlier hour.
        assert_eq!(split_hours(100, T0 + H - 30_000, T0 + H), vec![(T0 / 1000, 100)]);
        // 3/4 before the boundary, 1/4 after.
        assert_eq!(split_hours(400, T0 + H - 30_000, T0 + H + 10_000), vec![(T0 / 1000, 300), ((T0 + H) / 1000, 100)]);
        // Empty interval: the hour of `to`.
        assert_eq!(split_hours(5, T0 + 7, T0 + 7), vec![(T0 / 1000, 5)]);
    }

    #[test]
    fn counter_deltas_and_reset_cases() {
        let (_d, l) = ledger();
        let s = vm();
        let mut r = l.begin_round(T0 + 60_000).unwrap();
        // First sight of a counter that started at launch: counted in full.
        let d = r.counter(&s, "cpu.used", "i1", 1_000, T0 + 30_000, Origin::ZeroAt(T0)).unwrap();
        assert_eq!(d.unwrap().amount, 1_000);
        // Normal delta.
        assert_eq!(r.counter(&s, "cpu.used", "i1", 1_500, T0 + 60_000, Origin::Unknown).unwrap().unwrap().amount, 500);
        // Went backwards under the same key: the new value counts, flagged.
        assert_eq!(r.counter(&s, "cpu.used", "i1", 200, T0 + 90_000, Origin::Unknown).unwrap().unwrap().amount, 200);
        // New instance: counts from zero.
        assert_eq!(r.counter(&s, "cpu.used", "i2", 70, T0 + 120_000, Origin::ZeroAt(T0 + 100_000)).unwrap().unwrap().amount, 70);
        l.commit(r).unwrap();
        assert_eq!(total(&l, "cpu.used"), 1_770);
        assert!(rows(&l)[0].flags.contains(&Flag::Reset));
    }

    #[test]
    fn no_backfill_before_metering_started() {
        let (_d, l) = ledger();
        l.set_started_at_for_test(T0 + H);
        let s = vm();
        let mut r = l.begin_round(T0 + H + 30_000).unwrap();
        // Launched before metering started: baseline only.
        assert_eq!(r.counter(&s, "cpu.used", "i1", 9_999, T0 + H + 30_000, Origin::ZeroAt(T0)).unwrap(), None);
        assert_eq!(r.counter(&s, "cpu.used", "i1", 10_099, T0 + H + 60_000, Origin::Unknown).unwrap().unwrap().amount, 100);
        l.commit(r).unwrap();
        assert_eq!(total(&l, "cpu.used"), 100);
    }

    #[test]
    fn exactly_once_across_rounds_and_failed_commits() {
        let (_d, l) = ledger();
        let s = vm();
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        r.counter(&s, "net.rx_bytes", "port-a", 0, T0, Origin::ZeroAt(T0)).unwrap();
        r.counter(&s, "net.rx_bytes", "port-a", 100, T0 + 30_000, Origin::Unknown).unwrap();
        l.commit(r).unwrap();
        // A round that is dropped (failed commit) changes nothing...
        let mut lost = l.begin_round(T0 + 60_000).unwrap();
        lost.counter(&s, "net.rx_bytes", "port-a", 250, T0 + 60_000, Origin::Unknown).unwrap();
        drop(lost);
        // ...and the next round recomputes from the old cursor.
        let mut r = l.begin_round(T0 + 90_000).unwrap();
        r.counter(&s, "net.rx_bytes", "port-a", 400, T0 + 90_000, Origin::Unknown).unwrap();
        l.commit(r).unwrap();
        assert_eq!(total(&l, "net.rx_bytes"), 400);
    }

    #[test]
    fn random_readings_total_equals_last_value() {
        let (_d, l) = ledger();
        let s = vm();
        let mut rng = Rng(42);
        let mut t = T0;
        let mut v = 0u64;
        let mut r = l.begin_round(t).unwrap();
        r.counter(&s, "disk.read_ops", "i1", 0, t, Origin::ZeroAt(t)).unwrap();
        for i in 0..500 {
            t += 1 + rng.next() % 600_000; // up to 10 minutes: gaps included
            v += rng.next() % 50_000;
            r.counter(&s, "disk.read_ops", "i1", v, t, Origin::Unknown).unwrap();
            if i % 7 == 0 {
                let mut next = l.begin_round(t).unwrap();
                std::mem::swap(&mut r, &mut next);
                l.commit(next).unwrap();
            }
        }
        r.now = t + 10 * H; // close everything
        l.commit(r).unwrap();
        assert_eq!(total(&l, "disk.read_ops"), v);
        assert!(rows(&l).iter().all(|row| row.written_at > 0), "all hours closed");
    }

    #[test]
    fn gauge_integrates_level_times_seconds() {
        let (_d, l) = ledger();
        let s = vm();
        let mut r = l.begin_round(T0 + H).unwrap();
        r.gauge(&s, "mem.alloc", 2048, T0).unwrap();
        r.gauge(&s, "mem.alloc", 4096, T0 + 30_000).unwrap(); // 2048 MiB × 30 s
        r.gauge_end(&s, "mem.alloc", T0 + 60_000).unwrap(); //   4096 MiB × 30 s
        // Ended: no more accrual.
        r.gauge(&s, "cpu.alloc", 2, T0 + H - 1_000).unwrap();
        l.commit(r).unwrap();
        assert_eq!(total(&l, "mem.alloc"), 2048 * 30 + 4096 * 30);
        assert_eq!(total(&l, "cpu.alloc"), 0);
    }

    /// Found end to end (2026-10-05): rounds are ~30 s plus jitter, and a
    /// level-1 gauge truncated each interval to whole seconds, losing up
    /// to a second per round. The remainder is carried instead.
    #[test]
    fn gauges_lose_nothing_to_rounding() {
        let (_d, l) = ledger();
        let s = vm();
        let mut rng = Rng(7);
        let mut t = T0;
        let mut r = l.begin_round(t).unwrap();
        r.gauge_run(&s, "vm.running", "i1", T0, 1, T0, T0, None).unwrap();
        for i in 0..400 {
            t += 30_000 + rng.next() % 900; // 30 s + up to 0.9 s jitter
            r.gauge_run(&s, "vm.running", "i1", T0, 1, T0, t, None).unwrap();
            if i % 9 == 0 {
                let mut next = l.begin_round(t).unwrap();
                std::mem::swap(&mut r, &mut next);
                l.commit(next).unwrap();
            }
        }
        l.commit(r).unwrap();
        assert_eq!(total(&l, "vm.running"), (t - T0) / 1000, "whole seconds of the span, none lost");
    }

    #[test]
    fn gauge_since_switches_level_at_the_transition() {
        let (_d, l) = ledger();
        let s = vm();
        let mut r = l.begin_round(T0 + H).unwrap();
        r.gauge(&s, "cpu.alloc", 4, T0).unwrap();
        // Paused at +10 s, seen at +30 s: 4 vCPU × 10 s, then 0.
        r.gauge_since(&s, "cpu.alloc", 0, T0 + 10_000, T0 + 30_000).unwrap();
        // The same transition seen again: no double count.
        r.gauge_since(&s, "cpu.alloc", 0, T0 + 10_000, T0 + 60_000).unwrap();
        // Resumed at +70 s, seen at +90 s: 4 × 20 s.
        r.gauge_since(&s, "cpu.alloc", 4, T0 + 70_000, T0 + 90_000).unwrap();
        // Exited at +100 s, seen at +120 s.
        r.gauge_end_at(&s, "cpu.alloc", T0 + 100_000).unwrap();
        r.gauge_end_at(&s, "cpu.alloc", T0 + 120_000).unwrap(); // already ended
        l.commit(r).unwrap();
        assert_eq!(total(&l, "cpu.alloc"), 4 * 10 + 4 * 20 + 4 * 10);
    }

    #[test]
    fn gauge_runs_do_not_accrue_across_a_restart_gap() {
        let (_d, l) = ledger();
        let s = vm();
        let mut r = l.begin_round(T0 + H).unwrap();
        // Instance i1 launched at T0, seen at +30 s.
        r.gauge_run(&s, "mem.alloc", "i1", T0, 1024, T0, T0 + 30_000, None).unwrap();
        r.gauge_run(&s, "mem.alloc", "i1", T0, 1024, T0, T0 + 60_000, None).unwrap();
        // Control plane down; i1 exited at +100 s, i2 launched at +500 s;
        // seen at +530 s. Charged: i1 to its exit, i2 from its launch.
        r.gauge_run(&s, "mem.alloc", "i2", T0 + 500_000, 1024, T0 + 500_000, T0 + 530_000, Some(T0 + 100_000)).unwrap();
        l.commit(r).unwrap();
        assert_eq!(total(&l, "mem.alloc"), 1024 * 100 + 1024 * 30);
    }

    #[test]
    fn counter_parts_add_into_one_meter_and_prune() {
        let (_d, l) = ledger();
        let net = Subject::new(SubjectKind::Network, "nat", "nat", Some("p".into()));
        let mut r = l.begin_round(T0 + 60_000).unwrap();
        for (part, v0, v1) in [("port-a", 100, 150), ("port-b", 1000, 1300)] {
            r.counter_part(&net, "bridge.bytes", part, part, v0, T0, Origin::ZeroAt(T0)).unwrap();
            r.counter_part(&net, "bridge.bytes", part, part, v1, T0 + 30_000, Origin::Unknown).unwrap();
        }
        l.commit(r).unwrap();
        assert_eq!(total(&l, "bridge.bytes"), 1450);
        // port-a went away: its cursor is dropped, port-b's is kept.
        let mut r = l.begin_round(T0 + 90_000).unwrap();
        r.prune_parts(&net, "bridge.bytes", &["port-b".to_string()].into()).unwrap();
        r.counter_part(&net, "bridge.bytes", "port-b", "port-b", 1400, T0 + 60_000, Origin::Unknown).unwrap();
        l.commit(r).unwrap();
        assert_eq!(total(&l, "bridge.bytes"), 1550);
        let r = l.begin_round(T0 + 91_000).unwrap();
        let txn = r.db.begin_read().unwrap();
        let keys: Vec<String> = txn.open_table(CURSORS).unwrap().iter().unwrap().map(|e| e.unwrap().0.value().to_string()).collect();
        assert_eq!(keys, vec!["network/nat/bridge.bytes#port-b".to_string()]);
    }

    #[test]
    fn forgotten_subjects_lose_their_cursors() {
        let (_d, l) = ledger();
        let a = Subject::new(SubjectKind::Disk, "a", "a", Some("p".into()));
        let b = Subject::new(SubjectKind::Disk, "b", "b", Some("p".into()));
        let mut r = l.begin_round(T0).unwrap();
        r.gauge(&a, "disk.alloc", 1024, T0).unwrap();
        r.gauge(&b, "disk.alloc", 1024, T0).unwrap();
        l.commit(r).unwrap();
        assert_eq!(l.cursor_subjects(&[SubjectKind::Disk]).unwrap(), ["disk/a".to_string(), "disk/b".to_string()].into());
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        r.forget_subject("disk/a");
        r.gauge(&b, "disk.alloc", 1024, T0 + 30_000).unwrap();
        l.commit(r).unwrap();
        assert_eq!(l.cursor_subjects(&[SubjectKind::Disk, SubjectKind::Vm]).unwrap(), ["disk/b".to_string()].into());
        assert_eq!(total(&l, "disk.alloc"), 1024 * 30);
    }

    #[test]
    fn hours_close_with_grace_and_late_deltas_become_adjustments() {
        let (_d, l) = ledger();
        let s = vm();
        let mut r = l.begin_round(T0 + H + 60_000).unwrap();
        r.counter(&s, "cpu.used", "i1", 0, T0, Origin::ZeroAt(T0)).unwrap();
        r.counter(&s, "cpu.used", "i1", 10, T0 + 30_000, Origin::Unknown).unwrap();
        l.commit(r).unwrap();
        // Within the 120 s grace: still open.
        assert!(rows(&l).iter().all(|r| r.written_at == 0));
        let r = l.begin_round(T0 + H + 121_000).unwrap();
        l.commit(r).unwrap();
        let closed = rows(&l);
        assert_eq!(closed.len(), 1);
        assert_eq!((closed[0].seq, closed[0].written_at > 0), (0, true));
        assert_eq!(l.complete_through().unwrap(), (T0 + H) / 1000);
        // A late delta for the closed hour (e.g. an exit snapshot): adjustment row.
        let mut r = l.begin_round(T0 + H + 150_000).unwrap();
        r.counter_final(&s, "cpu.used", "i1", 15, T0 + 40_000).unwrap();
        l.commit(r).unwrap();
        let rows = rows(&l);
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(combine(&rows)["cpu.used"], 15);
    }

    #[test]
    fn peaks_take_the_max_and_skip_gaps() {
        let (_d, l) = ledger();
        let s = vm();
        let mut r = l.begin_round(T0 + 120_000).unwrap();
        r.counter(&s, "net.rx_bytes", "p", 0, T0, Origin::ZeroAt(T0)).unwrap();
        for (v, t) in [(3_000_000u64, 30_000u64), (3_300_000, 60_000), (33_300_000, 600_000)] {
            let d = r.counter(&s, "net.rx_bytes", "p", v, T0 + t, Origin::Unknown).unwrap().unwrap();
            if let Some(kbps) = d.rate(8) {
                r.max(&s, "net.rx_kbps_peak", kbps / 1000, T0 + t);
            }
        }
        l.commit(r).unwrap();
        // 3 MB in 30 s = 800 kbps; 0.3 MB = 80 kbps; the 9-minute gap is skipped.
        assert_eq!(combine(&rows(&l))["net.rx_kbps_peak"], 800);
    }

    #[test]
    fn scan_filters_by_project_and_reopens_cleanly() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        {
            let db = Arc::new(Database::create(&path).unwrap());
            let l = Ledger::new(db, LedgerSettings::from_secs(30, 0)).unwrap();
            l.started_at().unwrap();
            let mut r = l.begin_round(T0 + 2 * H).unwrap();
            for (p, id) in [("p1", "a"), ("p2", "b")] {
                let s = Subject::new(SubjectKind::Vm, id, id, Some(p.into()));
                r.gauge(&s, "vm.running", 1, T0).unwrap();
                r.gauge(&s, "vm.running", 1, T0 + H).unwrap();
            }
            l.commit(r).unwrap();
        }
        let db = Arc::new(Database::create(&path).unwrap());
        let l = Ledger::new(db, LedgerSettings::from_secs(30, 0)).unwrap();
        let only: BTreeSet<String> = ["p2".to_string()].into();
        let rows = l.scan(T0 / 1000, (T0 + H) / 1000, Some(&only)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].subject.id.as_str(), rows[0].meters["vm.running"]), ("b", 3600));
    }
}

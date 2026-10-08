//! External-traffic counters on OVN networks (spec/clustering.md §13.2–13.3,
//! D22): conntrack accounting in each router's SNAT zone, on the gateway
//! node. A connection SNATed for VM address A carries A's outbound bytes in
//! its original direction and A's inbound bytes in the reply direction.
//!
//! The collector keeps **cumulative** per-(zone, address) totals and one
//! baseline per live connection:
//!
//! - a dump of the zones (once per metering round) adds each connection's
//!   growth since its last seen counters;
//! - a `DESTROY` event adds the growth up to the final counters and drops
//!   the baseline, so a connection that lives between two dumps is still
//!   counted;
//! - the state is persisted by the caller before it is reported, so the
//!   totals stay monotonic across restarts. A connection that opens and
//!   closes entirely while the collector is down is never seen: `gaps`
//!   counts such restarts so the ledger can flag the hour (`ext_gap`).
//!
//! Bytes are L2-normalized like `nat_counters`: L3 bytes + 14 per packet,
//! so they match OVS's frame counts.

use crate::exec::{Cmd, Exec, Program};
use crate::nat_meter::ETHERNET_HEADER;
use crate::OvsError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const ACCT_SYSCTL: &str = "net.netfilter.nf_conntrack_acct";
const ACCT_PROC: &str = "/proc/sys/net/netfilter/nf_conntrack_acct";

/// One conntrack entry, reduced to what metering needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtEntry {
    pub id: u64,
    pub zone: u16,
    /// The VM: the original tuple's source.
    pub vm: Ipv4Addr,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub rx_packets: u64,
}

/// Parse a line of `conntrack -L|-E -o extended,id` (IPv4). Lines without
/// counters (accounting off), a zone or an id are not entries.
pub fn parse_line(line: &str) -> Option<CtEntry> {
    let (mut srcs, mut packets, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
    let (mut zone, mut id) = (None, None);
    for tok in line.split_whitespace() {
        let Some((k, v)) = tok.split_once('=') else { continue };
        match k {
            "src" => srcs.push(v),
            "packets" => packets.push(v.parse::<u64>().ok()?),
            "bytes" => bytes.push(v.parse::<u64>().ok()?),
            "zone" => zone = v.parse::<u16>().ok(),
            // ICMP tuples carry their own `id=`; the entry's is the last.
            "id" => id = v.parse::<u64>().ok(),
            _ => {}
        }
    }
    if srcs.len() < 2 || packets.len() < 2 || bytes.len() < 2 {
        return None;
    }
    Some(CtEntry {
        id: id?,
        zone: zone?,
        vm: srcs[0].parse().ok()?,
        tx_bytes: bytes[0],
        tx_packets: packets[0],
        rx_bytes: bytes[1],
        rx_packets: packets[1],
    })
}

/// Cumulative counters of one VM address in one zone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtCounter {
    pub zone: u16,
    pub ip: Option<Ipv4Addr>,
    /// Out of the VM (guest tx), L2-normalized.
    pub tx_bytes: u64,
    pub tx_packets: u64,
    /// Into the VM (guest rx), L2-normalized.
    pub rx_bytes: u64,
    pub rx_packets: u64,
}

/// What `ct_external_counters` returns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtCounters {
    /// Changes when the totals started over (lost state): metering's reset rule.
    pub epoch: u64,
    /// Times the collector was down while connections could open and close.
    pub gaps: u64,
    pub counters: Vec<CtCounter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Seen {
    zone: u16,
    vm: Ipv4Addr,
    tx_bytes: u64,
    tx_packets: u64,
    rx_bytes: u64,
    rx_packets: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    epoch: u64,
    gaps: u64,
    /// `<zone>/<address>`: JSON keys are text.
    totals: BTreeMap<String, CtCounter>,
    seen: BTreeMap<u64, Seen>,
}

/// The collector. Cheap to share; every method takes `&self`.
pub struct CtMeter {
    zones: (u16, u16),
    state: Mutex<State>,
}

impl CtMeter {
    /// A collector for the router zones `zones` (inclusive). `saved` is the
    /// state persisted by an earlier run; starting from it records a gap,
    /// starting without it begins a new epoch.
    pub fn new(zones: (u16, u16), saved: Option<&str>) -> Self {
        let state = match saved.and_then(|s| serde_json::from_str::<State>(s).ok()) {
            Some(mut st) => {
                st.gaps += 1;
                st
            }
            None => State { epoch: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(1), ..Default::default() },
        };
        Self { zones, state: Mutex::new(state) }
    }

    fn mine(&self, e: &CtEntry) -> bool {
        (self.zones.0..=self.zones.1).contains(&e.zone)
    }

    fn add(st: &mut State, e: &CtEntry, closed: bool) {
        let prev = st.seen.get(&e.id).filter(|p| p.zone == e.zone && p.vm == e.vm && p.tx_bytes <= e.tx_bytes && p.rx_bytes <= e.rx_bytes && p.tx_packets <= e.tx_packets && p.rx_packets <= e.rx_packets).cloned();
        // A counter that went backwards is a different connection under a reused id.
        let (tb, tp, rb, rp) = match &prev {
            Some(p) => (e.tx_bytes - p.tx_bytes, e.tx_packets - p.tx_packets, e.rx_bytes - p.rx_bytes, e.rx_packets - p.rx_packets),
            None => (e.tx_bytes, e.tx_packets, e.rx_bytes, e.rx_packets),
        };
        let c = st.totals.entry(format!("{}/{}", e.zone, e.vm)).or_insert_with(|| CtCounter { zone: e.zone, ip: Some(e.vm), ..Default::default() });
        c.tx_bytes += tb + tp * ETHERNET_HEADER;
        c.tx_packets += tp;
        c.rx_bytes += rb + rp * ETHERNET_HEADER;
        c.rx_packets += rp;
        if closed {
            st.seen.remove(&e.id);
        } else {
            st.seen.insert(e.id, Seen { zone: e.zone, vm: e.vm, tx_bytes: e.tx_bytes, tx_packets: e.tx_packets, rx_bytes: e.rx_bytes, rx_packets: e.rx_packets });
        }
    }

    /// Account a dump of the zones. A connection missing from it closed
    /// without an event we saw; it was counted up to its last dump.
    pub fn ingest_dump(&self, entries: &[CtEntry]) {
        let mut st = self.state.lock().unwrap();
        let live: std::collections::BTreeSet<u64> = entries.iter().filter(|e| self.mine(e)).map(|e| e.id).collect();
        st.seen.retain(|id, _| live.contains(id));
        for e in entries.iter().filter(|e| self.mine(e)) {
            Self::add(&mut st, e, false);
        }
    }

    /// Account the final counters of a closed connection.
    pub fn ingest_destroy(&self, line: &str) {
        let Some(e) = parse_line(line).filter(|e| self.mine(e)) else { return };
        Self::add(&mut self.state.lock().unwrap(), &e, true);
    }

    pub fn counters(&self) -> CtCounters {
        let st = self.state.lock().unwrap();
        CtCounters { epoch: st.epoch, gaps: st.gaps, counters: st.totals.values().cloned().collect() }
    }

    /// The state to persist before reporting it.
    pub fn save(&self) -> Result<String, OvsError> {
        serde_json::to_string(&*self.state.lock().unwrap()).map_err(|e| OvsError::Io(format!("conntrack counters: {e}")))
    }
}

/// Dump the router zones with the `conntrack` tool.
pub fn dump(exec: &dyn Exec) -> Result<Vec<CtEntry>, OvsError> {
    let out = exec.check(&Cmd::new(Program::Conntrack, ["-L", "-f", "ipv4", "-o", "extended,id"]))?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(parse_line).collect())
}

/// One collection round: dump, account, persist (via `persist`), report.
pub fn collect(exec: &dyn Exec, meter: &CtMeter, persist: impl FnOnce(&str) -> Result<(), OvsError>) -> Result<CtCounters, OvsError> {
    meter.ingest_dump(&dump(exec)?);
    persist(&meter.save()?)?;
    Ok(meter.counters())
}

/// Turn conntrack accounting on (it is off by default). Returns whether
/// glidex changed it, so uninstall can put it back.
pub fn ensure_accounting(exec: &dyn Exec) -> Result<bool, OvsError> {
    if exec.read_file(Path::new(ACCT_PROC)).map(|s| s.trim() == "1").unwrap_or(false) {
        return Ok(false);
    }
    exec.check(&Cmd::new(Program::Sysctl, ["-w", &format!("{ACCT_SYSCTL}=1")]))?;
    Ok(true)
}

/// Follow `DESTROY` events in a thread, restarting the reader if it dies
/// (each restart is a gap). Not used in tests.
pub fn spawn_event_reader(meter: Arc<CtMeter>) {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let _ = std::thread::Builder::new().name("ct-events".into()).spawn(move || loop {
        let child = Command::new(Program::Conntrack.name()).args(["-E", "-e", "DESTROY", "-f", "ipv4", "-o", "extended,id"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        match child {
            Ok(mut c) => {
                if let Some(out) = c.stdout.take() {
                    for line in BufRead::lines(BufReader::new(out)).map_while(Result::ok) {
                        meter.ingest_destroy(&line);
                    }
                }
                let _ = c.wait();
            }
            Err(e) => tracing_log(&format!("conntrack events: {e}")),
        }
        meter.state.lock().unwrap().gaps += 1;
        std::thread::sleep(std::time::Duration::from_secs(5));
    });
}

fn tracing_log(m: &str) {
    eprintln!("{m}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: u64, zone: u16, vm: &str, tx: (u64, u64), rx: (u64, u64)) -> String {
        format!(
            "ipv4     2 tcp      6 431999 ESTABLISHED src={vm} dst=203.0.113.9 sport=44444 dport=80 packets={} bytes={} src=203.0.113.9 dst=192.0.2.3 sport=80 dport=44444 packets={} bytes={} [ASSURED] mark=0 zone={zone} use=1 id={id}",
            tx.0, tx.1, rx.0, rx.1
        )
    }

    fn e(l: &str) -> CtEntry {
        parse_line(l).unwrap()
    }

    #[test]
    fn a_line_gives_the_vm_and_both_directions() {
        let x = e(&line(7, 60001, "10.89.0.5", (10, 1000), (8, 9000)));
        assert_eq!((x.id, x.zone, x.vm.to_string()), (7, 60001, "10.89.0.5".into()));
        assert_eq!((x.tx_packets, x.tx_bytes, x.rx_packets, x.rx_bytes), (10, 1000, 8, 9000));
        // ICMP has its own id= in the tuples; the entry's is the last one.
        let icmp = "ipv4 2 icmp 1 29 src=10.89.0.5 dst=203.0.113.9 type=8 code=0 id=21 packets=1 bytes=84 src=203.0.113.9 dst=192.0.2.3 type=0 code=0 id=21 packets=1 bytes=84 mark=0 zone=60001 use=1 id=99";
        assert_eq!(e(icmp).id, 99);
        // Accounting off: no counters, not an entry.
        assert!(parse_line("ipv4 2 tcp 6 10 ESTABLISHED src=10.0.0.1 dst=10.0.0.2 sport=1 dport=2 src=10.0.0.2 dst=10.0.0.1 sport=2 dport=1 zone=60001 id=1").is_none());
    }

    #[test]
    fn dumps_add_growth_and_destroy_adds_the_rest_once() {
        let m = CtMeter::new((60000, 60010), None);
        m.ingest_dump(&[e(&line(1, 60001, "10.89.0.5", (10, 1000), (8, 9000)))]);
        m.ingest_dump(&[e(&line(1, 60001, "10.89.0.5", (20, 2000), (16, 18000)))]);
        let c = m.counters();
        // L2-normalized: bytes + 14 per packet.
        assert_eq!((c.counters[0].tx_bytes, c.counters[0].tx_packets), (2000 + 20 * 14, 20));
        assert_eq!((c.counters[0].rx_bytes, c.counters[0].rx_packets), (18000 + 16 * 14, 16));
        // The connection closes between dumps: its final counters count once.
        m.ingest_destroy(&line(1, 60001, "10.89.0.5", (25, 2600), (20, 20000)));
        // A short connection never dumped counts in full.
        m.ingest_destroy(&line(2, 60001, "10.89.0.5", (3, 300), (3, 300)));
        let c = &m.counters().counters[0];
        assert_eq!(c.tx_packets, 28);
        assert_eq!(c.tx_bytes, 2600 + 300 + 28 * 14);
    }

    #[test]
    fn other_zones_and_counters_that_went_backwards_are_handled() {
        let m = CtMeter::new((60000, 60010), None);
        m.ingest_dump(&[e(&line(1, 50, "10.0.0.1", (1, 100), (1, 100))), e(&line(2, 60002, "10.89.0.6", (1, 100), (1, 100)))]);
        assert_eq!(m.counters().counters.len(), 1, "a zone that isn't a router's isn't metered");
        // The id is reused by a new connection: counted from zero, not negative.
        m.ingest_dump(&[e(&line(2, 60002, "10.89.0.6", (1, 50), (1, 50)))]);
        assert_eq!(m.counters().counters[0].tx_packets, 2);
        // Two VMs in a zone are separate counters.
        m.ingest_dump(&[e(&line(3, 60002, "10.89.0.7", (2, 200), (2, 200)))]);
        assert_eq!(m.counters().counters.len(), 2);
    }

    #[test]
    fn restarts_keep_totals_and_record_a_gap_while_lost_state_starts_a_new_epoch() {
        let m = CtMeter::new((60000, 60010), None);
        m.ingest_dump(&[e(&line(1, 60001, "10.89.0.5", (10, 1000), (8, 9000)))]);
        let (saved, before) = (m.save().unwrap(), m.counters());
        let again = CtMeter::new((60000, 60010), Some(&saved));
        let after = again.counters();
        assert_eq!((after.epoch, after.counters.clone()), (before.epoch, before.counters.clone()));
        assert_eq!((before.gaps, after.gaps), (0, 1));
        // The surviving connection keeps its baseline: no double counting.
        again.ingest_dump(&[e(&line(1, 60001, "10.89.0.5", (10, 1000), (8, 9000)))]);
        assert_eq!(again.counters().counters, before.counters);
        let lost = CtMeter::new((60000, 60010), Some("not json"));
        assert!(lost.counters().counters.is_empty() && lost.counters().epoch > 0);
    }

    #[test]
    fn collect_dumps_persists_then_reports_and_accounting_is_turned_on() {
        let ex = crate::exec::RecordingExec::new();
        ex.on("conntrack -L -f ipv4 -o extended,id", crate::exec::Output::ok(line(4, 60001, "10.89.0.5", (2, 200), (2, 200)).as_str()));
        let m = CtMeter::new((60000, 60010), None);
        let mut saved = String::new();
        let c = collect(&ex, &m, |s| {
            saved = s.to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(c.counters[0].tx_packets, 2);
        assert!(saved.contains("10.89.0.5"));
        assert!(ensure_accounting(&ex).unwrap());
        assert!(ex.calls().iter().any(|c| c == "sysctl -w net.netfilter.nf_conntrack_acct=1"), "{:?}", ex.calls());
    }
}

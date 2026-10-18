//! What a console client is replayed (spec console.md "Log files").
//!
//! The log holds the guest's queries to its terminal: systemd probes a
//! serial console's size (`CSI 6n`) and name (`DCS + q`) at each getty
//! start. A terminal that is replayed them answers each one now, and the
//! guest reads the stale answers as typed input (`^[[38;1R` at the shell
//! prompt). Replay therefore leaves them out; everything else, and the
//! live output, passes through unchanged. A query cut off at the end of
//! the log is passed on: the rest arrives live, and the guest is waiting
//! for that answer.

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// `log` without the terminal queries in it.
pub fn strip_queries(log: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(log.len());
    let mut i = 0;
    while i < log.len() {
        let end = match (log[i], log.get(i + 1)) {
            (ESC, Some(b'[')) => csi_query(log, i + 2),
            (ESC, Some(b'P')) => dcs_query(log, i + 2),
            (ESC, Some(b']')) => osc_query(log, i + 2),
            _ => None,
        };
        match end {
            Some(end) => i = end,
            None => {
                out.push(log[i]);
                i += 1;
            }
        }
    }
    out
}

/// The end of the CSI sequence whose body starts at `at`, if it is a query.
fn csi_query(log: &[u8], at: usize) -> Option<usize> {
    let mut i = at;
    while i < log.len() && (0x30..=0x3f).contains(&log[i]) {
        i += 1;
    }
    let params = &log[at..i];
    let mid = i;
    while i < log.len() && (0x20..=0x2f).contains(&log[i]) {
        i += 1;
    }
    let inter = &log[mid..i];
    let fin = *log.get(i).filter(|b| (0x40..=0x7e).contains(*b))?;
    let query = match (fin, inter) {
        // DSR: status, cursor position, and the DEC private reports.
        (b'n', []) => true,
        // DA1, DA2 (`>`), DA3 (`=`).
        (b'c', []) => true,
        // DECRQM, ANSI and DEC private modes.
        (b'p', b"$") => true,
        // XTVERSION.
        (b'q', []) => params.starts_with(b">"),
        // The kitty keyboard flags.
        (b'u', []) => params == b"?",
        // XTWINOPS reports (sizes, positions, state, title).
        (b't', []) => {
            matches!(
                params,
                b"11" | b"13" | b"14" | b"15" | b"16" | b"18" | b"19" | b"20" | b"21"
            ) || params.starts_with(b"13;")
                || params.starts_with(b"14;")
        }
        _ => false,
    };
    query.then_some(i + 1)
}

/// The end of the DCS string whose body starts at `at`, if it is a query:
/// XTGETTCAP (`+q`) or DECRQSS (`$q`).
fn dcs_query(log: &[u8], at: usize) -> Option<usize> {
    let body = log.get(at..at + 2)?;
    if body != b"+q" && body != b"$q" {
        return None;
    }
    string_end(log, at, false)
}

/// The end of the OSC string whose body starts at `at`, if it asks for a
/// colour (4, 5, 10-19) or the clipboard (52): a `?` field.
fn osc_query(log: &[u8], at: usize) -> Option<usize> {
    let end = string_end(log, at, true)?;
    let string = &log[at..end];
    let body = string
        .strip_suffix(&[ESC, b'\\'])
        .or_else(|| string.strip_suffix(&[BEL]))?;
    let mut fields = body.split(|&b| b == b';');
    let ps = fields.next()?;
    let colour_or_clipboard = matches!(
        ps,
        b"4" | b"5"
            | b"10"
            | b"11"
            | b"12"
            | b"13"
            | b"14"
            | b"15"
            | b"16"
            | b"17"
            | b"18"
            | b"19"
            | b"52"
    );
    (colour_or_clipboard && fields.any(|f| f == b"?")).then_some(end)
}

/// The end of a control string starting at `at`, past its ST (or BEL, for
/// OSC). `None` if it is not terminated: cut off, or abandoned by another
/// escape sequence.
fn string_end(log: &[u8], at: usize, bel: bool) -> Option<usize> {
    let mut i = at;
    while i < log.len() {
        match log[i] {
            BEL if bel => return Some(i + 1),
            ESC => return (log.get(i + 1) == Some(&b'\\')).then_some(i + 2),
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::strip_queries;

    fn strip(s: &[u8]) -> Vec<u8> {
        strip_queries(s)
    }

    #[test]
    fn systemds_serial_probes_are_left_out() {
        // As in a real console log: size and name probes around a prompt.
        let log =
            b"\x1b7\x1b[32766;32766H\x1b[6n\x1b8\x1bP+q6E616D65\x1b\\\x1b]104\x1b\\vm login: ";
        assert_eq!(
            strip(log),
            b"\x1b7\x1b[32766;32766H\x1b8\x1b]104\x1b\\vm login: "
        );
    }

    #[test]
    fn every_kind_of_query_is_left_out() {
        for q in [
            &b"\x1b[5n"[..],
            b"\x1b[?6n",
            b"\x1b[c",
            b"\x1b[0c",
            b"\x1b[>c",
            b"\x1b[=0c",
            b"\x1b[?2004$p",
            b"\x1b[4$p",
            b"\x1b[>0q",
            b"\x1b[?u",
            b"\x1b[18t",
            b"\x1b[14;2t",
            b"\x1bP$qm\x1b\\",
            b"\x1b]11;?\x07",
            b"\x1b]4;1;?;2;?\x1b\\",
            b"\x1b]52;c;?\x07",
        ] {
            let log = [b"a", q, b"b"].concat();
            assert_eq!(strip(&log), b"ab", "{:?}", String::from_utf8_lossy(q));
        }
    }

    #[test]
    fn output_that_only_looks_alike_is_kept() {
        for s in [
            &b"\x1b[1;31mred\x1b[0m"[..],
            b"\x1b[2J\x1b[H",
            b"\x1b[?1049h\x1b[?25l",
            b"\x1b[ q",       // DECSCUSR, not XTVERSION
            b"\x1b[>1u",      // kitty keyboard push
            b"\x1b[8;24;80t", // a resize, not a report
            b"\x1b]0;?\x07",  // a title that is a question mark
            b"\x1b]11;#000000\x07",
            b"\x1bPq#0;2;0;0;0\x1b\\", // sixel
            "caf\u{e9}".as_bytes(),
        ] {
            assert_eq!(strip(s), s, "{:?}", String::from_utf8_lossy(s));
        }
    }

    #[test]
    fn a_query_cut_off_at_the_end_is_passed_on() {
        for s in [&b"x\x1b[6"[..], b"x\x1b", b"x\x1bP+q6E", b"x\x1b]11;?\x1b"] {
            assert_eq!(strip(s), s);
        }
    }

    #[test]
    fn an_abandoned_string_does_not_swallow_what_follows() {
        let log = b"\x1bP+q6E\x1b[6nafter";
        assert_eq!(strip(log), b"\x1bP+q6Eafter");
    }
}

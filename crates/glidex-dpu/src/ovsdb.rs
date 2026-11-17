//! Parsing `ovs-vsctl`'s text output: scalars (`"3.5.0098"`), maps
//! (`{doca-init="true", hw-offload="true"}`), sets (`[doca, netdev]`) and
//! the records of `ovs-vsctl --columns=… list <table>`.

use std::collections::BTreeMap;

/// A scalar with its quotes removed; `[]` (the empty set) is `None`.
pub fn scalar(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s == "[]" {
        return None;
    }
    Some(unquote(s))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        s.to_string()
    }
}

/// Split on `sep` outside double quotes.
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                cur.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c == sep && !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// `{k=v, k2="v 2"}` → map. Anything that isn't a map is empty.
pub fn map(s: &str) -> BTreeMap<String, String> {
    let s = s.trim();
    let Some(inner) = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return BTreeMap::new();
    };
    split_top(inner, ',')
        .into_iter()
        .filter_map(|kv| {
            let kv = kv.trim();
            let parts = split_top(kv, '=');
            // A value may itself hold `=` (an `ovn-remote` list does not, a
            // DPDK devargs string does): rejoin all but the key.
            let (k, v) = parts.split_first()?;
            Some((unquote(k), unquote(&v.join("="))))
        })
        .collect()
}

/// `[a, "b c"]` (or a bare scalar) → list.
pub fn set(s: &str) -> Vec<String> {
    let s = s.trim();
    match s.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(inner) => split_top(inner, ',')
            .into_iter()
            .map(|v| unquote(&v))
            .filter(|v| !v.is_empty())
            .collect(),
        None => scalar(s).into_iter().collect(),
    }
}

/// The records of `ovs-vsctl --columns=… list <table>`: blank-line
/// separated blocks of `column : value`. Values are kept raw, for
/// [`scalar`], [`map`] or [`set`].
pub fn records(s: &str) -> Vec<BTreeMap<String, String>> {
    let mut out = Vec::new();
    let mut cur = BTreeMap::new();
    for line in s.lines() {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            cur.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The rows of `ovs-vsctl --format=json … list|find <table>`, each column
/// flattened to text: strings as they are, sets as [`set`] would print
/// them, maps as `k=v` pairs (read them with [`json_map`]).
pub fn json_rows(s: &str) -> Result<Vec<BTreeMap<String, serde_json::Value>>, crate::DpuError> {
    let v: serde_json::Value = serde_json::from_str(s)
        .map_err(|e| crate::DpuError::parse("ovs-vsctl json", e.to_string()))?;
    let headings: Vec<String> = v["headings"]
        .as_array()
        .map(|h| {
            h.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(v["data"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.as_array())
                .map(|r| headings.iter().cloned().zip(r.iter().cloned()).collect())
                .collect()
        })
        .unwrap_or_default())
}

/// A string column of a [`json_rows`] row; an empty optional column is `None`.
pub fn json_str(row: &BTreeMap<String, serde_json::Value>, column: &str) -> Option<String> {
    use serde_json::Value;
    match row.get(column)? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) if a.first().and_then(Value::as_str) == Some("set") => {
            a.get(1)?.as_array()?.first()?.as_str().map(str::to_string)
        }
        _ => None,
    }
}

/// A map column of a [`json_rows`] row (`["map", [[k, v], …]]`).
pub fn json_map(
    row: &BTreeMap<String, serde_json::Value>,
    column: &str,
) -> BTreeMap<String, String> {
    use serde_json::Value;
    let mut out = BTreeMap::new();
    if let Some(Value::Array(a)) = row.get(column)
        && a.first().and_then(Value::as_str) == Some("map")
        && let Some(Value::Array(pairs)) = a.get(1)
    {
        for pair in pairs {
            let k = pair.get(0).and_then(Value::as_str);
            let v = pair.get(1).map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            if let (Some(k), Some(v)) = (k, v) {
                out.insert(k.to_string(), v);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars_maps_and_sets() {
        assert_eq!(scalar("\"3.5.0098\"\n").as_deref(), Some("3.5.0098"));
        assert_eq!(scalar("true").as_deref(), Some("true"));
        assert_eq!(scalar("[]"), None);
        assert_eq!(scalar(""), None);

        let m = map(
            r#"{doca-init="true", dpdk-hugepage-dir="/hugepages-2M", hw-offload="true", pmd-cpu-mask="0xc0"}"#,
        );
        assert_eq!(m["doca-init"], "true");
        assert_eq!(m["dpdk-hugepage-dir"], "/hugepages-2M");
        assert_eq!(m["pmd-cpu-mask"], "0xc0");
        let m = map(r#"{glidex-owner=glidex, iface-id="gx-0123abcd-0", note="a, b=c"}"#);
        assert_eq!(m["iface-id"], "gx-0123abcd-0");
        assert_eq!(m["note"], "a, b=c");
        assert!(map("{}").is_empty());
        assert!(map("").is_empty());

        assert_eq!(set("[doca, netdev, system]"), ["doca", "netdev", "system"]);
        assert_eq!(set("[]"), Vec::<String>::new());
        assert_eq!(set("p0"), ["p0"]);
    }

    #[test]
    fn parses_list_records() {
        let r = records(
            "name                : ovsbr1\ndatapath_type       : doca\nfail_mode           : []\n\n\
             name                : br-int\ndatapath_type       : doca\nfail_mode           : secure\n",
        );
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["name"], "ovsbr1");
        assert_eq!(scalar(&r[0]["fail_mode"]), None);
        assert_eq!(scalar(&r[1]["fail_mode"]).as_deref(), Some("secure"));
    }
}

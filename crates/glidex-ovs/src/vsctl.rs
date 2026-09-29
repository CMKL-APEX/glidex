//! `ovs-vsctl` helpers: ownership tags and parsing of `--format=json`
//! table output.
//!
//! `ovs-vsctl --format=json list <Table>` prints
//! `{"headings": [...], "data": [[...], ...]}` where cells use OVSDB's JSON
//! encoding: plain strings/numbers/booleans, `["uuid", "…"]`,
//! `["set", [...]]` (also used for empty optional columns) and
//! `["map", [[k, v], ...]]`.

use crate::exec::{Cmd, Exec, Program};
use crate::OvsError;
use serde_json::Value;
use std::collections::BTreeMap;

pub const OWNER_KEY: &str = "glidex-owner";
pub const OWNER_VALUE: &str = "glidex";
pub const ROLE_KEY: &str = "glidex-role";
pub const VM_ID_KEY: &str = "glidex-vm-id";
pub const NIC_KEY: &str = "glidex-nic";

/// `external_ids:k=v` arguments for `ovs-vsctl set`.
pub fn tag_args(tags: &[(&str, &str)]) -> Vec<String> {
    tags.iter()
        .map(|(k, v)| format!("external_ids:{}={}", k, v))
        .collect()
}

/// Owner + role tags, plus any extras.
pub fn owned_tags<'a>(role: &'a str, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut tags = vec![(OWNER_KEY, OWNER_VALUE), (ROLE_KEY, role)];
    tags.extend_from_slice(extra);
    tags
}

/// One row of a `list` result, keyed by column name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row(pub BTreeMap<String, Value>);

impl Row {
    pub fn str(&self, column: &str) -> Option<String> {
        match self.0.get(column)? {
            Value::String(s) => Some(s.clone()),
            // Optional string columns are an empty set when unset,
            // and a one-element set can appear too.
            Value::Array(a) if a.first() == Some(&Value::String("set".into())) => {
                a.get(1)?.as_array()?.first()?.as_str().map(str::to_string)
            }
            _ => None,
        }
    }

    pub fn bool(&self, column: &str) -> Option<bool> {
        self.0.get(column)?.as_bool()
    }

    pub fn int(&self, column: &str) -> Option<i64> {
        match self.0.get(column)? {
            Value::Number(n) => n.as_i64(),
            Value::Array(a) if a.first() == Some(&Value::String("set".into())) => {
                a.get(1)?.as_array()?.first()?.as_i64()
            }
            _ => None,
        }
    }

    pub fn map(&self, column: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(Value::Array(a)) = self.0.get(column) {
            if a.first() == Some(&Value::String("map".into())) {
                if let Some(Value::Array(pairs)) = a.get(1) {
                    for pair in pairs {
                        if let (Some(k), Some(v)) = (
                            pair.get(0).and_then(Value::as_str),
                            pair.get(1).and_then(scalar_to_string),
                        ) {
                            out.insert(k.to_string(), v);
                        }
                    }
                }
            }
        }
        out
    }

    /// A set column (or a single scalar) as strings.
    pub fn set(&self, column: &str) -> Vec<String> {
        match self.0.get(column) {
            Some(Value::Array(a)) if a.first() == Some(&Value::String("set".into())) => a
                .get(1)
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(scalar_to_string).collect())
                .unwrap_or_default(),
            Some(v) => scalar_to_string(v).into_iter().collect(),
            None => Vec::new(),
        }
    }

    pub fn owned_by_glidex(&self) -> bool {
        self.map("external_ids").get(OWNER_KEY).map(String::as_str) == Some(OWNER_VALUE)
    }
}

fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Parse `ovs-vsctl --format=json` table output.
pub fn parse_table(json: &str) -> Result<Vec<Row>, OvsError> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| OvsError::Io(format!("unexpected ovs-vsctl output: {}", e)))?;
    let headings: Vec<String> = v
        .get("headings")
        .and_then(Value::as_array)
        .map(|h| h.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(data
        .into_iter()
        .filter_map(|row| {
            let cells = row.as_array()?.clone();
            Some(Row(headings.iter().cloned().zip(cells).collect()))
        })
        .collect())
}

/// `ovs-vsctl --format=json --columns=<cols> list <table>`.
pub fn list(exec: &dyn Exec, table: &str, columns: &[&str]) -> Result<Vec<Row>, OvsError> {
    let cmd = Cmd::new(
        Program::OvsVsctl,
        [
            "--format=json".to_string(),
            format!("--columns={}", columns.join(",")),
            "list".to_string(),
            table.to_string(),
        ],
    );
    parse_table(&exec.check(&cmd)?.stdout_str())
}

/// Rows of `table` whose `name` column equals `name`.
pub fn find_by_name(
    exec: &dyn Exec,
    table: &str,
    name: &str,
    columns: &[&str],
) -> Result<Option<Row>, OvsError> {
    let cmd = Cmd::new(
        Program::OvsVsctl,
        [
            "--format=json".to_string(),
            format!("--columns={}", columns.join(",")),
            "find".to_string(),
            table.to_string(),
            format!("name={}", name),
        ],
    );
    Ok(parse_table(&exec.check(&cmd)?.stdout_str())?.into_iter().next())
}

/// `ovs-vsctl <args…>`, requiring success.
pub fn run(exec: &dyn Exec, args: Vec<String>) -> Result<(), OvsError> {
    exec.check(&Cmd::new(Program::OvsVsctl, args)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIDGES: &str = r#"{"data":[["gxbr-nat",["map",[["glidex-owner","glidex"],["glidex-role","bridge"]]],"system",["set",[]]],["br-int",["map",[]],"netdev",1500]],"headings":["name","external_ids","datapath_type","mtu"]}"#;

    #[test]
    fn parses_rows_maps_and_optional_columns() {
        let rows = parse_table(BRIDGES).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].str("name").as_deref(), Some("gxbr-nat"));
        assert!(rows[0].owned_by_glidex());
        assert_eq!(rows[0].int("mtu"), None);
        assert!(!rows[1].owned_by_glidex());
        assert_eq!(rows[1].str("datapath_type").as_deref(), Some("netdev"));
        assert_eq!(rows[1].int("mtu"), Some(1500));
    }

    #[test]
    fn parses_sets() {
        let json = r#"{"data":[[["set",["afxdp","dpdk","tap"]],false,"3.7.1"]],"headings":["iface_types","dpdk_initialized","ovs_version"]}"#;
        let row = &parse_table(json).unwrap()[0];
        assert_eq!(row.set("iface_types"), vec!["afxdp", "dpdk", "tap"]);
        assert_eq!(row.bool("dpdk_initialized"), Some(false));
        assert_eq!(row.set("ovs_version"), vec!["3.7.1"]);
    }

    #[test]
    fn tag_args_format() {
        assert_eq!(
            tag_args(&owned_tags("vm", &[(VM_ID_KEY, "abc")])),
            vec![
                "external_ids:glidex-owner=glidex",
                "external_ids:glidex-role=vm",
                "external_ids:glidex-vm-id=abc"
            ]
        );
    }
}

//! The glidex RPC API (spec/grpc.md): messages, service traits and
//! clients generated from `proto/`, plus the operation table every RPC
//! is checked against.
//!
//! Messages are `buffa` types; their JSON is the proto3 JSON mapping with
//! the proto field names (G-D11), which is also what REST serves for
//! ported operations.

#[allow(clippy::all, unused_imports, dead_code, non_camel_case_types)]
pub mod proto {
    connectrpc::include_generated!();
}

/// How the cluster treats an operation (spec/grpc.md G-D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Served by any server after a read barrier.
    Read,
    /// Runs on the leader; a follower forwards it.
    Write,
}

/// One RPC, from its `glidex.api.v1.op` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    /// `/glidex.<area>.v1.<Service>/<Method>`, the HTTP path.
    pub procedure: &'static str,
    pub service: &'static str,
    pub method: &'static str,
    /// Cedar action, or `public` / `authenticated`.
    pub action: &'static str,
    pub kind: Kind,
    /// The REST twin: (`GET`, `/auth/server-info`).
    pub rest: Option<(&'static str, &'static str)>,
    pub streaming: bool,
}

include!(concat!(env!("OUT_DIR"), "/operations.rs"));

/// The operation served at `path`.
pub fn operation(path: &str) -> Option<&'static Operation> {
    OPERATIONS.iter().find(|o| o.procedure == path)
}

/// The services, each once, in table order.
pub fn services() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for o in OPERATIONS {
        if !out.contains(&o.service) {
            out.push(o.service);
        }
    }
    out
}

/// Domain of every `google.rpc.ErrorInfo` glidex sends (spec/grpc.md §7).
pub const ERROR_DOMAIN: &str = "glidex";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_the_system_service() {
        let o = operation("/glidex.system.v1.SystemService/GetServerInfo").unwrap();
        assert_eq!(o.action, "public");
        assert_eq!(o.kind, Kind::Read);
        assert_eq!(o.rest, Some(("GET", "/auth/server-info")));
        let w = operation("/glidex.system.v1.SystemService/WhoAmI").unwrap();
        assert_eq!(w.rest, None);
        assert_eq!(services(), vec!["glidex.system.v1.SystemService"]);
    }

    #[test]
    fn json_uses_proto_names_and_keeps_set_falses() {
        use proto::glidex::system::v1::{GetServerInfoResponse, LoginMethods};
        let r = GetServerInfoResponse {
            cluster_name: "lab".into(),
            version: "0.1.0".into(),
            methods: LoginMethods { pam: Some(true), oidc: Some(false), disabled: Some(false), ..Default::default() }.into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["cluster_name"], "lab");
        assert_eq!(v["methods"]["oidc"], false, "{v}");
        assert!(v.get("cluster_id").is_none(), "{v}");
        let back: GetServerInfoResponse = serde_json::from_value(v).unwrap();
        assert_eq!(back.cluster_name, "lab");
    }
}

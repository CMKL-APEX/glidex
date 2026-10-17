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

/// A field of a message's JSON form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    /// The glidex message its values are, if any (a list's items, a map's values).
    pub message: Option<&'static str>,
    pub map: bool,
}

include!(concat!(env!("OUT_DIR"), "/fields.rs"));

/// Whether the message `full_name` has the JSON field `name` (a message
/// this crate doesn't know has none).
pub fn has_field(full_name: &str, name: &str) -> bool {
    MESSAGE_FIELDS
        .binary_search_by(|(n, _)| (*n).cmp(full_name))
        .is_ok_and(|i| MESSAGE_FIELDS[i].1.iter().any(|f| f.name == name))
}

/// The first field of `v` that the message `full_name` doesn't have, as
/// a path (`quotas.vm`). Only glidex messages are checked; well-known
/// types (Struct, Timestamp, …) are taken as they come.
pub fn unknown_field(full_name: &str, v: &serde_json::Value) -> Option<String> {
    let i = MESSAGE_FIELDS
        .binary_search_by(|(n, _)| (*n).cmp(full_name))
        .ok()?;
    let fields = MESSAGE_FIELDS[i].1;
    for (k, val) in v.as_object()? {
        let Some(f) = fields.iter().find(|f| f.name == k) else {
            return Some(k.clone());
        };
        let Some(nested) = f.message else { continue };
        let inner = |path: String, x: &serde_json::Value| {
            unknown_field(nested, x).map(|u| format!("{path}.{u}"))
        };
        let found = match val {
            serde_json::Value::Object(o) if f.map => {
                o.iter().find_map(|(mk, x)| inner(format!("{k}.{mk}"), x))
            }
            serde_json::Value::Array(a) => a
                .iter()
                .enumerate()
                .find_map(|(n, x)| inner(format!("{k}[{n}]"), x)),
            x => inner(k.clone(), x),
        };
        if found.is_some() {
            return found;
        }
    }
    None
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
        assert_eq!(w.rest, Some(("GET", "/auth/whoami")));
        assert_eq!(
            services(),
            vec![
                "glidex.tenancy.v1.TenantService",
                "glidex.tenancy.v1.AdmissionService",
                "glidex.access.v1.ProjectService",
                "glidex.access.v1.UserService",
                "glidex.access.v1.TeamService",
                "glidex.access.v1.TokenService",
                "glidex.access.v1.SystemBindingService",
                "glidex.access.v1.PolicyService",
                "glidex.access.v1.AuditService",
                "glidex.access.v1.AuthzService",
                "glidex.auth.v1.LoginService",
                "glidex.cluster.v1.ClusterService",
                "glidex.cluster.v1.NodeService",
                "glidex.cluster.v1.ImportService",
                "glidex.directory.v1.ConnectorService",
                "glidex.host.v1.HostNetworkService",
                "glidex.host.v1.PciService",
                "glidex.network.v1.NetworkService",
                "glidex.network.v1.NetworkShareService",
                "glidex.network.v1.RouterService",
                "glidex.storage.v1.CredentialService",
                "glidex.storage.v1.ImageService",
                "glidex.storage.v1.DiskService",
                "glidex.system.v1.SystemService",
                "glidex.usage.v1.UsageService",
                "glidex.usage.v1.MetricsService",
                "glidex.vm.v1.VmService",
                "glidex.watch.v1.WatchService"
            ]
        );
        let w = operation("/glidex.tenancy.v1.TenantService/CreateTenant").unwrap();
        assert_eq!(
            (w.action, w.kind, w.rest),
            ("createTenant", Kind::Write, Some(("POST", "/tenants")))
        );
    }

    #[test]
    fn json_uses_proto_names_and_keeps_set_falses() {
        use proto::glidex::system::v1::{GetServerInfoResponse, LoginMethods};
        let r = GetServerInfoResponse {
            cluster_name: "lab".into(),
            version: "0.1.0".into(),
            methods: LoginMethods {
                pam: Some(true),
                oidc: Some(false),
                disabled: Some(false),
                ..Default::default()
            }
            .into(),
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

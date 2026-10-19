//! RPC adapters (spec/grpc.md §6): gRPC, gRPC-Web and the Connect
//! protocol, served by `connectrpc` inside the API router.
//!
//! Each service is mounted at `/<package>.<Service>/{*method}`, so the
//! router's global layers (UI relay, authentication with its Origin and
//! CSRF checks, the cluster gate) run for RPCs as for REST. The REST
//! per-route layers don't; [`unary`] does their part: it finds the
//! operation, applies the per-listener protocol and credential rules,
//! builds the [`Call`], runs the service function and audits the result.

mod access;
mod cluster;
mod directory;
mod host;
mod login;
mod network;
mod storage;
mod system;
mod tenancy;
mod usage;
mod vm;
mod watch;

use super::{AppState, Caller, Listener, PUBLIC, RequestId, write_audit};
use crate::auth::Principal;
use crate::service::{Call, ServiceError};
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use connectrpc::{
    ConnectError, ConnectRpcService, DeadlinePolicy, ErrorCode, ErrorDetail, Limits, Protocol,
    RequestContext,
};
use glidex_api::proto::glidex::api::v1::ErrorDetails;
use glidex_api::proto::google::rpc::ErrorInfo;
use glidex_api::{Kind, Operation};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// Largest RPC message (spec/grpc.md §3).
pub const MAX_MESSAGE: usize = 4 * 1024 * 1024;
/// Longest deadline a unary call may ask for.
const MAX_DEADLINE: Duration = Duration::from_secs(300);

/// Services that aren't in the operation table: answered by whichever
/// server receives them, never forwarded, never audited.
const LOCAL_SERVICES: &[&str] = &["grpc.health.v1.Health", REFLECTION, REFLECTION_ALPHA];

/// Server reflection (spec/grpc.md §3): on `api.sock` only, where the
/// caller is a local user.
const REFLECTION: &str = "grpc.reflection.v1.ServerReflection";
const REFLECTION_ALPHA: &str = "grpc.reflection.v1alpha.ServerReflection";

/// The service an RPC path belongs to, if it is one of ours or a local one.
fn service_of(path: &str) -> Option<&str> {
    let svc = path.strip_prefix('/')?.split_once('/')?.0;
    (glidex_api::services().contains(&svc) || LOCAL_SERVICES.contains(&svc)).then_some(svc)
}

/// An RPC this server answers itself, whatever its role (health).
pub(crate) fn is_local(path: &str) -> bool {
    service_of(path).is_some_and(|s| LOCAL_SERVICES.contains(&s))
}

/// The RPC routes, added to the API router.
pub(crate) fn routes(app: &AppState) -> Router<AppState> {
    let connect = connectrpc::Router::new();
    let connect = system::register(app, connect);
    let connect = access::register(app, connect);
    let connect = tenancy::register(app, connect);
    let connect = directory::register(app, connect);
    let connect = login::register(app, connect);
    let connect = storage::register(app, connect);
    let connect = vm::register(app, connect);
    let connect = usage::register(app, connect);
    let connect = network::register(app, connect);
    let connect = host::register(app, connect);
    let connect = cluster::register(app, connect);
    let connect = watch::register(app, connect);
    let (connect, _health) = connectrpc_health::install_static(connect, glidex_api::services());
    let svc = ConnectRpcService::new(connect)
        .with_limits(
            Limits::default()
                .with_max_message_size(MAX_MESSAGE)
                .with_max_request_body_size(MAX_MESSAGE + 5),
        )
        .with_deadline_policy(DeadlinePolicy::new().with_max(MAX_DEADLINE));
    let mut r = Router::new();
    for s in glidex_api::services()
        .into_iter()
        .chain(["grpc.health.v1.Health"])
    {
        r = r.route_service(&format!("/{s}/{{*method}}"), svc.clone());
    }
    let reflector =
        connectrpc_reflection::Reflector::from_descriptor_set_bytes(glidex_api::DESCRIPTOR_SET)
            .expect("glidex-api's descriptor set reads");
    let reflection = ConnectRpcService::new(connectrpc_reflection::install(
        connectrpc::Router::new(),
        reflector,
    ));
    for s in [REFLECTION, REFLECTION_ALPHA] {
        r = r.route_service(
            &format!("/{s}/{{*method}}"),
            tower::ServiceBuilder::new()
                .layer(axum::middleware::from_fn(api_socket_only))
                .service(reflection.clone()),
        );
    }
    // A host RPC relayed from an agent's UI runs on that agent (§8).
    r.layer(axum::middleware::from_fn_with_state(
        app.clone(),
        crate::api::relay::remote_host,
    ))
}

/// Reflection is answered on `api.sock` only; elsewhere it isn't there.
async fn api_socket_only(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if req.extensions().get::<Listener>() == Some(&Listener::Api) {
        next.run(req).await
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// Which protocols a listener takes (spec/grpc.md §6): gRPC-Web is the
/// browser's, so only the UI socket (and UI requests relayed from an
/// agent) take it, and they take nothing else.
fn protocol_allowed(listener: Listener, protocol: Option<Protocol>) -> bool {
    match listener {
        Listener::Ui | Listener::Relay => protocol == Some(Protocol::GrpcWeb),
        Listener::Tcp | Listener::Api => {
            matches!(protocol, Some(Protocol::Grpc | Protocol::Connect))
        }
        Listener::Cluster => true,
    }
}

/// Session cookies are a browser credential: accepted for gRPC-Web from
/// glidex-ui only. A forwarded call was checked where it came in.
fn cookie_ok(listener: Listener, protocol: Option<Protocol>, p: &Principal) -> bool {
    p.session_hash.is_none()
        || listener == Listener::Cluster
        || (matches!(listener, Listener::Ui | Listener::Relay)
            && protocol == Some(Protocol::GrpcWeb))
}

/// One unary RPC: checks, then `f` with the caller (`None` for public
/// operations), then the audit entry and the error mapping.
pub(crate) async fn unary<T, F, Fut>(
    app: &AppState,
    ctx: &RequestContext,
    f: F,
) -> Result<connectrpc::Response<T>, ConnectError>
where
    F: FnOnce(Option<Call>) -> Fut,
    Fut: Future<Output = Result<T, ServiceError>>,
{
    let ext = ctx.extensions();
    let op = ctx.spec().and_then(|s| glidex_api::operation(s.procedure));
    let Some(op) = op else {
        return Err(ConnectError::unimplemented("no such operation"));
    };
    let listener = ext.get::<Listener>().copied().unwrap_or(Listener::Tcp);
    let protocol = ctx.protocol();
    let request_id = ext
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_default();
    let principal = ext.get::<Principal>();
    let checked = if !protocol_allowed(listener, protocol) {
        Err(ServiceError::new(
            StatusCode::FORBIDDEN,
            "protocol_not_allowed",
            "this listener doesn't take this RPC protocol (spec/grpc.md §6)",
        ))
    } else if principal.is_some_and(|p| !cookie_ok(listener, protocol, p)) {
        Err(ServiceError::new(
            StatusCode::UNAUTHORIZED,
            "cookie_not_accepted",
            "session cookies are accepted only for gRPC-Web through glidex-ui; send a bearer token",
        ))
    } else if op.action == PUBLIC {
        Ok(None)
    } else {
        Caller::from_extensions(ext, app, op.action)
            .map(Some)
            .map_err(ServiceError::from)
    };
    // Kept for the audit entry: the caller itself moves into `f`.
    let audited = checked
        .as_ref()
        .ok()
        .and_then(|c| c.as_ref().map(|c| (c.p.clone(), c.audit.clone())));
    let result = match checked {
        Ok(c) => f(c).await,
        Err(e) => Err(e),
    };
    if let Some((p, slot)) = &audited {
        let failure = result
            .as_ref()
            .err()
            .map(|e| (format!("error:{}", e.code), e.code.clone()));
        write_audit(
            app,
            Some(p),
            &request_id,
            op.action,
            op.kind == Kind::Write,
            slot,
            failure,
            ("rpc", rpc_detail(op, protocol)),
        );
    }
    match result {
        Ok(v) => connectrpc::Response::ok(v),
        Err(e) => Err(to_connect(e, &request_id)),
    }
}

fn rpc_detail(op: &Operation, protocol: Option<Protocol>) -> String {
    let p = match protocol {
        Some(Protocol::Grpc) => "grpc",
        Some(Protocol::GrpcWeb) => "grpc-web",
        Some(Protocol::Connect) => "connect",
        _ => "?",
    };
    format!("{p} {}", op.procedure)
}

/// The RPC status for an error code (spec/grpc.md §7). Codes not listed
/// fall back to their REST status.
pub(crate) fn rpc_code(code: &str, status: StatusCode) -> ErrorCode {
    match code {
        "unauthenticated" | "login_failed" | "reauth_required" | "cookie_not_accepted" => {
            ErrorCode::Unauthenticated
        }
        "forbidden"
        | "login_not_allowed"
        | "tls_required"
        | "ticket_required"
        | "tenant_suspended"
        | "origin_not_allowed"
        | "csrf_failed"
        | "protocol_not_allowed"
        | "agent_no_local_api" => ErrorCode::PermissionDenied,
        "quota_exceeded"
        | "tenant_quota_exceeded"
        | "too_large"
        | "rate_limited"
        | "slow_down"
        | "insufficient_storage" => ErrorCode::ResourceExhausted,
        "not_found" | "oidc_disabled" => ErrorCode::NotFound,
        "conflict" => ErrorCode::Aborted,
        "authorization_pending" | "unsupported_on_host" => ErrorCode::FailedPrecondition,
        "not_leader" | "use_a_server" | "leader_changed" | "cluster_unavailable" => {
            ErrorCode::Unavailable
        }
        "internal" | "persistence_error" => ErrorCode::Internal,
        _ => match status.as_u16() {
            400 | 422 => ErrorCode::InvalidArgument,
            401 => ErrorCode::Unauthenticated,
            403 => ErrorCode::PermissionDenied,
            404 => ErrorCode::NotFound,
            409 => ErrorCode::FailedPrecondition,
            413 | 429 => ErrorCode::ResourceExhausted,
            421 | 502 | 503 | 504 => ErrorCode::Unavailable,
            501 => ErrorCode::Unimplemented,
            _ => ErrorCode::Internal,
        },
    }
}

/// A `ServiceError` as an RPC error: the status from [`rpc_code`], the
/// code as `google.rpc.ErrorInfo.reason`, REST's `details` as
/// `glidex.api.v1.ErrorDetails`. Internal errors carry a generic message;
/// the real one is logged with the request id.
pub(crate) fn to_connect(e: ServiceError, request_id: &str) -> ConnectError {
    let code = rpc_code(&e.code, e.status);
    let message = if code == ErrorCode::Internal {
        tracing::error!(request_id, code = %e.code, "rpc failed: {}", e.message);
        format!("internal error (request {request_id})")
    } else {
        e.message
    };
    // The REST status too, so a client on RPC (the UI) can tell what REST
    // would have said (422 from 400, say).
    let mut info = ErrorInfo {
        reason: e.code,
        domain: glidex_api::ERROR_DOMAIN.into(),
        ..Default::default()
    };
    info.metadata
        .insert("http_status".into(), e.status.as_u16().to_string());
    let mut err = ConnectError::new(code, message)
        .with_detail(ErrorDetail::from_message("google.rpc.ErrorInfo", &info));
    if !e.details.is_null() {
        let d = ErrorDetails {
            json: e.details.to_string(),
            ..Default::default()
        };
        err = err.with_detail(ErrorDetail::from_message("glidex.api.v1.ErrorDetails", &d));
    }
    err
}

/// Shared by the service implementations: the app they serve.
pub(crate) struct Svc {
    pub app: AppState,
}

impl Svc {
    pub fn new(app: &AppState) -> Arc<Self> {
        Arc::new(Svc { app: app.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_map_like_the_spec_table() {
        let cases: &[(&str, u16, ErrorCode)] = &[
            ("unauthenticated", 401, ErrorCode::Unauthenticated),
            ("reauth_required", 401, ErrorCode::Unauthenticated),
            ("forbidden", 403, ErrorCode::PermissionDenied),
            ("quota_exceeded", 403, ErrorCode::ResourceExhausted),
            ("not_found", 404, ErrorCode::NotFound),
            ("conflict", 409, ErrorCode::Aborted),
            ("would_lock_out", 409, ErrorCode::FailedPrecondition),
            ("confirmation_required", 409, ErrorCode::FailedPrecondition),
            ("invalid_policy", 422, ErrorCode::InvalidArgument),
            ("bad_request", 400, ErrorCode::InvalidArgument),
            ("unsupported_on_host", 422, ErrorCode::FailedPrecondition),
            ("too_large", 413, ErrorCode::ResourceExhausted),
            ("rate_limited", 429, ErrorCode::ResourceExhausted),
            ("not_leader", 421, ErrorCode::Unavailable),
            ("netd_error", 502, ErrorCode::Unavailable),
            ("cluster_unavailable", 503, ErrorCode::Unavailable),
            ("persistence_error", 500, ErrorCode::Internal),
        ];
        for (code, status, want) in cases {
            assert_eq!(
                rpc_code(code, StatusCode::from_u16(*status).unwrap()),
                *want,
                "{code}"
            );
        }
    }

    #[test]
    fn listener_protocols() {
        use Listener::*;
        assert!(protocol_allowed(Ui, Some(Protocol::GrpcWeb)));
        assert!(!protocol_allowed(Ui, Some(Protocol::Grpc)));
        assert!(!protocol_allowed(Ui, Some(Protocol::Connect)));
        assert!(!protocol_allowed(Tcp, Some(Protocol::GrpcWeb)));
        assert!(protocol_allowed(Tcp, Some(Protocol::Grpc)));
        assert!(protocol_allowed(Api, Some(Protocol::Connect)));
        assert!(protocol_allowed(Cluster, Some(Protocol::GrpcWeb)));
    }

    #[test]
    fn rpc_paths() {
        assert_eq!(
            service_of("/glidex.system.v1.SystemService/WhoAmI"),
            Some("glidex.system.v1.SystemService")
        );
        assert!(is_local("/grpc.health.v1.Health/Check"));
        assert!(!is_local("/glidex.system.v1.SystemService/WhoAmI"));
        assert_eq!(service_of("/vms/abc"), None);
    }
}

//! `glidex.auth.v1.LoginService` over RPC; the logic is in
//! `service::login`. These operations have no caller: the adapter hands
//! the service where the request came from instead.

use super::{Svc, unary};
use crate::api::{AppState, ClientAddr, Listener, RequestId};
use crate::service::login::{self, Origin};
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::auth::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    LoginServiceExt::register(Svc::new(app), router)
}

/// The request id, listener and client address of `ctx`.
fn origin_of(ctx: &RequestContext) -> (String, Listener, Option<std::net::IpAddr>) {
    let ext = ctx.extensions();
    (
        ext.get::<RequestId>()
            .map(|r| r.0.clone())
            .unwrap_or_default(),
        ext.get::<Listener>().copied().unwrap_or(Listener::Tcp),
        ext.get::<ClientAddr>().map(|a| a.0.ip()),
    )
}

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl LoginService for Svc {
    async fn discover(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DiscoverRequest>,
    ) -> ServiceResult<DiscoverResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |_| async {
            Ok(login::discover(&self.app, req))
        })
        .await
    }

    async fn start_device(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, StartDeviceRequest>,
    ) -> ServiceResult<StartDeviceResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |_| login::start_device(&self.app, req)).await
    }

    async fn poll_device(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PollDeviceRequest>,
    ) -> ServiceResult<PollDeviceResponse> {
        let req = req.to_owned_message();
        let (rid, listener, client_ip) = origin_of(&ctx);
        unary(&self.app, &ctx, |_| async move {
            let origin = Origin {
                request_id: &rid,
                listener,
                client_ip,
            };
            Ok(match login::poll_device(&self.app, &origin, req).await? {
                None => PollDeviceResponse {
                    pending: Some(true),
                    ..Default::default()
                },
                Some(m) => PollDeviceResponse {
                    issued: m.to_pb().into(),
                    ..Default::default()
                },
            })
        })
        .await
    }

    async fn password_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PasswordTokenRequest>,
    ) -> ServiceResult<PasswordTokenResponse> {
        let req = req.to_owned_message();
        let (rid, listener, client_ip) = origin_of(&ctx);
        unary(&self.app, &ctx, |_| async move {
            let origin = Origin {
                request_id: &rid,
                listener,
                client_ip,
            };
            let m = login::password_token(&self.app, &origin, req).await?;
            Ok(PasswordTokenResponse {
                issued: m.to_pb().into(),
                ..Default::default()
            })
        })
        .await
    }
}

//! `glidex.system.v1.SystemService` over RPC; the logic is in
//! `service::system`.

use super::{unary, Svc};
use crate::api::AppState;
use crate::service::system;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::system::v1::{
    GetServerInfoRequest, GetServerInfoResponse, SystemService, SystemServiceExt, WhoAmIRequest,
    WhoAmIResponse,
};

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    Svc::new(app).register(router)
}

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl SystemService for Svc {
    async fn get_server_info(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, GetServerInfoRequest>,
    ) -> ServiceResult<GetServerInfoResponse> {
        unary(&self.app, &ctx, |_| async {
            Ok(system::server_info(&self.app))
        })
        .await
    }

    async fn who_am_i(
        &self,
        ctx: RequestContext,
        _req: ServiceRequest<'_, WhoAmIRequest>,
    ) -> ServiceResult<WhoAmIResponse> {
        unary(&self.app, &ctx, |c| async move {
            system::whoami(&c.expect("authenticated operation"))
        })
        .await
    }
}

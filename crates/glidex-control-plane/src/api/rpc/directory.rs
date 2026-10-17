//! `glidex.directory.v1` over RPC; the logic is in `service::directory`.

use super::{unary, Svc};
use crate::api::AppState;
use crate::service::directory;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::directory::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    ConnectorServiceExt::register(Svc::new(app), router)
}

macro_rules! op {
    ($self:ident, $ctx:ident, $req:ident, $f:path) => {{
        let req = $req.to_owned_message();
        unary(&$self.app, &$ctx, |c| async move {
            $f(&c.expect("authenticated operation"), req)
        })
        .await
    }};
}

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl ConnectorService for Svc {
    async fn list_connectors(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListConnectorsRequest>,
    ) -> ServiceResult<ListConnectorsResponse> {
        op!(self, ctx, req, directory::list_connectors)
    }

    async fn get_connector(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetConnectorRequest>,
    ) -> ServiceResult<GetConnectorResponse> {
        op!(self, ctx, req, directory::get_connector)
    }

    async fn create_connector(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateConnectorRequest>,
    ) -> ServiceResult<CreateConnectorResponse> {
        op!(self, ctx, req, directory::create_connector)
    }

    async fn update_connector(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateConnectorRequest>,
    ) -> ServiceResult<UpdateConnectorResponse> {
        op!(self, ctx, req, directory::update_connector)
    }

    async fn delete_connector(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteConnectorRequest>,
    ) -> ServiceResult<DeleteConnectorResponse> {
        op!(self, ctx, req, directory::delete_connector)
    }

    async fn set_secret(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, SetSecretRequest>,
    ) -> ServiceResult<SetSecretResponse> {
        op!(self, ctx, req, directory::set_secret)
    }

    async fn test_connector(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, TestConnectorRequest>,
    ) -> ServiceResult<TestConnectorResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            directory::test_connector(&c.expect("authenticated operation"), req).await
        })
        .await
    }
}

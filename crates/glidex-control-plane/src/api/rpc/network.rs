//! `glidex.network.v1` over RPC; the logic is in `service::network`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::network as svc;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::network::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = NetworkServiceExt::register(Svc::new(app), router);
    let router = NetworkShareServiceExt::register(Svc::new(app), router);
    RouterServiceExt::register(Svc::new(app), router)
}

/// A synchronous operation.
macro_rules! op {
    ($self:ident, $ctx:ident, $req:ident, $f:path) => {{
        let req = $req.to_owned_message();
        unary(&$self.app, &$ctx, |c| async move {
            $f(&c.expect("authenticated operation"), req)
        })
        .await
    }};
}

/// An async one.
macro_rules! op_async {
    ($self:ident, $ctx:ident, $req:ident, $f:path) => {{
        let req = $req.to_owned_message();
        unary(&$self.app, &$ctx, |c| async move {
            $f(&c.expect("authenticated operation"), req).await
        })
        .await
    }};
}

// The generated traits return `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl NetworkService for Svc {
    async fn list_networks(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListNetworksRequest>,
    ) -> ServiceResult<ListNetworksResponse> {
        op!(self, ctx, req, svc::list_networks)
    }

    async fn create_network(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateNetworkRequest>,
    ) -> ServiceResult<CreateNetworkResponse> {
        op_async!(self, ctx, req, svc::create_network)
    }

    async fn get_network(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetNetworkRequest>,
    ) -> ServiceResult<GetNetworkResponse> {
        op!(self, ctx, req, svc::get_network)
    }

    async fn delete_network(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteNetworkRequest>,
    ) -> ServiceResult<DeleteNetworkResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            let network = svc::delete_network(&c.expect("authenticated operation"), req).await?;
            Ok(DeleteNetworkResponse {
                network: network.into(),
                ..Default::default()
            })
        })
        .await
    }

    async fn list_network_events(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListNetworkEventsRequest>,
    ) -> ServiceResult<ListNetworkEventsResponse> {
        op!(self, ctx, req, svc::network_events)
    }

    async fn get_network_ovn(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetNetworkOvnRequest>,
    ) -> ServiceResult<GetNetworkOvnResponse> {
        op_async!(self, ctx, req, svc::network_ovn)
    }

    async fn grant_network(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GrantNetworkRequest>,
    ) -> ServiceResult<GrantNetworkResponse> {
        op!(self, ctx, req, svc::grant_network)
    }

    async fn set_network_host_api(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, SetNetworkHostApiRequest>,
    ) -> ServiceResult<SetNetworkHostApiResponse> {
        op!(self, ctx, req, svc::set_network_host_api)
    }

    async fn create_project_network(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateProjectNetworkRequest>,
    ) -> ServiceResult<CreateProjectNetworkResponse> {
        op_async!(self, ctx, req, svc::create_project_network)
    }

    async fn set_network_router(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, SetNetworkRouterRequest>,
    ) -> ServiceResult<SetNetworkRouterResponse> {
        op_async!(self, ctx, req, svc::set_network_router)
    }
}

#[allow(refining_impl_trait)]
impl NetworkShareService for Svc {
    async fn offer_network_share(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, OfferNetworkShareRequest>,
    ) -> ServiceResult<OfferNetworkShareResponse> {
        op!(self, ctx, req, svc::offer_share)
    }

    async fn unshare_network(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UnshareNetworkRequest>,
    ) -> ServiceResult<UnshareNetworkResponse> {
        op_async!(self, ctx, req, svc::unshare)
    }

    async fn list_network_shares(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListNetworkSharesRequest>,
    ) -> ServiceResult<ListNetworkSharesResponse> {
        op!(self, ctx, req, svc::list_shares)
    }

    async fn accept_network_share(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AcceptNetworkShareRequest>,
    ) -> ServiceResult<AcceptNetworkShareResponse> {
        op!(self, ctx, req, svc::accept_share)
    }

    async fn leave_network_share(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, LeaveNetworkShareRequest>,
    ) -> ServiceResult<LeaveNetworkShareResponse> {
        op_async!(self, ctx, req, svc::leave_share)
    }
}

#[allow(refining_impl_trait)]
impl RouterService for Svc {
    async fn list_routers(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListRoutersRequest>,
    ) -> ServiceResult<ListRoutersResponse> {
        op!(self, ctx, req, svc::list_routers)
    }

    async fn get_router(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetRouterRequest>,
    ) -> ServiceResult<GetRouterResponse> {
        op!(self, ctx, req, svc::get_router)
    }

    async fn create_router(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateRouterRequest>,
    ) -> ServiceResult<CreateRouterResponse> {
        op_async!(self, ctx, req, svc::create_router)
    }

    async fn delete_router(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteRouterRequest>,
    ) -> ServiceResult<DeleteRouterResponse> {
        op_async!(self, ctx, req, svc::delete_router)
    }
}

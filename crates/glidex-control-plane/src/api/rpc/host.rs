//! `glidex.host.v1` over RPC; the logic is in `service::host`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::host as svc;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::host::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = HostNetworkServiceExt::register(Svc::new(app), router);
    let router = TopologyServiceExt::register(Svc::new(app), router);
    PciServiceExt::register(Svc::new(app), router)
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
impl HostNetworkService for Svc {
    async fn get_ovs_status(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetOvsStatusRequest>,
    ) -> ServiceResult<GetOvsStatusResponse> {
        op_async!(self, ctx, req, svc::ovs_status)
    }

    async fn install_ovs(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, InstallOvsRequest>,
    ) -> ServiceResult<InstallOvsResponse> {
        op_async!(self, ctx, req, svc::install_ovs)
    }

    async fn init_dpdk(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, InitDpdkRequest>,
    ) -> ServiceResult<InitDpdkResponse> {
        op_async!(self, ctx, req, svc::init_dpdk)
    }

    async fn list_bridges(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListBridgesRequest>,
    ) -> ServiceResult<ListBridgesResponse> {
        op_async!(self, ctx, req, svc::list_bridges)
    }

    async fn create_bridge(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateBridgeRequest>,
    ) -> ServiceResult<CreateBridgeResponse> {
        op_async!(self, ctx, req, svc::create_bridge)
    }

    async fn delete_bridge(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteBridgeRequest>,
    ) -> ServiceResult<DeleteBridgeResponse> {
        op_async!(self, ctx, req, svc::delete_bridge)
    }

    async fn list_uplinks(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListUplinksRequest>,
    ) -> ServiceResult<ListUplinksResponse> {
        op_async!(self, ctx, req, svc::list_uplinks)
    }

    async fn ensure_uplink(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, EnsureUplinkRequest>,
    ) -> ServiceResult<EnsureUplinkResponse> {
        op_async!(self, ctx, req, svc::ensure_uplink)
    }

    async fn delete_uplink(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteUplinkRequest>,
    ) -> ServiceResult<DeleteUplinkResponse> {
        op_async!(self, ctx, req, svc::delete_uplink)
    }

    async fn commit_uplink(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CommitUplinkRequest>,
    ) -> ServiceResult<CommitUplinkResponse> {
        op_async!(self, ctx, req, svc::commit_uplink)
    }

    async fn reapply_uplink(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ReapplyUplinkRequest>,
    ) -> ServiceResult<ReapplyUplinkResponse> {
        op_async!(self, ctx, req, svc::reapply_uplink)
    }
}

// The generated traits return `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl PciService for Svc {
    async fn list_pci_devices(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListPciDevicesRequest>,
    ) -> ServiceResult<ListPciDevicesResponse> {
        op!(self, ctx, req, svc::list_pci_devices)
    }
}

// The generated traits return `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl TopologyService for Svc {
    async fn get_host_topology(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetHostTopologyRequest>,
    ) -> ServiceResult<GetHostTopologyResponse> {
        op!(self, ctx, req, svc::get_host_topology)
    }
}

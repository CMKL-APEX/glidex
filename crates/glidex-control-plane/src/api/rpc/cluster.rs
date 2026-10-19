//! `glidex.cluster.v1` over RPC; the logic is in `service::cluster` and
//! `service::node`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::{cluster as svc, node};
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::cluster::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = ClusterServiceExt::register(Svc::new(app), router);
    let router = NodeServiceExt::register(Svc::new(app), router);
    ImportServiceExt::register(Svc::new(app), router)
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
impl ClusterService for Svc {
    async fn init_cluster(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, InitClusterRequest>,
    ) -> ServiceResult<InitClusterResponse> {
        op_async!(self, ctx, req, svc::init)
    }

    async fn join_cluster(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, JoinClusterRequest>,
    ) -> ServiceResult<JoinClusterResponse> {
        op_async!(self, ctx, req, svc::join)
    }

    async fn import_join(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ImportJoinRequest>,
    ) -> ServiceResult<ImportJoinResponse> {
        op_async!(self, ctx, req, svc::import_join)
    }

    async fn rejoin_cluster(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RejoinClusterRequest>,
    ) -> ServiceResult<RejoinClusterResponse> {
        op_async!(self, ctx, req, svc::rejoin)
    }

    async fn get_cluster_status(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetClusterStatusRequest>,
    ) -> ServiceResult<GetClusterStatusResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            svc::status(&c.expect("authenticated operation"), req, None).await
        })
        .await
    }

    async fn create_join_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateJoinTokenRequest>,
    ) -> ServiceResult<CreateJoinTokenResponse> {
        op_async!(self, ctx, req, svc::create_token)
    }

    async fn promote_nodes(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PromoteNodesRequest>,
    ) -> ServiceResult<PromoteNodesResponse> {
        op_async!(self, ctx, req, svc::promote)
    }

    async fn dissolve_cluster(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DissolveClusterRequest>,
    ) -> ServiceResult<DissolveClusterResponse> {
        op_async!(self, ctx, req, svc::dissolve)
    }

    async fn leave_cluster(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, LeaveClusterRequest>,
    ) -> ServiceResult<LeaveClusterResponse> {
        op_async!(self, ctx, req, svc::leave)
    }

    async fn rotate_ca(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RotateCaRequest>,
    ) -> ServiceResult<RotateCaResponse> {
        op_async!(self, ctx, req, svc::rotate_ca)
    }

    async fn rotate_secrets_key(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RotateSecretsKeyRequest>,
    ) -> ServiceResult<RotateSecretsKeyResponse> {
        op_async!(self, ctx, req, svc::rotate_secrets_key)
    }

    async fn get_ovn_settings(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetOvnSettingsRequest>,
    ) -> ServiceResult<GetOvnSettingsResponse> {
        op!(self, ctx, req, svc::get_ovn_settings)
    }

    async fn put_ovn_settings(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PutOvnSettingsRequest>,
    ) -> ServiceResult<PutOvnSettingsResponse> {
        op_async!(self, ctx, req, svc::put_ovn_settings)
    }

    async fn get_edge_defaults(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetEdgeDefaultsRequest>,
    ) -> ServiceResult<GetEdgeDefaultsResponse> {
        op!(self, ctx, req, svc::edge_defaults)
    }

    async fn get_ovn_overview(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetOvnOverviewRequest>,
    ) -> ServiceResult<GetOvnOverviewResponse> {
        op_async!(self, ctx, req, svc::ovn_overview)
    }

    async fn get_ovn_tunnels(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetOvnTunnelsRequest>,
    ) -> ServiceResult<GetOvnTunnelsResponse> {
        op_async!(self, ctx, req, svc::ovn_tunnels)
    }
}

// The generated traits return `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl NodeService for Svc {
    async fn list_nodes(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListNodesRequest>,
    ) -> ServiceResult<ListNodesResponse> {
        op!(self, ctx, req, node::list)
    }

    async fn get_node(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetNodeRequest>,
    ) -> ServiceResult<GetNodeResponse> {
        op!(self, ctx, req, node::get_node)
    }

    async fn get_node_topology(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetNodeTopologyRequest>,
    ) -> ServiceResult<GetNodeTopologyResponse> {
        op_async!(self, ctx, req, node::get_node_topology)
    }

    async fn drain_node(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DrainNodeRequest>,
    ) -> ServiceResult<DrainNodeResponse> {
        op_async!(self, ctx, req, node::drain)
    }

    async fn undrain_node(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UndrainNodeRequest>,
    ) -> ServiceResult<UndrainNodeResponse> {
        op_async!(self, ctx, req, node::undrain)
    }

    async fn configure_node_ovn(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ConfigureNodeOvnRequest>,
    ) -> ServiceResult<ConfigureNodeOvnResponse> {
        op_async!(self, ctx, req, node::put_node_ovn)
    }

    async fn delete_spare_image_files(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteSpareImageFilesRequest>,
    ) -> ServiceResult<DeleteSpareImageFilesResponse> {
        op!(self, ctx, req, node::delete_spare_image_files)
    }

    async fn remove_node(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RemoveNodeRequest>,
    ) -> ServiceResult<RemoveNodeResponse> {
        op_async!(self, ctx, req, node::remove)
    }

    async fn forget_node(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ForgetNodeRequest>,
    ) -> ServiceResult<ForgetNodeResponse> {
        op_async!(self, ctx, req, node::forget)
    }

    async fn purge_node(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PurgeNodeRequest>,
    ) -> ServiceResult<PurgeNodeResponse> {
        op_async!(self, ctx, req, node::purge)
    }

    async fn create_rejoin_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateRejoinTokenRequest>,
    ) -> ServiceResult<CreateRejoinTokenResponse> {
        op_async!(self, ctx, req, node::rejoin_token)
    }

    async fn detach_node(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DetachNodeRequest>,
    ) -> ServiceResult<DetachNodeResponse> {
        op_async!(self, ctx, req, node::detach)
    }

    async fn abort_detach(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AbortDetachRequest>,
    ) -> ServiceResult<AbortDetachResponse> {
        op_async!(self, ctx, req, node::abort_detach)
    }

    async fn forget_departed(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ForgetDepartedRequest>,
    ) -> ServiceResult<ForgetDepartedResponse> {
        op_async!(self, ctx, req, node::forget_departed)
    }

    async fn list_departures(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListDeparturesRequest>,
    ) -> ServiceResult<ListDeparturesResponse> {
        op!(self, ctx, req, node::list_departures)
    }
}

// The generated traits return `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl ImportService for Svc {
    async fn list_imports(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListImportsRequest>,
    ) -> ServiceResult<ListImportsResponse> {
        op!(self, ctx, req, node::list_imports)
    }

    async fn get_import(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetImportRequest>,
    ) -> ServiceResult<GetImportResponse> {
        op!(self, ctx, req, node::get_import)
    }

    async fn approve_import(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ApproveImportRequest>,
    ) -> ServiceResult<ApproveImportResponse> {
        op_async!(self, ctx, req, node::approve_import)
    }

    async fn reject_import(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RejectImportRequest>,
    ) -> ServiceResult<RejectImportResponse> {
        op_async!(self, ctx, req, node::reject_import)
    }
}

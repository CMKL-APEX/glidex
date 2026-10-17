//! `glidex.usage.v1` over RPC; the logic is in `service::usage`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::usage::{self as svc, recast};
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::usage::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = UsageServiceExt::register(Svc::new(app), router);
    MetricsServiceExt::register(Svc::new(app), router)
}

/// An operation answering its own message.
macro_rules! op {
    ($self:ident, $ctx:ident, $req:ident, $f:path) => {{
        let req = $req.to_owned_message();
        unary(&$self.app, &$ctx, |c| async move {
            $f(&c.expect("authenticated operation"), req).await
        })
        .await
    }};
}

/// A usage query: the shared answer, as the operation's own response.
macro_rules! usage_op {
    ($self:ident, $ctx:ident, $req:ident, $f:path) => {{
        let req = $req.to_owned_message();
        unary(&$self.app, &$ctx, |c| async move {
            let r = $f(&c.expect("authenticated operation"), req).await?;
            Ok(recast(&r))
        })
        .await
    }};
}

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl UsageService for Svc {
    async fn get_usage(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetUsageRequest>,
    ) -> ServiceResult<GetUsageResponse> {
        op!(self, ctx, req, svc::get_usage)
    }

    async fn get_project_usage(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetProjectUsageRequest>,
    ) -> ServiceResult<GetProjectUsageResponse> {
        usage_op!(self, ctx, req, svc::project_usage)
    }

    async fn get_vm_usage(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetVmUsageRequest>,
    ) -> ServiceResult<GetVmUsageResponse> {
        usage_op!(self, ctx, req, svc::vm_usage)
    }

    async fn get_disk_usage(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetDiskUsageRequest>,
    ) -> ServiceResult<GetDiskUsageResponse> {
        usage_op!(self, ctx, req, svc::disk_usage)
    }

    async fn get_bandwidth_report(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetBandwidthReportRequest>,
    ) -> ServiceResult<GetBandwidthReportResponse> {
        op!(self, ctx, req, svc::bandwidth_report)
    }

    async fn get_disk_io_report(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetDiskIoReportRequest>,
    ) -> ServiceResult<GetDiskIoReportResponse> {
        op!(self, ctx, req, svc::disk_io_report)
    }

    async fn get_compute_report(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetComputeReportRequest>,
    ) -> ServiceResult<GetComputeReportResponse> {
        op!(self, ctx, req, svc::compute_report)
    }
}

#[allow(refining_impl_trait)]
impl MetricsService for Svc {
    async fn get_vm_bandwidth(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetVmBandwidthRequest>,
    ) -> ServiceResult<GetVmBandwidthResponse> {
        op!(self, ctx, req, svc::vm_bandwidth)
    }

    async fn get_vm_io(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetVmIoRequest>,
    ) -> ServiceResult<GetVmIoResponse> {
        op!(self, ctx, req, svc::vm_io)
    }

    async fn get_vm_compute(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetVmComputeRequest>,
    ) -> ServiceResult<GetVmComputeResponse> {
        op!(self, ctx, req, svc::vm_compute)
    }

    async fn get_vm_stats(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetVmStatsRequest>,
    ) -> ServiceResult<GetVmStatsResponse> {
        op!(self, ctx, req, svc::vm_stats)
    }

    async fn get_disk_io(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetDiskIoRequest>,
    ) -> ServiceResult<GetDiskIoResponse> {
        op!(self, ctx, req, svc::disk_io)
    }

    async fn get_network_bandwidth(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetNetworkBandwidthRequest>,
    ) -> ServiceResult<GetNetworkBandwidthResponse> {
        op!(self, ctx, req, svc::network_bandwidth)
    }

    async fn get_network_stats(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetNetworkStatsRequest>,
    ) -> ServiceResult<GetNetworkStatsResponse> {
        op!(self, ctx, req, svc::network_stats)
    }
}

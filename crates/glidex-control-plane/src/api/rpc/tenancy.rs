//! `glidex.tenancy.v1` over RPC; the logic is in `service::tenancy`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::{directories, tenancy};
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::tenancy::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = TenantServiceExt::register(Svc::new(app), router);
    let router = DirectoryServiceExt::register(Svc::new(app), router);
    AdmissionServiceExt::register(Svc::new(app), router)
}

/// Every tenancy operation needs a caller: none of them is public.
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

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl TenantService for Svc {
    async fn list_tenants(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListTenantsRequest>,
    ) -> ServiceResult<ListTenantsResponse> {
        op!(self, ctx, req, tenancy::list_tenants)
    }

    async fn get_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetTenantRequest>,
    ) -> ServiceResult<GetTenantResponse> {
        op!(self, ctx, req, tenancy::get_tenant)
    }

    async fn create_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateTenantRequest>,
    ) -> ServiceResult<CreateTenantResponse> {
        op!(self, ctx, req, tenancy::create_tenant)
    }

    async fn update_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateTenantRequest>,
    ) -> ServiceResult<UpdateTenantResponse> {
        op!(self, ctx, req, tenancy::update_tenant)
    }

    async fn suspend_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, SuspendTenantRequest>,
    ) -> ServiceResult<SuspendTenantResponse> {
        op!(self, ctx, req, tenancy::suspend_tenant)
    }

    async fn resume_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ResumeTenantRequest>,
    ) -> ServiceResult<ResumeTenantResponse> {
        op!(self, ctx, req, tenancy::resume_tenant)
    }

    async fn delete_tenant(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteTenantRequest>,
    ) -> ServiceResult<DeleteTenantResponse> {
        op!(self, ctx, req, tenancy::delete_tenant)
    }

    async fn list_device_pools(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListDevicePoolsRequest>,
    ) -> ServiceResult<ListDevicePoolsResponse> {
        op_async!(self, ctx, req, tenancy::list_device_pools)
    }

    async fn set_device_pools(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, SetDevicePoolsRequest>,
    ) -> ServiceResult<SetDevicePoolsResponse> {
        op_async!(self, ctx, req, tenancy::set_device_pools)
    }

    async fn list_members(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListMembersRequest>,
    ) -> ServiceResult<ListMembersResponse> {
        op!(self, ctx, req, tenancy::list_members)
    }

    async fn add_member(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AddMemberRequest>,
    ) -> ServiceResult<AddMemberResponse> {
        op!(self, ctx, req, tenancy::add_member)
    }

    async fn remove_member(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RemoveMemberRequest>,
    ) -> ServiceResult<RemoveMemberResponse> {
        op!(self, ctx, req, tenancy::remove_member)
    }

    async fn list_bindings(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListBindingsRequest>,
    ) -> ServiceResult<ListBindingsResponse> {
        op!(self, ctx, req, tenancy::list_bindings)
    }

    async fn add_binding(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AddBindingRequest>,
    ) -> ServiceResult<AddBindingResponse> {
        op!(self, ctx, req, tenancy::add_binding)
    }

    async fn remove_binding(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RemoveBindingRequest>,
    ) -> ServiceResult<RemoveBindingResponse> {
        op!(self, ctx, req, tenancy::remove_binding)
    }

    async fn list_tenant_clusters(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListTenantClustersRequest>,
    ) -> ServiceResult<ListTenantClustersResponse> {
        op!(self, ctx, req, tenancy::list_tenant_clusters)
    }

    async fn opt_in_cluster(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, OptInClusterRequest>,
    ) -> ServiceResult<OptInClusterResponse> {
        op_async!(self, ctx, req, tenancy::opt_in_cluster)
    }

    async fn opt_out_cluster(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, OptOutClusterRequest>,
    ) -> ServiceResult<OptOutClusterResponse> {
        op_async!(self, ctx, req, tenancy::opt_out_cluster)
    }
}

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl AdmissionService for Svc {
    async fn list_admissions(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListAdmissionsRequest>,
    ) -> ServiceResult<ListAdmissionsResponse> {
        op!(self, ctx, req, tenancy::list_admissions)
    }

    async fn set_admission(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, SetAdmissionRequest>,
    ) -> ServiceResult<SetAdmissionResponse> {
        op!(self, ctx, req, tenancy::set_admission)
    }

    async fn remove_admission(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RemoveAdmissionRequest>,
    ) -> ServiceResult<RemoveAdmissionResponse> {
        op!(self, ctx, req, tenancy::remove_admission)
    }

    async fn offer_admission(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, OfferAdmissionRequest>,
    ) -> ServiceResult<OfferAdmissionResponse> {
        op!(self, ctx, req, tenancy::offer_admission)
    }

    async fn list_offers(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListOffersRequest>,
    ) -> ServiceResult<ListOffersResponse> {
        op!(self, ctx, req, tenancy::list_offers)
    }

    async fn revoke_offer(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RevokeOfferRequest>,
    ) -> ServiceResult<RevokeOfferResponse> {
        op!(self, ctx, req, tenancy::revoke_offer)
    }

    async fn list_trusted_directories(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListTrustedDirectoriesRequest>,
    ) -> ServiceResult<ListTrustedDirectoriesResponse> {
        op!(self, ctx, req, tenancy::list_trusted_directories)
    }
}

#[allow(refining_impl_trait)]
impl DirectoryService for Svc {
    async fn list_directories(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListDirectoriesRequest>,
    ) -> ServiceResult<ListDirectoriesResponse> {
        op!(self, ctx, req, directories::list_directories)
    }

    async fn create_directory(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateDirectoryRequest>,
    ) -> ServiceResult<CreateDirectoryResponse> {
        op!(self, ctx, req, directories::create_directory)
    }

    async fn delete_directory(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteDirectoryRequest>,
    ) -> ServiceResult<DeleteDirectoryResponse> {
        op!(self, ctx, req, directories::delete_directory)
    }
}

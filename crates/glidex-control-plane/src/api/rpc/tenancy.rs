//! `glidex.tenancy.v1` over RPC; the logic is in `service::tenancy`.

use super::{unary, Svc};
use crate::api::AppState;
use crate::service::tenancy;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::tenancy::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = TenantServiceExt::register(Svc::new(app), router);
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
}

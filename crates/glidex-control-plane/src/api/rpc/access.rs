//! `glidex.access.v1` over RPC; the logic is in `service::access`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::access as svc;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::access::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = ProjectServiceExt::register(Svc::new(app), router);
    let router = UserServiceExt::register(Svc::new(app), router);
    let router = TeamServiceExt::register(Svc::new(app), router);
    let router = TokenServiceExt::register(Svc::new(app), router);
    let router = SystemBindingServiceExt::register(Svc::new(app), router);
    let router = PolicyServiceExt::register(Svc::new(app), router);
    let router = AuditServiceExt::register(Svc::new(app), router);
    AuthzServiceExt::register(Svc::new(app), router)
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
impl ProjectService for Svc {
    async fn list_projects(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListProjectsRequest>,
    ) -> ServiceResult<ListProjectsResponse> {
        op_async!(self, ctx, req, svc::list_projects)
    }

    async fn create_project(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateProjectRequest>,
    ) -> ServiceResult<CreateProjectResponse> {
        op!(self, ctx, req, svc::create_project)
    }

    async fn get_project(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetProjectRequest>,
    ) -> ServiceResult<GetProjectResponse> {
        op_async!(self, ctx, req, svc::get_project)
    }

    async fn update_project(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateProjectRequest>,
    ) -> ServiceResult<UpdateProjectResponse> {
        op!(self, ctx, req, svc::update_project)
    }

    async fn delete_project(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteProjectRequest>,
    ) -> ServiceResult<DeleteProjectResponse> {
        op_async!(self, ctx, req, svc::delete_project)
    }

    async fn list_project_bindings(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListProjectBindingsRequest>,
    ) -> ServiceResult<ListProjectBindingsResponse> {
        op!(self, ctx, req, svc::list_project_bindings)
    }

    async fn add_project_binding(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AddProjectBindingRequest>,
    ) -> ServiceResult<AddProjectBindingResponse> {
        op!(self, ctx, req, svc::add_project_binding)
    }

    async fn remove_project_binding(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RemoveProjectBindingRequest>,
    ) -> ServiceResult<RemoveProjectBindingResponse> {
        op!(self, ctx, req, svc::remove_project_binding)
    }
}

#[allow(refining_impl_trait)]
impl UserService for Svc {
    async fn list_users(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListUsersRequest>,
    ) -> ServiceResult<ListUsersResponse> {
        op!(self, ctx, req, svc::list_users)
    }

    async fn create_user(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateUserRequest>,
    ) -> ServiceResult<CreateUserResponse> {
        op!(self, ctx, req, svc::create_user)
    }

    async fn update_user(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateUserRequest>,
    ) -> ServiceResult<UpdateUserResponse> {
        op!(self, ctx, req, svc::update_user)
    }

    async fn delete_user(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteUserRequest>,
    ) -> ServiceResult<DeleteUserResponse> {
        op!(self, ctx, req, svc::delete_user)
    }

    async fn link_identity(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, LinkIdentityRequest>,
    ) -> ServiceResult<LinkIdentityResponse> {
        op!(self, ctx, req, svc::link_identity)
    }

    async fn unlink_identity(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UnlinkIdentityRequest>,
    ) -> ServiceResult<UnlinkIdentityResponse> {
        op!(self, ctx, req, svc::unlink_identity)
    }

    async fn list_passkeys(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListPasskeysRequest>,
    ) -> ServiceResult<ListPasskeysResponse> {
        op!(self, ctx, req, svc::list_passkeys)
    }

    async fn rename_passkey(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RenamePasskeyRequest>,
    ) -> ServiceResult<RenamePasskeyResponse> {
        op!(self, ctx, req, svc::rename_passkey)
    }

    async fn delete_passkey(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeletePasskeyRequest>,
    ) -> ServiceResult<DeletePasskeyResponse> {
        op!(self, ctx, req, svc::delete_passkey)
    }

    async fn update_me(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateMeRequest>,
    ) -> ServiceResult<UpdateMeResponse> {
        op!(self, ctx, req, svc::update_me)
    }

    async fn get_my_ssh_keys(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetMySshKeysRequest>,
    ) -> ServiceResult<GetMySshKeysResponse> {
        op_async!(self, ctx, req, svc::my_ssh_keys)
    }
}

#[allow(refining_impl_trait)]
impl TeamService for Svc {
    async fn list_teams(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListTeamsRequest>,
    ) -> ServiceResult<ListTeamsResponse> {
        op!(self, ctx, req, svc::list_teams)
    }

    async fn create_team(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateTeamRequest>,
    ) -> ServiceResult<CreateTeamResponse> {
        op!(self, ctx, req, svc::create_team)
    }

    async fn update_team(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateTeamRequest>,
    ) -> ServiceResult<UpdateTeamResponse> {
        op!(self, ctx, req, svc::update_team)
    }

    async fn delete_team(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteTeamRequest>,
    ) -> ServiceResult<DeleteTeamResponse> {
        op!(self, ctx, req, svc::delete_team)
    }

    async fn add_team_member(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AddTeamMemberRequest>,
    ) -> ServiceResult<AddTeamMemberResponse> {
        op!(self, ctx, req, svc::add_team_member)
    }

    async fn remove_team_member(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RemoveTeamMemberRequest>,
    ) -> ServiceResult<RemoveTeamMemberResponse> {
        op!(self, ctx, req, svc::remove_team_member)
    }
}

#[allow(refining_impl_trait)]
impl TokenService for Svc {
    async fn list_tokens(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListTokensRequest>,
    ) -> ServiceResult<ListTokensResponse> {
        op!(self, ctx, req, svc::list_tokens)
    }

    async fn create_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateTokenRequest>,
    ) -> ServiceResult<CreateTokenResponse> {
        op!(self, ctx, req, svc::create_token)
    }

    async fn revoke_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RevokeTokenRequest>,
    ) -> ServiceResult<RevokeTokenResponse> {
        op!(self, ctx, req, svc::revoke_token)
    }
}

#[allow(refining_impl_trait)]
impl SystemBindingService for Svc {
    async fn list_system_bindings(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListSystemBindingsRequest>,
    ) -> ServiceResult<ListSystemBindingsResponse> {
        op!(self, ctx, req, svc::list_system_bindings)
    }

    async fn add_system_binding(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AddSystemBindingRequest>,
    ) -> ServiceResult<AddSystemBindingResponse> {
        op!(self, ctx, req, svc::add_system_binding)
    }

    async fn remove_system_binding(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RemoveSystemBindingRequest>,
    ) -> ServiceResult<RemoveSystemBindingResponse> {
        op!(self, ctx, req, svc::remove_system_binding)
    }
}

#[allow(refining_impl_trait)]
impl PolicyService for Svc {
    async fn list_policies(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListPoliciesRequest>,
    ) -> ServiceResult<ListPoliciesResponse> {
        op!(self, ctx, req, svc::list_policies)
    }

    async fn get_policy(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetPolicyRequest>,
    ) -> ServiceResult<GetPolicyResponse> {
        op!(self, ctx, req, svc::get_policy)
    }

    async fn list_policy_versions(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListPolicyVersionsRequest>,
    ) -> ServiceResult<ListPolicyVersionsResponse> {
        op!(self, ctx, req, svc::policy_versions)
    }

    async fn put_policy(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PutPolicyRequest>,
    ) -> ServiceResult<PutPolicyResponse> {
        op!(self, ctx, req, svc::put_policy)
    }

    async fn delete_policy(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeletePolicyRequest>,
    ) -> ServiceResult<DeletePolicyResponse> {
        op!(self, ctx, req, svc::delete_policy)
    }

    async fn validate_policy(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ValidatePolicyRequest>,
    ) -> ServiceResult<ValidatePolicyResponse> {
        op!(self, ctx, req, svc::validate_policy)
    }

    async fn simulate_policy(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, SimulatePolicyRequest>,
    ) -> ServiceResult<SimulatePolicyResponse> {
        op_async!(self, ctx, req, svc::simulate_policy)
    }

    async fn reload_policies(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ReloadPoliciesRequest>,
    ) -> ServiceResult<ReloadPoliciesResponse> {
        op!(self, ctx, req, svc::reload_policies)
    }
}

#[allow(refining_impl_trait)]
impl AuditService for Svc {
    async fn read_audit(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ReadAuditRequest>,
    ) -> ServiceResult<ReadAuditResponse> {
        op!(self, ctx, req, svc::read_audit)
    }
}

#[allow(refining_impl_trait)]
impl AuthzService for Svc {
    async fn check(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CheckRequest>,
    ) -> ServiceResult<CheckResponse> {
        op_async!(self, ctx, req, svc::check)
    }

    async fn allowed(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AllowedRequest>,
    ) -> ServiceResult<AllowedResponse> {
        op_async!(self, ctx, req, svc::allowed)
    }
}

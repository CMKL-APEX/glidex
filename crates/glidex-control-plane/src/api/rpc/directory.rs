//! `glidex.directory.v1` over RPC; the logic is in `service::directory`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::{directory, federations, invitations, signing_keys};
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::directory::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = ConnectorServiceExt::register(Svc::new(app), router);
    let router = FederationServiceExt::register(Svc::new(app), router);
    let router = SigningKeyServiceExt::register(Svc::new(app), router);
    InvitationServiceExt::register(Svc::new(app), router)
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

    async fn list_scim_tokens(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListScimTokensRequest>,
    ) -> ServiceResult<ListScimTokensResponse> {
        op!(self, ctx, req, directory::list_scim_tokens)
    }

    async fn create_scim_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateScimTokenRequest>,
    ) -> ServiceResult<CreateScimTokenResponse> {
        op!(self, ctx, req, directory::create_scim_token)
    }

    async fn revoke_scim_token(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RevokeScimTokenRequest>,
    ) -> ServiceResult<RevokeScimTokenResponse> {
        op!(self, ctx, req, directory::revoke_scim_token)
    }

    async fn sync_connector(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, SyncConnectorRequest>,
    ) -> ServiceResult<SyncConnectorResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            directory::sync_connector(&c.expect("authenticated operation"), req).await
        })
        .await
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

#[allow(refining_impl_trait)]
impl InvitationService for Svc {
    async fn list_invitations(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListInvitationsRequest>,
    ) -> ServiceResult<ListInvitationsResponse> {
        op!(self, ctx, req, invitations::list_invitations)
    }

    async fn create_invitation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateInvitationRequest>,
    ) -> ServiceResult<CreateInvitationResponse> {
        op!(self, ctx, req, invitations::create_invitation)
    }

    async fn revoke_invitation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RevokeInvitationRequest>,
    ) -> ServiceResult<RevokeInvitationResponse> {
        op!(self, ctx, req, invitations::revoke_invitation)
    }
}

#[allow(refining_impl_trait)]
impl FederationService for Svc {
    async fn list_federations(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListFederationsRequest>,
    ) -> ServiceResult<ListFederationsResponse> {
        op!(self, ctx, req, federations::list_federations)
    }

    async fn get_federation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetFederationRequest>,
    ) -> ServiceResult<GetFederationResponse> {
        op!(self, ctx, req, federations::get_federation)
    }

    async fn create_federation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateFederationRequest>,
    ) -> ServiceResult<CreateFederationResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            federations::create_federation(&c.expect("authenticated operation"), req).await
        })
        .await
    }

    async fn update_federation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateFederationRequest>,
    ) -> ServiceResult<UpdateFederationResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            federations::update_federation(&c.expect("authenticated operation"), req).await
        })
        .await
    }

    async fn delete_federation(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteFederationRequest>,
    ) -> ServiceResult<DeleteFederationResponse> {
        op!(self, ctx, req, federations::delete_federation)
    }
}

#[allow(refining_impl_trait)]
impl SigningKeyService for Svc {
    async fn list_signing_keys(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListSigningKeysRequest>,
    ) -> ServiceResult<ListSigningKeysResponse> {
        op!(self, ctx, req, signing_keys::list_signing_keys)
    }

    async fn rotate_signing_key(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RotateSigningKeyRequest>,
    ) -> ServiceResult<RotateSigningKeyResponse> {
        op!(self, ctx, req, signing_keys::rotate_signing_key)
    }
}

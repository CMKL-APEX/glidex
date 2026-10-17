//! `glidex.watch.v1` over RPC: the watch stream as a server stream (the
//! logic is in `service::watch`). Opening it is checked and audited like
//! any call; a refusal (too many watchers, an unknown project) is the
//! call's error, before any event.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::watch as svc;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult, ServiceStream};
use glidex_api::proto::glidex::watch::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    WatchServiceExt::register(Svc::new(app), router)
}

#[allow(refining_impl_trait)]
impl WatchService for Svc {
    async fn watch(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, WatchRequest>,
    ) -> ServiceResult<ServiceStream<WatchResponse>> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            let c = c.expect("authenticated operation");
            let rx = svc::open(c, svc::kinds_of(&req), req.project.as_deref(), None).await?;
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|e| (Ok(svc::event_pb(&e)), rx))
            });
            Ok(Box::pin(stream) as ServiceStream<WatchResponse>)
        })
        .await
    }
}

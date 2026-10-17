//! `glidex.watch.v1` over RPC: the watch stream as a server stream (the
//! logic is in `service::watch`). Opening it is checked and audited like
//! any call; a refusal (too many watchers, an unknown project) is the
//! call's error, before any event.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::watch as svc;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult, ServiceStream};
use glidex_api::proto::glidex::watch::v1::*;

const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);

fn keepalive() -> WatchResponse {
    WatchResponse {
        r#type: WatchEventType::WATCH_EVENT_TYPE_KEEPALIVE.into(),
        ..Default::default()
    }
}

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
        // Through a node's UI, the cluster summary is about that node.
        let viewer = ctx
            .extensions()
            .get::<crate::api::RelayedBy>()
            .map(|v| v.0.clone());
        unary(&self.app, &ctx, |c| async move {
            let c = c.expect("authenticated operation");
            let rx = svc::open(c, svc::kinds_of(&req), req.project.as_deref(), viewer).await?;
            // Events, and a keep-alive whenever 15 s pass without one (as
            // the SSE stream's), so a proxy's idle timeout doesn't cut it.
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                match tokio::time::timeout(KEEPALIVE, rx.recv()).await {
                    Ok(Some(e)) => Some((Ok(svc::event_pb(&e)), rx)),
                    Ok(None) => None,
                    Err(_) => Some((Ok(keepalive()), rx)),
                }
            });
            Ok(Box::pin(stream) as ServiceStream<WatchResponse>)
        })
        .await
    }
}

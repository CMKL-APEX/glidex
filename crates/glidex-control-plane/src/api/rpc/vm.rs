//! `glidex.vm.v1` over RPC; the logic is in `service::vm`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::vm as svc;
use connectrpc::{InboundStream, RequestContext, ServiceRequest, ServiceResult, ServiceStream};
use glidex_api::proto::glidex::vm::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    VmServiceExt::register(Svc::new(app), router)
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

/// A VM write: the VM and whether it settled.
macro_rules! vm_write {
    ($self:ident, $ctx:ident, $req:ident, $f:path, $resp:ident) => {{
        let req = $req.to_owned_message();
        unary(&$self.app, &$ctx, |c| async move {
            let (vm, settled) = $f(&c.expect("authenticated operation"), req).await?.parts();
            Ok($resp {
                vm: vm.into(),
                settled,
                ..Default::default()
            })
        })
        .await
    }};
}

// The generated trait returns `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl VmService for Svc {
    async fn console(
        &self,
        ctx: RequestContext,
        mut requests: InboundStream<ConsoleRequest>,
    ) -> ServiceResult<ServiceStream<ConsoleResponse>> {
        use crate::service::ServiceError;
        use axum::http::StatusCode;
        use futures_util::StreamExt;
        use glidex_api::proto::glidex::vm::v1::__buffa::oneof::console_request::Request as R;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let app = self.app.clone();
        unary(&self.app, &ctx, |c| async move {
            let c = c.expect("authenticated operation");
            let bad = |m: &str| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", m);
            // The first message says which console (soon: a stream that
            // never says doesn't hold a handler).
            let first = tokio::time::timeout(std::time::Duration::from_secs(30), requests.next())
                .await
                .map_err(|_| bad("no {open: {id}} within 30 s"))?;
            let open = match first {
                Some(Ok(m)) => match m.to_owned_message().request {
                    Some(R::Open(o)) => *o,
                    _ => return Err(bad("the first message opens the console: {open: {id}}")),
                },
                _ => return Err(bad("the first message opens the console: {open: {id}}")),
            };
            let target = svc::console_target(&c, &open.id, open.ticket.as_deref()).await?;
            let io = svc::console_connect(&app.manager, target)
                .await
                .map_err(|m| {
                    ServiceError::new(StatusCode::SERVICE_UNAVAILABLE, "console_unavailable", m)
                })?;
            let (rx, mut tx) = tokio::io::split(io);
            // Keystrokes in, until the client stops sending (a half-close
            // ends the call, as closing a WebSocket does) or the console goes.
            let (done_tx, done) = tokio::sync::oneshot::channel::<()>();
            tokio::spawn(async move {
                while let Some(Ok(m)) = requests.next().await {
                    if let Some(R::Input(bytes)) = m.to_owned_message().request
                        && tx.write_all(&bytes).await.is_err()
                    {
                        break;
                    }
                }
                let _ = tx.shutdown().await;
                let _ = done_tx.send(());
            });
            // The console's bytes out, until it closes or the client is done.
            let out = futures_util::stream::unfold((rx, done), |(mut rx, mut done)| async move {
                let mut buf = vec![0u8; 4096];
                tokio::select! {
                    read = rx.read(&mut buf) => match read {
                        Ok(0) | Err(_) => None,
                        Ok(n) => {
                            buf.truncate(n);
                            Some((
                                Ok(ConsoleResponse {
                                    output: buf,
                                    ..Default::default()
                                }),
                                (rx, done),
                            ))
                        }
                    },
                    _ = &mut done => None,
                }
            });
            Ok(Box::pin(out) as ServiceStream<ConsoleResponse>)
        })
        .await
    }

    async fn list_vms(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListVmsRequest>,
    ) -> ServiceResult<ListVmsResponse> {
        op!(self, ctx, req, svc::list_vms)
    }

    async fn create_vm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateVmRequest>,
    ) -> ServiceResult<CreateVmResponse> {
        vm_write!(self, ctx, req, svc::create_vm, CreateVmResponse)
    }

    async fn preview_placement(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PreviewPlacementRequest>,
    ) -> ServiceResult<PreviewPlacementResponse> {
        op!(self, ctx, req, svc::preview_placement)
    }

    async fn get_vm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetVmRequest>,
    ) -> ServiceResult<GetVmResponse> {
        op!(self, ctx, req, svc::get_vm)
    }

    async fn update_vm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateVmRequest>,
    ) -> ServiceResult<UpdateVmResponse> {
        vm_write!(self, ctx, req, svc::update_vm, UpdateVmResponse)
    }

    async fn delete_vm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteVmRequest>,
    ) -> ServiceResult<DeleteVmResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            let vm = svc::delete_vm(&c.expect("authenticated operation"), req).await?;
            Ok(DeleteVmResponse {
                vm: vm.into(),
                ..Default::default()
            })
        })
        .await
    }

    async fn start_vm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, StartVmRequest>,
    ) -> ServiceResult<StartVmResponse> {
        vm_write!(self, ctx, req, svc::start_vm, StartVmResponse)
    }

    async fn place_vm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PlaceVmRequest>,
    ) -> ServiceResult<PlaceVmResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            svc::place_vm(&c.expect("authenticated operation"), req).await
        })
        .await
    }

    async fn stop_vm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, StopVmRequest>,
    ) -> ServiceResult<StopVmResponse> {
        vm_write!(self, ctx, req, svc::stop_vm, StopVmResponse)
    }

    async fn pause_vm(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PauseVmRequest>,
    ) -> ServiceResult<PauseVmResponse> {
        vm_write!(self, ctx, req, svc::pause_vm, PauseVmResponse)
    }

    async fn list_vm_events(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListVmEventsRequest>,
    ) -> ServiceResult<ListVmEventsResponse> {
        op!(self, ctx, req, svc::vm_events)
    }

    async fn get_console_info(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetConsoleInfoRequest>,
    ) -> ServiceResult<GetConsoleInfoResponse> {
        op!(self, ctx, req, svc::console_info)
    }

    async fn create_console_ticket(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateConsoleTicketRequest>,
    ) -> ServiceResult<CreateConsoleTicketResponse> {
        op!(self, ctx, req, svc::console_ticket)
    }

    async fn get_console_log(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetConsoleLogRequest>,
    ) -> ServiceResult<GetConsoleLogResponse> {
        op!(self, ctx, req, svc::console_log)
    }

    async fn attach_device(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AttachDeviceRequest>,
    ) -> ServiceResult<AttachDeviceResponse> {
        vm_write!(self, ctx, req, svc::attach_device, AttachDeviceResponse)
    }

    async fn detach_device(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DetachDeviceRequest>,
    ) -> ServiceResult<DetachDeviceResponse> {
        vm_write!(self, ctx, req, svc::detach_device, DetachDeviceResponse)
    }

    async fn attach_disk(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AttachDiskRequest>,
    ) -> ServiceResult<AttachDiskResponse> {
        vm_write!(self, ctx, req, svc::attach_disk, AttachDiskResponse)
    }

    async fn detach_disk(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DetachDiskRequest>,
    ) -> ServiceResult<DetachDiskResponse> {
        vm_write!(self, ctx, req, svc::detach_disk, DetachDiskResponse)
    }
}

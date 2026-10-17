//! `glidex.vm.v1` over RPC; the logic is in `service::vm`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::vm as svc;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
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

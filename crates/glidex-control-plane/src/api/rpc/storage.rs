//! `glidex.storage.v1` over RPC; the logic is in `service::storage`.

use super::{Svc, unary};
use crate::api::AppState;
use crate::service::storage as svc;
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};
use glidex_api::proto::glidex::storage::v1::*;

pub(super) fn register(app: &AppState, router: connectrpc::Router) -> connectrpc::Router {
    let router = CredentialServiceExt::register(Svc::new(app), router);
    let router = ImageServiceExt::register(Svc::new(app), router);
    let router = DatastoreServiceExt::register(Svc::new(app), router);
    DiskServiceExt::register(Svc::new(app), router)
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

/// A disk write: the disk and whether it settled.
macro_rules! disk_write {
    ($self:ident, $ctx:ident, $req:ident, $f:path, $resp:ident) => {{
        let req = $req.to_owned_message();
        unary(&$self.app, &$ctx, |c| async move {
            let (disk, settled) = $f(&c.expect("authenticated operation"), req).await?.parts();
            Ok($resp {
                disk: disk.into(),
                settled,
                ..Default::default()
            })
        })
        .await
    }};
}

// The generated traits return `impl Encodable<_>`; the concrete message is fine.
#[allow(refining_impl_trait)]
impl CredentialService for Svc {
    async fn list_credentials(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListCredentialsRequest>,
    ) -> ServiceResult<ListCredentialsResponse> {
        op!(self, ctx, req, svc::list_credentials)
    }

    async fn create_credential(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateCredentialRequest>,
    ) -> ServiceResult<CreateCredentialResponse> {
        op!(self, ctx, req, svc::create_credential)
    }

    async fn get_credential(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetCredentialRequest>,
    ) -> ServiceResult<GetCredentialResponse> {
        op!(self, ctx, req, svc::get_credential)
    }

    async fn update_credential(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateCredentialRequest>,
    ) -> ServiceResult<UpdateCredentialResponse> {
        op!(self, ctx, req, svc::update_credential)
    }

    async fn delete_credential(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteCredentialRequest>,
    ) -> ServiceResult<DeleteCredentialResponse> {
        op_async!(self, ctx, req, svc::delete_credential)
    }
}

#[allow(refining_impl_trait)]
impl ImageService for Svc {
    async fn get_image_catalog(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetImageCatalogRequest>,
    ) -> ServiceResult<GetImageCatalogResponse> {
        op!(self, ctx, req, svc::image_catalog)
    }

    async fn get_firmware_catalog(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetFirmwareCatalogRequest>,
    ) -> ServiceResult<GetFirmwareCatalogResponse> {
        op!(self, ctx, req, svc::firmware_catalog)
    }

    async fn list_images(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListImagesRequest>,
    ) -> ServiceResult<ListImagesResponse> {
        op_async!(self, ctx, req, svc::list_images)
    }

    async fn get_image(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetImageRequest>,
    ) -> ServiceResult<GetImageResponse> {
        op_async!(self, ctx, req, svc::get_image)
    }

    async fn pull_image(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, PullImageRequest>,
    ) -> ServiceResult<PullImageResponse> {
        op_async!(self, ctx, req, svc::pull_image)
    }

    async fn extract_vm_kernel(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ExtractVmKernelRequest>,
    ) -> ServiceResult<ExtractVmKernelResponse> {
        op_async!(self, ctx, req, crate::service::vm::extract_vm_kernel)
    }

    async fn delete_image(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteImageRequest>,
    ) -> ServiceResult<DeleteImageResponse> {
        op_async!(self, ctx, req, svc::delete_image)
    }

    async fn retry_image(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RetryImageRequest>,
    ) -> ServiceResult<RetryImageResponse> {
        op!(self, ctx, req, svc::retry_image)
    }

    async fn add_image_copy(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, AddImageCopyRequest>,
    ) -> ServiceResult<AddImageCopyResponse> {
        op_async!(self, ctx, req, svc::add_image_copy)
    }

    async fn remove_image_copy(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RemoveImageCopyRequest>,
    ) -> ServiceResult<RemoveImageCopyResponse> {
        op_async!(self, ctx, req, svc::remove_image_copy)
    }

    async fn list_image_events(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListImageEventsRequest>,
    ) -> ServiceResult<ListImageEventsResponse> {
        op_async!(self, ctx, req, svc::image_events)
    }
}

#[allow(refining_impl_trait)]
impl DiskService for Svc {
    async fn list_disks(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListDisksRequest>,
    ) -> ServiceResult<ListDisksResponse> {
        op!(self, ctx, req, svc::list_disks)
    }

    async fn get_disk(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetDiskRequest>,
    ) -> ServiceResult<GetDiskResponse> {
        op_async!(self, ctx, req, svc::get_disk)
    }

    async fn create_disk(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateDiskRequest>,
    ) -> ServiceResult<CreateDiskResponse> {
        disk_write!(self, ctx, req, svc::create_disk, CreateDiskResponse)
    }

    async fn resize_disk(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ResizeDiskRequest>,
    ) -> ServiceResult<ResizeDiskResponse> {
        disk_write!(self, ctx, req, svc::resize_disk, ResizeDiskResponse)
    }

    async fn move_disk(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, MoveDiskRequest>,
    ) -> ServiceResult<MoveDiskResponse> {
        disk_write!(self, ctx, req, svc::move_disk, MoveDiskResponse)
    }

    async fn extend_root(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ExtendRootRequest>,
    ) -> ServiceResult<ExtendRootResponse> {
        disk_write!(self, ctx, req, svc::extend_root, ExtendRootResponse)
    }

    async fn delete_disk(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteDiskRequest>,
    ) -> ServiceResult<DeleteDiskResponse> {
        let req = req.to_owned_message();
        unary(&self.app, &ctx, |c| async move {
            let still = svc::delete_disk(&c.expect("authenticated operation"), req).await?;
            Ok(DeleteDiskResponse {
                disk: still.as_ref().map(svc::disk_pb).into(),
                ..Default::default()
            })
        })
        .await
    }

    async fn list_disk_events(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListDiskEventsRequest>,
    ) -> ServiceResult<ListDiskEventsResponse> {
        op_async!(self, ctx, req, svc::disk_events)
    }
}

#[allow(refining_impl_trait)]
impl DatastoreService for Svc {
    async fn list_datastores(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListDatastoresRequest>,
    ) -> ServiceResult<ListDatastoresResponse> {
        op!(self, ctx, req, svc::list_datastores)
    }

    async fn get_datastore(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, GetDatastoreRequest>,
    ) -> ServiceResult<GetDatastoreResponse> {
        op!(self, ctx, req, svc::get_datastore)
    }

    async fn create_datastore(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CreateDatastoreRequest>,
    ) -> ServiceResult<CreateDatastoreResponse> {
        op_async!(self, ctx, req, svc::create_datastore)
    }

    async fn update_datastore(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, UpdateDatastoreRequest>,
    ) -> ServiceResult<UpdateDatastoreResponse> {
        op_async!(self, ctx, req, svc::update_datastore)
    }

    async fn delete_datastore(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, DeleteDatastoreRequest>,
    ) -> ServiceResult<DeleteDatastoreResponse> {
        op_async!(self, ctx, req, svc::delete_datastore)
    }
}

//! `glidex.storage.v1` (spec/10-images.md §8, spec/08-credentials.md): guest
//! credentials, images and disks, scoped to projects. The REST twins in
//! `api/storage.rs` and the RPC adapter in `api/rpc/storage.rs` both call
//! these.

use super::common::{arch_of, arch_pb, condition_pb, event_pb, hypervisor_of, hypervisor_pb, ts};
use super::{Call, ServiceError, ServiceResult};
use crate::api::{credential_entities, disk_entities, project_entities};
use crate::authz::{Ent, EntitySet};
use crate::images::{self as img, DiskResponse, ImageResponse};
use axum::http::StatusCode;
use glidex_api::proto::glidex::storage::v1 as pb;

fn manager_err(e: crate::state::VmManagerError) -> ServiceError {
    crate::api::error_to_response(e).into()
}

// ---- conversions ----------------------------------------------------------

fn format_pb(f: img::qemu_img::DiskFormat) -> pb::DiskFormat {
    match f {
        img::qemu_img::DiskFormat::Qcow2 => pb::DiskFormat::DISK_FORMAT_QCOW2,
        img::qemu_img::DiskFormat::Raw => pb::DiskFormat::DISK_FORMAT_RAW,
    }
}

fn format_of(f: pb::DiskFormat) -> Option<img::qemu_img::DiskFormat> {
    match f {
        pb::DiskFormat::DISK_FORMAT_QCOW2 => Some(img::qemu_img::DiskFormat::Qcow2),
        pb::DiskFormat::DISK_FORMAT_RAW => Some(img::qemu_img::DiskFormat::Raw),
        pb::DiskFormat::DISK_FORMAT_UNSPECIFIED => None,
    }
}

fn clone_pb(m: img::CloneMode) -> pb::CloneMode {
    match m {
        img::CloneMode::Linked => pb::CloneMode::CLONE_MODE_LINKED,
        img::CloneMode::Full => pb::CloneMode::CLONE_MODE_FULL,
    }
}

fn clone_of(m: pb::CloneMode) -> Option<img::CloneMode> {
    match m {
        pb::CloneMode::CLONE_MODE_LINKED => Some(img::CloneMode::Linked),
        pb::CloneMode::CLONE_MODE_FULL => Some(img::CloneMode::Full),
        pb::CloneMode::CLONE_MODE_UNSPECIFIED => None,
    }
}

fn kind_pb(k: img::ImageKind) -> pb::ImageKind {
    match k {
        img::ImageKind::Disk => pb::ImageKind::IMAGE_KIND_DISK,
        img::ImageKind::Firmware => pb::ImageKind::IMAGE_KIND_FIRMWARE,
        img::ImageKind::Kernel => pb::ImageKind::IMAGE_KIND_KERNEL,
    }
}

fn info_pb(i: &img::qemu_img::ImgInfo) -> pb::ImgInfo {
    pb::ImgInfo {
        format: i.format.clone(),
        virtual_size: i.virtual_size,
        actual_size: i.actual_size,
        backing_filename: i.backing_filename.clone(),
        backing_format: i.backing_format.clone(),
        ..Default::default()
    }
}

pub fn credential_pb(c: &crate::credentials::CredentialInfo) -> pb::Credential {
    pb::Credential {
        username: c.username.clone(),
        project: c.project.clone(),
        has_password: Some(c.has_password),
        ssh_authorized_keys: c.ssh_authorized_keys.clone(),
        created_at: ts(c.created_at).into(),
        updated_at: ts(c.updated_at).into(),
        ..Default::default()
    }
}

pub fn image_pb(i: &ImageResponse) -> pb::Image {
    use pb::__buffa::oneof::image_source::Source;
    let source = match &i.source {
        img::ImageSource::Catalog { key, url, version } => {
            Source::Catalog(Box::new(pb::CatalogSource {
                key: key.clone(),
                url: url.clone(),
                version: version.clone(),
                ..Default::default()
            }))
        }
        img::ImageSource::Firmware { key, url, version } => {
            Source::Firmware(Box::new(pb::CatalogSource {
                key: key.clone(),
                url: url.clone(),
                version: version.clone(),
                ..Default::default()
            }))
        }
        img::ImageSource::Url {
            url,
            expected_sha256,
        } => Source::Url(Box::new(pb::UrlSource {
            url: url.clone(),
            expected_sha256: expected_sha256.clone(),
            ..Default::default()
        })),
        img::ImageSource::Host { path } => Source::Host(Box::new(pb::HostSource {
            path: path.clone(),
            ..Default::default()
        })),
        img::ImageSource::Extracted {
            image_id,
            disk_id,
            image_sha256,
            path,
            ..
        } => Source::Extracted(Box::new(pb::ExtractedSource {
            image_id: image_id.clone(),
            disk_id: disk_id.clone(),
            image_sha256: image_sha256.clone(),
            path: path.clone(),
            ..Default::default()
        })),
    };
    let status = match &i.status {
        img::ImageStatus::Downloading {
            received_bytes,
            total_bytes,
        } => pb::ImageStatus {
            state: pb::ImageState::IMAGE_STATE_DOWNLOADING.into(),
            received_bytes: *received_bytes,
            total_bytes: *total_bytes,
            ..Default::default()
        },
        img::ImageStatus::Verifying => pb::ImageStatus {
            state: pb::ImageState::IMAGE_STATE_VERIFYING.into(),
            ..Default::default()
        },
        img::ImageStatus::Extracting => pb::ImageStatus {
            state: pb::ImageState::IMAGE_STATE_EXTRACTING.into(),
            ..Default::default()
        },
        img::ImageStatus::Ready => pb::ImageStatus {
            state: pb::ImageState::IMAGE_STATE_READY.into(),
            ..Default::default()
        },
        img::ImageStatus::Failed { reason } => pb::ImageStatus {
            state: pb::ImageState::IMAGE_STATE_FAILED.into(),
            reason: reason.clone(),
            ..Default::default()
        },
        img::ImageStatus::Missing => pb::ImageStatus {
            state: pb::ImageState::IMAGE_STATE_MISSING.into(),
            ..Default::default()
        },
    };
    pb::Image {
        id: i.id.clone(),
        name: i.name.clone(),
        kind: kind_pb(i.kind).into(),
        hypervisor: i.hypervisor.map(hypervisor_pb).unwrap_or_default().into(),
        source: pb::ImageSource {
            source: Some(source),
            ..Default::default()
        }
        .into(),
        status: status.into(),
        format: format_pb(i.format).into(),
        virtual_size_bytes: i.virtual_size_bytes,
        file_size_bytes: i.file_size_bytes,
        sha256: i.sha256.clone(),
        arch: arch_pb(i.arch).into(),
        created_at: ts(i.created_at).into(),
        verified: Some(i.verified),
        path: i.path.clone(),
        linked_disks: i.linked_disks.clone(),
        vars_template: i.vars_template.then_some(true),
        used_by_vms: i.used_by_vms.clone(),
        copies: i
            .copies
            .iter()
            .map(|c| pb::ImageCopy {
                node: c.node.clone(),
                node_name: c.node_name.clone(),
                state: c.state.clone(),
                bytes: c.bytes,
                error: c.error.clone(),
                pinned: c.pinned.then_some(true),
                datastore: c.datastore.clone(),
                datastore_name: c.datastore_name.clone(),
                ..Default::default()
            })
            .collect(),
        info: i.info.as_ref().map(info_pb).into(),
        deleting: i.deleting.then_some(true),
        for_image: i.for_image.clone(),
        owner: i.owner.clone(),
        kernel: i
            .kernel
            .as_ref()
            .map(|k| pb::KernelInfo {
                release: k.release.clone(),
                cmdline: k.cmdline.clone(),
                initrd: k
                    .initrd
                    .as_ref()
                    .map(|f| pb::FileInfo {
                        sha256: f.sha256.clone(),
                        size_bytes: f.size_bytes,
                        ..Default::default()
                    })
                    .into(),
                ..Default::default()
            })
            .into(),
        ..Default::default()
    }
}

pub fn disk_pb(d: &DiskResponse) -> pb::Disk {
    use pb::__buffa::oneof::disk_origin::Origin;
    let origin = match &d.origin {
        img::DiskOrigin::Blank => Origin::Blank(Box::default()),
        img::DiskOrigin::Image { image_id, mode } => Origin::Image(Box::new(pb::ImageOrigin {
            image_id: image_id.clone(),
            mode: clone_pb(*mode).into(),
            ..Default::default()
        })),
    };
    let phase = match d.phase {
        img::DiskPhase::Pending => pb::DiskPhase::DISK_PHASE_PENDING,
        img::DiskPhase::Creating => pb::DiskPhase::DISK_PHASE_CREATING,
        img::DiskPhase::Ready => pb::DiskPhase::DISK_PHASE_READY,
        img::DiskPhase::Resizing => pb::DiskPhase::DISK_PHASE_RESIZING,
        img::DiskPhase::Missing => pb::DiskPhase::DISK_PHASE_MISSING,
        img::DiskPhase::Failed => pb::DiskPhase::DISK_PHASE_FAILED,
    };
    let table = d.partition_table.as_ref().map(|t| pb::PartitionTable {
        kind: match t.kind {
            img::partition::TableKind::Gpt => pb::PartitionTableKind::PARTITION_TABLE_KIND_GPT,
            img::partition::TableKind::Mbr => pb::PartitionTableKind::PARTITION_TABLE_KIND_MBR,
        }
        .into(),
        partitions: t
            .partitions
            .iter()
            .map(|p| pb::Partition {
                number: p.number,
                start_bytes: p.start_bytes,
                size_bytes: p.size_bytes,
                r#type: p.type_id.clone(),
                is_root: Some(p.is_root),
                ..Default::default()
            })
            .collect(),
        free_tail_bytes: t.free_tail_bytes,
        ..Default::default()
    });
    let extend = d.extend_root.map(|o| match o {
        img::ExtendOutcome::Grown => pb::ExtendOutcome::EXTEND_OUTCOME_GROWN,
        img::ExtendOutcome::AlreadyFull => pb::ExtendOutcome::EXTEND_OUTCOME_ALREADY_FULL,
        img::ExtendOutcome::OnBoot => pb::ExtendOutcome::EXTEND_OUTCOME_ON_BOOT,
        img::ExtendOutcome::Skipped => pb::ExtendOutcome::EXTEND_OUTCOME_SKIPPED,
    });
    pb::Disk {
        id: d.id.clone(),
        name: d.name.clone(),
        project: d.project.clone(),
        format: format_pb(d.format).into(),
        size_bytes: d.size_bytes,
        origin: pb::DiskOrigin {
            origin: Some(origin),
            ..Default::default()
        }
        .into(),
        attached_to: d.attached_to.clone(),
        node: d.node.clone(),
        node_name: d.node_name.clone(),
        pending_growpart: Some(d.pending_growpart),
        status: d.status.clone(),
        phase: phase.into(),
        conditions: d.conditions.iter().map(condition_pb).collect(),
        pending_size_bytes: d.pending_size_bytes,
        owner: d.owner.clone(),
        deleting: d.deleting.then_some(true),
        busy_op: d.busy_op.clone(),
        path: d.path.clone(),
        created_at: ts(d.created_at).into(),
        info: d.info.as_ref().map(info_pb).into(),
        partition_table: table.into(),
        extend_root: extend.unwrap_or_default().into(),
        warnings: d.warnings.clone(),
        datastore: d.datastore.clone(),
        worker: d.worker.clone(),
        worker_name: d.worker_name.clone(),
        moving_to: d.moving_to.clone(),
        ..Default::default()
    }
}

// ---- guest credentials ----------------------------------------------------

pub fn list_credentials(
    call: &Call,
    req: pb::ListCredentialsRequest,
) -> ServiceResult<pb::ListCredentialsResponse> {
    let only = match &req.project {
        Some(p) => Some(call.target_project(Some(p))?),
        None => None,
    };
    let visible = call.visible_projects()?;
    let creds = call
        .manager()
        .list_credentials_in(only.as_deref())
        .map_err(manager_err)?;
    let credentials = creds
        .iter()
        .filter(|cr| visible.contains(&cr.project))
        .filter(|cr| {
            let (e, es) = credential_entities(&cr.project, &cr.username);
            call.allowed("readCredential", e, es)
        })
        .map(|cr| credential_pb(&cr.into()))
        .collect();
    Ok(pb::ListCredentialsResponse {
        credentials,
        ..Default::default()
    })
}

pub fn create_credential(
    call: &Call,
    req: pb::CreateCredentialRequest,
) -> ServiceResult<pb::CreateCredentialResponse> {
    let project = call.target_project(req.project.as_deref())?;
    call.set_project(&project);
    call.set_target(format!("credential:{}", req.username));
    call.require(Ent::Project(project.clone()), project_entities(&project))?;
    let r = crate::credentials::CreateCredentialRequest {
        username: req.username,
        password: req.password,
        ssh_authorized_keys: Some(req.ssh_authorized_keys),
    };
    let cred = call
        .manager()
        .create_credential_in(&project, r)
        .map_err(manager_err)?;
    Ok(pb::CreateCredentialResponse {
        credential: credential_pb(&(&cred).into()).into(),
        ..Default::default()
    })
}

/// A credential in the requested (or default) project the caller may see.
fn visible_credential(call: &Call, project: Option<&str>, username: &str) -> ServiceResult<String> {
    let project = call.target_project(project)?;
    call.set_project(&project);
    call.set_target(format!("credential:{}", username));
    let (e, es) = credential_entities(&project, username);
    call.require_visible("readCredential", e, es, "credential")?;
    Ok(project)
}

pub fn get_credential(
    call: &Call,
    req: pb::GetCredentialRequest,
) -> ServiceResult<pb::GetCredentialResponse> {
    let project = visible_credential(call, req.project.as_deref(), &req.username)?;
    let cred = call
        .manager()
        .get_credential_in(&project, &req.username)
        .map_err(manager_err)?;
    Ok(pb::GetCredentialResponse {
        credential: credential_pb(&(&cred).into()).into(),
        ..Default::default()
    })
}

pub fn update_credential(
    call: &Call,
    req: pb::UpdateCredentialRequest,
) -> ServiceResult<pb::UpdateCredentialResponse> {
    let project = visible_credential(call, req.project.as_deref(), &req.username)?;
    let paths: Vec<String> = req
        .update_mask
        .as_option()
        .map(|m| m.paths.clone())
        .unwrap_or_default();
    let mut r = crate::credentials::UpdateCredentialRequest {
        password: None,
        ssh_authorized_keys: None,
    };
    for p in &paths {
        match p.as_str() {
            "password" => r.password = req.password.clone(),
            "ssh_authorized_keys" => r.ssh_authorized_keys = Some(req.ssh_authorized_keys.clone()),
            other => {
                return Err(ServiceError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    format!("can't update {other}: password or ssh_authorized_keys"),
                ));
            }
        }
    }
    let cred = call
        .manager()
        .update_credential_in(&project, &req.username, r)
        .map_err(manager_err)?;
    Ok(pb::UpdateCredentialResponse {
        credential: credential_pb(&(&cred).into()).into(),
        ..Default::default()
    })
}

pub async fn delete_credential(
    call: &Call,
    req: pb::DeleteCredentialRequest,
) -> ServiceResult<pb::DeleteCredentialResponse> {
    let project = visible_credential(call, req.project.as_deref(), &req.username)?;
    call.manager()
        .delete_credential_in(&project, &req.username)
        .await
        .map_err(manager_err)?;
    Ok(pb::DeleteCredentialResponse::default())
}

// ---- images ---------------------------------------------------------------

pub fn image_catalog(
    call: &Call,
    req: pb::GetImageCatalogRequest,
) -> ServiceResult<pb::GetImageCatalogResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let items = call
        .manager()
        .image_catalog(arch_of(req.arch.as_known().unwrap_or_default()))
        .into_iter()
        .map(|i| pb::CatalogItem {
            key: i.key,
            distro: i.distro,
            release: i.release,
            arch: arch_pb(i.arch).into(),
            url: i.url,
            downloaded_image_id: i.downloaded_image_id,
            ..Default::default()
        })
        .collect();
    Ok(pb::GetImageCatalogResponse {
        items,
        ..Default::default()
    })
}

pub fn firmware_catalog(
    call: &Call,
    req: pb::GetFirmwareCatalogRequest,
) -> ServiceResult<pb::GetFirmwareCatalogResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let items = call
        .manager()
        .firmware_catalog(arch_of(req.arch.as_known().unwrap_or_default()))
        .into_iter()
        .map(|i| pb::FirmwareCatalogItem {
            key: i.key,
            name: i.name,
            hypervisor: hypervisor_pb(i.hypervisor).into(),
            arch: arch_pb(i.arch).into(),
            source: i.source.into(),
            url: i.url,
            version: i.version,
            available: Some(i.available),
            hint: i.hint,
            downloaded_image_id: i.downloaded_image_id,
            ..Default::default()
        })
        .collect();
    Ok(pb::GetFirmwareCatalogResponse {
        items,
        ..Default::default()
    })
}

pub async fn list_images(
    call: &Call,
    _req: pb::ListImagesRequest,
) -> ServiceResult<pb::ListImagesResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    Ok(pb::ListImagesResponse {
        images: call
            .manager()
            .list_images()
            .await
            .iter()
            .map(image_pb)
            .collect(),
        ..Default::default()
    })
}

fn require_image(call: &Call, id: &str) -> ServiceResult<()> {
    let mut es = EntitySet::new();
    es.image(id);
    call.set_target(format!("image:{}", id));
    call.require(Ent::Image(id.into()), es)?;
    Ok(())
}

pub async fn get_image(
    call: &Call,
    req: pb::GetImageRequest,
) -> ServiceResult<pb::GetImageResponse> {
    require_image(call, &req.id)?;
    let i = call
        .manager()
        .get_image(&req.id)
        .await
        .map_err(manager_err)?;
    Ok(pb::GetImageResponse {
        image: image_pb(&i).into(),
        ..Default::default()
    })
}

pub async fn pull_image(
    call: &Call,
    req: pb::PullImageRequest,
) -> ServiceResult<pb::PullImageResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    // A kernel copied from host files (spec/23-kernel-boot.md §4.2).
    if req.kernel_path.is_some() || req.initrd_path.is_some() {
        call.require_action("useHostPath", Ent::Host, EntitySet::new(), &[])?;
    }
    let kind = match req.kind.as_known().unwrap_or_default() {
        pb::ImageKind::IMAGE_KIND_DISK => Some(img::ImageKind::Disk),
        pb::ImageKind::IMAGE_KIND_FIRMWARE => Some(img::ImageKind::Firmware),
        pb::ImageKind::IMAGE_KIND_KERNEL => Some(img::ImageKind::Kernel),
        pb::ImageKind::IMAGE_KIND_UNSPECIFIED => None,
    };
    let r = img::PullImageRequest {
        catalog: req.catalog,
        url: req.url,
        sha256: req.sha256,
        name: req.name,
        firmware: req.firmware,
        kind,
        hypervisor: hypervisor_of(req.hypervisor.as_known().unwrap_or_default()),
        vars_url: req.vars_url,
        vars_sha256: req.vars_sha256,
        arch: arch_of(req.arch.as_known().unwrap_or_default()).map(|a| a.as_str().to_string()),
        nodes: req.nodes,
        keep_server_copy: req.keep_server_copy,
        initrd_url: req.initrd_url,
        initrd_sha256: req.initrd_sha256,
        cmdline: req.cmdline,
        kernel_path: req.kernel_path.map(crate::models::expand_tilde),
        initrd_path: req.initrd_path.map(crate::models::expand_tilde),
        kernel_from: req.kernel_from,
        with_kernel: req.with_kernel,
    };
    let (i, created) = call.manager().pull_image(r).await.map_err(manager_err)?;
    call.set_target(format!("image:{}", i.id));
    Ok(pb::PullImageResponse {
        image: image_pb(&i).into(),
        created: Some(created),
        ..Default::default()
    })
}

/// `None` once the image is gone; with `wait_secs`, waits for that.
pub async fn delete_image(
    call: &Call,
    req: pb::DeleteImageRequest,
) -> ServiceResult<pb::DeleteImageResponse> {
    require_image(call, &req.id)?;
    let m = call.manager();
    let Some(still) = m.delete_image(&req.id).await.map_err(manager_err)? else {
        return Ok(pb::DeleteImageResponse::default());
    };
    let still = match req.wait_secs {
        None => Some(still),
        Some(secs) => {
            let id = still.id.clone();
            m.wait_gone(
                std::time::Duration::from_secs(u64::from(secs).min(300)),
                || {
                    m.images
                        .get_image(&id)
                        .ok()
                        .map(|i| m.images.image_response(&i, false))
                },
            )
            .await
        }
    };
    Ok(pb::DeleteImageResponse {
        image: still.as_ref().map(image_pb).into(),
        ..Default::default()
    })
}

pub fn retry_image(
    call: &Call,
    req: pb::RetryImageRequest,
) -> ServiceResult<pb::RetryImageResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    call.set_target(format!("image:{}", req.id));
    let i = call.manager().retry_image(&req.id).map_err(manager_err)?;
    Ok(pb::RetryImageResponse {
        image: image_pb(&i).into(),
        ..Default::default()
    })
}

pub async fn add_image_copy(
    call: &Call,
    req: pb::AddImageCopyRequest,
) -> ServiceResult<pb::AddImageCopyResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    call.set_target(format!("image:{}", req.id));
    let i = call
        .manager()
        .add_image_copy(&req.id, &req.node)
        .await
        .map_err(manager_err)?;
    Ok(pb::AddImageCopyResponse {
        image: image_pb(&i).into(),
        ..Default::default()
    })
}

pub async fn remove_image_copy(
    call: &Call,
    req: pb::RemoveImageCopyRequest,
) -> ServiceResult<pb::RemoveImageCopyResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    call.set_target(format!("image:{}", req.id));
    let i = call
        .manager()
        .remove_image_copy(&req.id, &req.node)
        .await
        .map_err(manager_err)?;
    Ok(pb::RemoveImageCopyResponse {
        image: image_pb(&i).into(),
        ..Default::default()
    })
}

pub async fn image_events(
    call: &Call,
    req: pb::ListImageEventsRequest,
) -> ServiceResult<pb::ListImageEventsResponse> {
    require_image(call, &req.id)?;
    let i = call
        .manager()
        .get_image(&req.id)
        .await
        .map_err(manager_err)?;
    let events = call
        .manager()
        .object_events("image", &i.id)
        .map_err(manager_err)?;
    Ok(pb::ListImageEventsResponse {
        events: events.iter().map(event_pb).collect(),
        ..Default::default()
    })
}

// ---- disks ----------------------------------------------------------------

pub fn list_disks(call: &Call, req: pb::ListDisksRequest) -> ServiceResult<pb::ListDisksResponse> {
    let only = match &req.project {
        Some(p) => Some(call.target_project(Some(p))?),
        None => None,
    };
    let visible = call.visible_projects()?;
    let disks = call
        .manager()
        .list_disks()
        .into_iter()
        .filter(|d| only.as_ref().is_none_or(|p| *p == d.project) && visible.contains(&d.project))
        .filter(|d| {
            let (e, es) = disk_entities(&d.id, &d.project);
            call.allowed("readDisk", e, es)
        })
        .map(|d| disk_pb(&d))
        .collect();
    Ok(pb::ListDisksResponse {
        disks,
        ..Default::default()
    })
}

/// Disk `key` (an id, or a name in the tenants of the projects the caller
/// sees, spec/19-tenancy.md D24) if the caller may see it, else not found.
async fn visible_disk(call: &Call, key: &str) -> ServiceResult<DiskResponse> {
    let not_found = || ServiceError::new(StatusCode::NOT_FOUND, "not_found", "disk not found");
    let visible = call.visible_projects()?;
    let d = call
        .manager()
        .images
        .get_disk_in(key, |p| visible.contains(p))
        .map_err(|e| match e {
            img::ImageError::Ambiguous(m) => {
                ServiceError::new(StatusCode::CONFLICT, "ambiguous", m)
            }
            _ => not_found(),
        })?;
    let d = call
        .manager()
        .get_disk(&d.id)
        .await
        .map_err(|_| not_found())?;
    call.set_project(&d.project);
    call.set_target(format!("disk:{}", d.id));
    let (e, es) = disk_entities(&d.id, &d.project);
    call.require_visible("readDisk", e, es, "disk")?;
    Ok(d)
}

pub async fn get_disk(call: &Call, req: pb::GetDiskRequest) -> ServiceResult<pb::GetDiskResponse> {
    Ok(pb::GetDiskResponse {
        disk: disk_pb(&visible_disk(call, &req.id).await?).into(),
        ..Default::default()
    })
}

/// Where a disk write left the disk (spec/12-reconciliation.md §12.3).
pub enum DiskReply {
    /// Not waited for: the disk as it is now.
    Now(DiskResponse),
    /// Waited for, and it settled.
    Settled(DiskResponse),
    /// Waited for, and the time ran out.
    Pending(DiskResponse),
    Gone,
}

impl DiskReply {
    /// The disk and whether it settled, for an RPC answer.
    pub fn parts(&self) -> (Option<pb::Disk>, Option<bool>) {
        match self {
            DiskReply::Now(d) => (Some(disk_pb(d)), None),
            DiskReply::Settled(d) => (Some(disk_pb(d)), Some(true)),
            DiskReply::Pending(d) => (Some(disk_pb(d)), Some(false)),
            DiskReply::Gone => (None, Some(true)),
        }
    }
}

/// The answer to a disk write: at once, or with `wait` once the disk
/// settled, failed (the error the call would have returned synchronously),
/// or the time ran out.
async fn disk_reply(call: &Call, id: &str, wait: Option<u32>) -> ServiceResult<DiskReply> {
    let m = call.manager();
    let Some(secs) = wait else {
        return Ok(match m.get_disk(id).await {
            Ok(d) => DiskReply::Now(d),
            Err(_) => DiskReply::Gone,
        });
    };
    match m
        .wait_disk(id, std::time::Duration::from_secs(u64::from(secs).min(300)))
        .await
    {
        (_, None) => Ok(DiskReply::Gone),
        (false, Some(d)) => Ok(DiskReply::Pending(d)),
        (true, Some(d)) => {
            let ready = d.conditions.iter().find(|c| c.kind == "Ready").cloned();
            match ready.as_ref().map(|c| c.reason.as_str()) {
                Some("InvalidDisk") | Some("ResizeInvalid") => Err(ServiceError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_disk",
                    ready.map(|c| c.message).unwrap_or_default(),
                )),
                _ if d.phase == img::DiskPhase::Failed || d.phase == img::DiskPhase::Missing => {
                    Err(ServiceError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "image_error",
                        ready.map(|c| c.message).unwrap_or_default(),
                    ))
                }
                _ => Ok(DiskReply::Settled(d)),
            }
        }
    }
}

pub async fn create_disk(call: &Call, req: pb::CreateDiskRequest) -> ServiceResult<DiskReply> {
    let project = call.target_project(req.project.as_deref())?;
    call.set_project(&project);
    call.require(Ent::Project(project.clone()), project_entities(&project))?;
    call.require_admitted(&project)?;
    if let Some(image) = &req.image {
        let mut es = EntitySet::new();
        es.image(image);
        call.require_action("readImage", Ent::Image(image.clone()), es, &[])?;
    }
    let datastore = disk_datastore(call, req.datastore.as_deref())?;
    let r = img::CreateDiskRequest {
        name: req.name,
        project: Some(project.clone()),
        size_gib: req.size_gib,
        size_bytes: req.size_bytes,
        image: req.image,
        clone: clone_of(req.clone.as_known().unwrap_or_default()),
        format: format_of(req.format.as_known().unwrap_or_default()),
        extend_root: req.extend_root,
        node: req.node,
        datastore: Some(datastore),
    };
    let quota = call.quota_mode(&project);
    let (disk, over) = call
        .manager()
        .create_disk_in(&project, r, quota)
        .await
        .map_err(manager_err)?;
    call.note_overruns(&over);
    call.set_target(format!("disk:{}", disk.id));
    disk_reply(call, &disk.id, req.wait_secs).await
}

pub async fn resize_disk(call: &Call, req: pb::ResizeDiskRequest) -> ServiceResult<DiskReply> {
    let d = visible_disk(call, &req.id).await?;
    let quota = call.quota_mode(&d.project);
    let r = img::ResizeDiskRequest {
        size_gib: req.size_gib,
        size_bytes: req.size_bytes,
        extend_root: req.extend_root,
    };
    let (disk, over) = call
        .manager()
        .resize_disk_with(&d.id, r, quota)
        .await
        .map_err(manager_err)?;
    call.note_overruns(&over);
    disk_reply(call, &disk.id, req.wait_secs).await
}

/// `POST /disks/{id}/move` (spec/24-storage.md §8.3); the destination must be
/// one the caller may put disks on.
pub async fn move_disk(call: &Call, req: pb::MoveDiskRequest) -> ServiceResult<DiskReply> {
    let d = visible_disk(call, &req.id).await?;
    let datastore = disk_datastore(call, Some(&req.datastore))?;
    call.manager()
        .move_disk(
            &d.id,
            &datastore,
            req.node.as_deref(),
            req.flatten.unwrap_or(false),
        )
        .await
        .map_err(manager_err)?;
    disk_reply(call, &d.id, req.wait_secs).await
}

pub async fn extend_root(call: &Call, req: pb::ExtendRootRequest) -> ServiceResult<DiskReply> {
    let d = visible_disk(call, &req.id).await?;
    let mode = match req.mode.as_known().unwrap_or_default() {
        pb::ExtendMode::EXTEND_MODE_ON_BOOT => img::ExtendMode::OnBoot,
        _ => img::ExtendMode::Offline,
    };
    let disk = call
        .manager()
        .extend_root(&d.id, mode)
        .await
        .map_err(manager_err)?;
    disk_reply(call, &disk.id, req.wait_secs).await
}

/// `None` once the disk is gone (normally at once); the disk while an
/// operation on it finishes first.
pub async fn delete_disk(
    call: &Call,
    req: pb::DeleteDiskRequest,
) -> ServiceResult<Option<DiskResponse>> {
    let d = visible_disk(call, &req.id).await?;
    call.manager()
        .delete_disk(&d.id)
        .await
        .map_err(manager_err)?;
    Ok(call.manager().get_disk(&d.id).await.ok())
}

pub async fn disk_events(
    call: &Call,
    req: pb::ListDiskEventsRequest,
) -> ServiceResult<pb::ListDiskEventsResponse> {
    let d = visible_disk(call, &req.id).await?;
    let events = call
        .manager()
        .object_events("disk", &d.id)
        .map_err(manager_err)?;
    Ok(pb::ListDiskEventsResponse {
        events: events.iter().map(event_pb).collect(),
        ..Default::default()
    })
}

// ---- datastores (spec/24-storage.md §13) -------------------------------------

/// The datastore a new disk goes to (the one asked for, else the default
/// for disks), which the caller must be allowed to use. Its id.
pub fn disk_datastore(call: &Call, key: Option<&str>) -> ServiceResult<String> {
    use crate::datastore::record;
    let db = call.manager().database();
    let ds = match key {
        Some(k) => record::find(&db, k)
            .map_err(|e| manager_err(e.into()))?
            .ok_or_else(|| {
                ServiceError::new(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("no datastore {k}"),
                )
            })?,
        None => record::default_for_disks(&db).map_err(|e| manager_err(e.into()))?,
    };
    let mut es = EntitySet::new();
    es.datastore(&ds.id);
    call.require_action("useDatastore", Ent::Datastore(ds.id.clone()), es, &[])?;
    Ok(ds.id)
}

pub fn datastore_pb(v: &crate::datastore::manage::DatastoreView) -> pb::Datastore {
    use crate::datastore::record::DriverSpec;
    let d = &v.datastore;
    let (path, node_paths) = d.driver.paths();
    let (driver, topology) = match d.driver {
        DriverSpec::Dir { .. } => (
            pb::DatastoreDriver::DATASTORE_DRIVER_DIR,
            pb::DatastoreTopology::DATASTORE_TOPOLOGY_NODE_LOCAL,
        ),
        DriverSpec::SharedDir { .. } => (
            pb::DatastoreDriver::DATASTORE_DRIVER_SHARED_DIR,
            pb::DatastoreTopology::DATASTORE_TOPOLOGY_SHARED,
        ),
        DriverSpec::Lvm { .. } => (
            pb::DatastoreDriver::DATASTORE_DRIVER_LVM,
            pb::DatastoreTopology::DATASTORE_TOPOLOGY_NODE_LOCAL,
        ),
        DriverSpec::Hyperstor { .. } => (
            pb::DatastoreDriver::DATASTORE_DRIVER_HYPERSTOR,
            pb::DatastoreTopology::DATASTORE_TOPOLOGY_SHARED,
        ),
    };
    let (hyperstor_api, hyperstor_pool, hyperstor_identity, expires) = match &d.driver {
        DriverSpec::Hyperstor {
            api,
            pool,
            identity,
            ..
        } => (
            api.clone(),
            *pool,
            identity
                .as_ref()
                .map(|i| i.name.clone())
                .unwrap_or_default(),
            identity.as_ref().map(|i| i.not_after),
        ),
        _ => (Vec::new(), 0, String::new(), None),
    };
    let (path, volume_group, thin_pool) = match &d.driver {
        DriverSpec::Lvm { vg, thin_pool, .. } => (String::new(), vg.clone(), thin_pool.clone()),
        _ => (path.clone(), String::new(), String::new()),
    };
    pb::Datastore {
        id: d.id.clone(),
        name: d.name.clone(),
        driver: driver.into(),
        topology: topology.into(),
        path,
        volume_group,
        thin_pool,
        node_paths: node_paths.clone().into_iter().collect(),
        nodes: d.nodes.clone(),
        default_for_disks: v.is_default,
        overcommit: d.overcommit,
        builtin: d.is_builtin(),
        created_at: crate::service::common::ts_opt(d.created_at).into(),
        node_status: v
            .rows
            .iter()
            .map(|(n, r)| pb::DatastoreNode {
                node: n.clone(),
                node_name: v.node_names.get(n).cloned(),
                reachable: r.reachable,
                reason: r.reason.clone(),
                message: r.message.clone(),
                path: r.path.clone(),
                total_bytes: r.total_bytes,
                free_bytes: r.free_bytes,
                checked_at: crate::service::common::ts_opt(r.checked_at).into(),
                warnings: r.warnings.clone(),
                image_volumes: r.image_volumes.clone(),
                ..Default::default()
            })
            .collect(),
        disks: v.disks,
        hyperstor_api,
        hyperstor_pool,
        hyperstor_identity,
        hyperstor_identity_expires: crate::service::common::ts_opt(expires.unwrap_or(0)).into(),
        ..Default::default()
    }
}

fn require_datastore(call: &Call, id: &str) -> ServiceResult<()> {
    let mut es = EntitySet::new();
    es.datastore(id);
    call.set_target(format!("datastore:{id}"));
    call.require(Ent::Datastore(id.into()), es)?;
    Ok(())
}

pub fn list_datastores(
    call: &Call,
    _req: pb::ListDatastoresRequest,
) -> ServiceResult<pb::ListDatastoresResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    Ok(pb::ListDatastoresResponse {
        datastores: call
            .manager()
            .list_datastores()
            .map_err(manager_err)?
            .iter()
            .map(datastore_pb)
            .collect(),
        ..Default::default()
    })
}

pub fn get_datastore(
    call: &Call,
    req: pb::GetDatastoreRequest,
) -> ServiceResult<pb::GetDatastoreResponse> {
    let v = call.manager().get_datastore(&req.id).map_err(manager_err)?;
    require_datastore(call, &v.datastore.id)?;
    Ok(pb::GetDatastoreResponse {
        datastore: datastore_pb(&v).into(),
        ..Default::default()
    })
}

pub async fn create_datastore(
    call: &Call,
    req: pb::CreateDatastoreRequest,
) -> ServiceResult<pb::CreateDatastoreResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let invalid = |m: &str| ServiceError::new(StatusCode::BAD_REQUEST, "invalid_datastore", m);
    let mut hyperstor = None;
    let (shared, lvm) = match req.driver.as_known() {
        Some(pb::DatastoreDriver::DATASTORE_DRIVER_DIR)
        | Some(pb::DatastoreDriver::DATASTORE_DRIVER_UNSPECIFIED) => (false, None),
        Some(pb::DatastoreDriver::DATASTORE_DRIVER_SHARED_DIR) => (true, None),
        Some(pb::DatastoreDriver::DATASTORE_DRIVER_HYPERSTOR) => {
            if req.hyperstor_api.is_empty()
                || req.hyperstor_token.is_empty()
                || req.hyperstor_pool == 0
                || !req.path.is_empty()
            {
                return Err(invalid(
                    "a hyperstor datastore takes hyperstor_api, hyperstor_pool and hyperstor_token, not path",
                ));
            }
            // hyperstor's volumes are thin and it doesn't report free space
            // yet, so glidex can't keep disks within it (S22).
            if req.overcommit == Some(false) {
                return Err(invalid(
                    "a hyperstor datastore can't refuse to overcommit: hyperstor doesn't report its free space yet",
                ));
            }
            hyperstor = Some(crate::datastore::manage::NewHyperstor {
                api: req.hyperstor_api.clone(),
                pool: req.hyperstor_pool,
                token: zeroize::Zeroizing::new(req.hyperstor_token.clone()),
            });
            (true, None)
        }
        Some(pb::DatastoreDriver::DATASTORE_DRIVER_LVM) => {
            if req.volume_group.is_empty() || req.thin_pool.is_empty() || !req.path.is_empty() {
                return Err(invalid(
                    "an lvm datastore takes volume_group and thin_pool, not path",
                ));
            }
            (
                false,
                Some((req.volume_group.clone(), req.thin_pool.clone())),
            )
        }
        None => return Err(invalid("driver: dir, shared-dir, lvm or hyperstor")),
    };
    let v = call
        .manager()
        .create_datastore(crate::datastore::manage::NewDatastore {
            name: req.name,
            path: req.path,
            node_paths: req.node_paths.into_iter().collect(),
            nodes: req.nodes,
            default_for_disks: req.default_for_disks.unwrap_or(false),
            overcommit: req.overcommit,
            shared,
            lvm,
            hyperstor,
        })
        .await
        .map_err(manager_err)?;
    call.set_target(format!("datastore:{}", v.datastore.id));
    Ok(pb::CreateDatastoreResponse {
        datastore: datastore_pb(&v).into(),
        ..Default::default()
    })
}

pub async fn update_datastore(
    call: &Call,
    req: pb::UpdateDatastoreRequest,
) -> ServiceResult<pb::UpdateDatastoreResponse> {
    let id = call
        .manager()
        .get_datastore(&req.id)
        .map_err(manager_err)?
        .datastore
        .id;
    require_datastore(call, &id)?;
    let paths: Vec<String> = req
        .update_mask
        .as_option()
        .map(|m| m.paths.clone())
        .unwrap_or_default();
    let mut ch = crate::datastore::manage::DatastoreChanges::default();
    for p in &paths {
        match p.as_str() {
            "node_paths" => ch.node_paths = Some(req.node_paths.clone().into_iter().collect()),
            "nodes" => ch.nodes = Some(req.nodes.clone()),
            "default_for_disks" => {
                ch.default_for_disks = Some(req.default_for_disks.unwrap_or(false))
            }
            "overcommit" => ch.overcommit = Some(req.overcommit.unwrap_or(true)),
            other => {
                return Err(ServiceError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    format!(
                        "can't update {other}: node_paths, nodes, default_for_disks or overcommit"
                    ),
                ));
            }
        }
    }
    let v = call
        .manager()
        .update_datastore(&id, ch)
        .await
        .map_err(manager_err)?;
    Ok(pb::UpdateDatastoreResponse {
        datastore: datastore_pb(&v).into(),
        ..Default::default()
    })
}

pub async fn delete_datastore(
    call: &Call,
    req: pb::DeleteDatastoreRequest,
) -> ServiceResult<pb::DeleteDatastoreResponse> {
    let id = call
        .manager()
        .get_datastore(&req.id)
        .map_err(manager_err)?
        .datastore
        .id;
    require_datastore(call, &id)?;
    call.manager()
        .delete_datastore(&id)
        .await
        .map_err(manager_err)?;
    Ok(pb::DeleteDatastoreResponse::default())
}

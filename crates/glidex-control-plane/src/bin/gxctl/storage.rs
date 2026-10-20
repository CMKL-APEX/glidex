//! Guest credentials, images and disks (spec/images.md, spec/credentials.md)
//! on the generated `glidex.storage.v1` clients, over the Connect protocol
//! (spec/grpc.md §9.1). Passwords come from hidden prompts and are never
//! shown.

use crate::client::{ApiClient, RpcTransport};
use crate::tenancy::{err, rpc_err, usage};
use crate::{
    chosen_key_files, flag_value, flag_values, format_bytes, format_unix_time, has_flag,
    own_public_key_files, prompt, prompt_new_password, prompt_path, read_public_key_files,
};
use buffa::EnumValue;
use colored::Colorize;
use glidex_api::proto::glidex::common::v1::{Condition, ConditionStatus, Hypervisor};
use glidex_api::proto::glidex::storage::v1::*;
use std::io::{self, Write};
use tabled::{Table, Tabled};

type Source = glidex_api::proto::glidex::storage::v1::__buffa::oneof::image_source::Source;
type Origin = glidex_api::proto::glidex::storage::v1::__buffa::oneof::disk_origin::Origin;

async fn rpc(client: &ApiClient) -> Result<RpcTransport, String> {
    client.rpc_transport().await.map_err(|e| e.message)
}

// ---- guest credentials --------------------------------------------------------

/// The credentials of the selected project (all the caller may see when
/// none is selected).
pub(crate) async fn list_credentials(client: &ApiClient) -> Result<Vec<Credential>, String> {
    let t = rpc(client).await?;
    CredentialServiceClient::new(t.clone(), t.config())
        .list_credentials(ListCredentialsRequest {
            project: client.project(),
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().credentials)
        .map_err(rpc_err)
}

pub(crate) async fn create_credential(
    client: &ApiClient,
    username: String,
    password: Option<String>,
    ssh_authorized_keys: Vec<String>,
) -> Result<Credential, String> {
    let t = rpc(client).await?;
    CredentialServiceClient::new(t.clone(), t.config())
        .create_credential(CreateCredentialRequest {
            project: client.project(),
            username,
            password,
            ssh_authorized_keys,
            ..Default::default()
        })
        .await
        .map(|r| {
            r.into_owned()
                .credential
                .as_option()
                .cloned()
                .unwrap_or_default()
        })
        .map_err(rpc_err)
}

/// Change the password and/or replace the SSH keys (an empty list clears
/// them); only what is given changes.
pub(crate) async fn update_credential(
    client: &ApiClient,
    username: &str,
    password: Option<String>,
    ssh_authorized_keys: Option<Vec<String>>,
) -> Result<Credential, String> {
    let mut paths = Vec::new();
    if password.is_some() {
        paths.push("password".to_string());
    }
    if ssh_authorized_keys.is_some() {
        paths.push("ssh_authorized_keys".to_string());
    }
    let mask = buffa_types::google::protobuf::FieldMask {
        paths,
        ..Default::default()
    };
    let t = rpc(client).await?;
    CredentialServiceClient::new(t.clone(), t.config())
        .update_credential(UpdateCredentialRequest {
            username: username.to_string(),
            project: client.project(),
            password,
            ssh_authorized_keys: ssh_authorized_keys.unwrap_or_default(),
            update_mask: mask.into(),
            ..Default::default()
        })
        .await
        .map(|r| {
            r.into_owned()
                .credential
                .as_option()
                .cloned()
                .unwrap_or_default()
        })
        .map_err(rpc_err)
}

pub(crate) async fn delete_credential(client: &ApiClient, username: &str) -> Result<(), String> {
    let t = rpc(client).await?;
    CredentialServiceClient::new(t.clone(), t.config())
        .delete_credential(DeleteCredentialRequest {
            username: username.to_string(),
            project: client.project(),
            ..Default::default()
        })
        .await
        .map(|_| ())
        .map_err(rpc_err)
}

#[derive(Tabled)]
struct CredentialRow {
    username: String,
    password: &'static str,
    ssh_keys: usize,
    updated: String,
}

impl From<&Credential> for CredentialRow {
    fn from(c: &Credential) -> Self {
        Self {
            username: c.username.clone(),
            password: if c.has_password == Some(true) {
                "set"
            } else {
                "-"
            },
            ssh_keys: c.ssh_authorized_keys.len(),
            updated: format_unix_time(
                c.updated_at
                    .as_option()
                    .map(|t| t.seconds.max(0) as u64)
                    .unwrap_or(0),
            ),
        }
    }
}

/// `credentials`
pub async fn credentials(client: &ApiClient) {
    match list_credentials(client).await {
        Ok(creds) if creds.is_empty() => println!("No credentials stored."),
        Ok(creds) => {
            let rows: Vec<CredentialRow> = creds.iter().map(CredentialRow::from).collect();
            println!("{}", Table::new(rows));
        }
        Err(e) => err(e),
    }
}

/// `credential-add` (interactive).
pub async fn credential_add(client: &ApiClient) {
    let me = std::env::var("USER").unwrap_or_default();
    let username = if me.is_empty() {
        prompt("Username: ")
    } else {
        let u = prompt(&format!("Username [{}]: ", me));
        if u.is_empty() { me } else { u }
    };
    if username.is_empty() {
        println!("{}", "Error: username is required".red());
        return;
    }
    println!(
        "{}",
        "Leave the password empty for SSH-key-only login.".dimmed()
    );
    let password = match prompt_new_password(true) {
        Ok(p) => p,
        Err(e) => return err(e),
    };
    // Offer your own ~/.ssh/*.pub by default.
    let defaults = own_public_key_files();
    let question = if defaults.is_empty() {
        "SSH public key files (optional, comma-separated, e.g. ~/.ssh/id_ed25519.pub): ".to_string()
    } else {
        format!(
            "SSH public key files (comma-separated; 'none' for no keys) [{}]: ",
            defaults.join(", ")
        )
    };
    let keys = match chosen_key_files(&prompt_path(&question), &defaults)
        .map(|s| read_public_key_files(&s))
        .transpose()
    {
        Ok(k) => k.unwrap_or_default(),
        Err(e) => return err(e),
    };
    match create_credential(client, username, password, keys).await {
        Ok(c) => println!(
            "{} {} (password: {}, SSH keys: {})",
            "Credential created:".green(),
            c.username.yellow(),
            if c.has_password == Some(true) {
                "set"
            } else {
                "none"
            },
            c.ssh_authorized_keys.len()
        ),
        Err(e) => err(e),
    }
}

async fn credential_update(
    client: &ApiClient,
    username: &str,
    password: Option<String>,
    keys: Option<Vec<String>>,
) {
    match update_credential(client, username, password, keys).await {
        Ok(c) => {
            println!("{} {}", "Credential updated:".green(), c.username.yellow());
            println!(
                "{} Changes apply to VMs on their first boot only; running or already-provisioned VMs keep their current login.",
                "Note:".dimmed()
            );
        }
        Err(e) => err(e),
    }
}

/// `credential-passwd <user>`
pub async fn credential_passwd(client: &ApiClient, username: Option<&str>) {
    let Some(username) = username else {
        return usage("Usage: credential-passwd <user>");
    };
    match prompt_new_password(false) {
        Ok(password) => credential_update(client, username, password, None).await,
        Err(e) => err(e),
    }
}

/// `credential-keys <user>`
pub async fn credential_keys(client: &ApiClient, username: Option<&str>) {
    let Some(username) = username else {
        return usage("Usage: credential-keys <user>");
    };
    let input = prompt_path("SSH public key files (comma-separated; empty to remove all keys): ");
    match read_public_key_files(&input) {
        Ok(keys) => credential_update(client, username, None, Some(keys)).await,
        Err(e) => err(e),
    }
}

/// `credential-rm <user>`
pub async fn credential_rm(client: &ApiClient, username: Option<&str>) {
    let Some(username) = username else {
        return usage("Usage: credential-rm <user>");
    };
    match delete_credential(client, username).await {
        Ok(()) => println!("{} {}", "Credential deleted:".green(), username),
        Err(e) => err(e),
    }
}

// ---- images -------------------------------------------------------------------

/// All images in the library.
pub(crate) async fn list_images(client: &ApiClient) -> Result<Vec<Image>, String> {
    let t = rpc(client).await?;
    ImageServiceClient::new(t.clone(), t.config())
        .list_images(ListImagesRequest::default())
        .await
        .map(|r| r.into_owned().images)
        .map_err(rpc_err)
}

/// `cloudhypervisor` / `qemu`, as gxctl names hypervisors.
pub(crate) fn hypervisor_name(h: &EnumValue<Hypervisor>) -> &'static str {
    match h.as_known() {
        Some(Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR) => "cloudhypervisor",
        Some(Hypervisor::HYPERVISOR_QEMU) => "qemu",
        _ => "",
    }
}

fn parse_hypervisor(s: &str) -> Option<Hypervisor> {
    match s {
        "cloudhypervisor" | "cloud-hypervisor" | "ch" => {
            Some(Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR)
        }
        "qemu" => Some(Hypervisor::HYPERVISOR_QEMU),
        _ => None,
    }
}

pub(crate) fn is_firmware(i: &Image) -> bool {
    i.kind.as_known() == Some(ImageKind::IMAGE_KIND_FIRMWARE)
}

pub(crate) fn is_kernel(i: &Image) -> bool {
    i.kind.as_known() == Some(ImageKind::IMAGE_KIND_KERNEL)
}

pub(crate) fn is_ready(i: &Image) -> bool {
    image_state(i) == Some(ImageState::IMAGE_STATE_READY)
}

fn image_state(i: &Image) -> Option<ImageState> {
    i.status.as_option().and_then(|s| s.state.as_known())
}

fn image_status(img: &Image) -> String {
    let st = img.status.as_option().cloned().unwrap_or_default();
    match st.state.as_known() {
        Some(ImageState::IMAGE_STATE_DOWNLOADING) => {
            let got = st.received_bytes;
            match st.total_bytes {
                Some(t) if t > 0 => format!("downloading {:.0}%", got as f64 * 100.0 / t as f64),
                _ => format!("downloading {}", format_bytes(got)),
            }
        }
        Some(ImageState::IMAGE_STATE_FAILED) => format!("failed: {}", st.reason).red().to_string(),
        Some(ImageState::IMAGE_STATE_READY) => "ready".green().to_string(),
        Some(ImageState::IMAGE_STATE_VERIFYING) => "verifying".into(),
        Some(ImageState::IMAGE_STATE_MISSING) => "missing".into(),
        Some(ImageState::IMAGE_STATE_EXTRACTING) => "extracting".into(),
        _ => "?".into(),
    }
}

/// `image …`
pub async fn image(client: &ApiClient, args: &[&str]) {
    let u = "Usage: image catalog | list | pull <catalog-key|url> [--name N] [--sha256 H] [--no-kernel] [--firmware-for <cloudhypervisor|qemu> [--vars-url U] [--vars-sha256 H]] | pull <url> --kernel [--sha256 H] [--initrd-url U [--initrd-sha256 H]] [--cmdline S] | pull --kernel-path P [--initrd-path P] [--cmdline S] | pull --firmware <key> [--name N] | extract-kernel <image> [--name N] | retry <name|id> | rm <name|id>";
    let t = match rpc(client).await {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let svc = ImageServiceClient::new(t.clone(), t.config());
    match args.first().copied().unwrap_or("list") {
        "catalog" => match svc
            .get_image_catalog(GetImageCatalogRequest::default())
            .await
            .map(|r| r.into_owned())
        {
            Ok(r) => {
                println!("{}", "Cloud images (image pull <key>):".bold());
                for i in &r.items {
                    println!(
                        "  {:<14} {} {}{}",
                        i.key.cyan(),
                        i.distro,
                        i.release,
                        if i.downloaded_image_id.is_none() {
                            String::new()
                        } else {
                            " (downloaded)".green().to_string()
                        }
                    );
                }
                if let Ok(fw) = svc
                    .get_firmware_catalog(GetFirmwareCatalogRequest::default())
                    .await
                    .map(|r| r.into_owned())
                {
                    println!("{}", "UEFI firmware (image pull --firmware <key>):".bold());
                    for i in &fw.items {
                        let note = if i.downloaded_image_id.is_some() {
                            " (downloaded)".green().to_string()
                        } else if let Some(h) = &i.hint {
                            format!(" ({})", h).yellow().to_string()
                        } else {
                            String::new()
                        };
                        println!(
                            "  {:<14} {} [{}] {}{}",
                            i.key.cyan(),
                            i.name,
                            hypervisor_name(&i.hypervisor),
                            i.version,
                            note
                        );
                    }
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        "list" | "ls" => match svc
            .list_images(ListImagesRequest::default())
            .await
            .map(|r| r.into_owned().images)
        {
            Ok(imgs) if imgs.is_empty() => {
                println!("No images. Pull one with 'image pull <key>' (see 'image catalog').")
            }
            Ok(imgs) => {
                for i in &imgs {
                    let from = match i.source.as_option().and_then(|s| s.source.as_ref()) {
                        Some(Source::Catalog(c)) | Some(Source::Firmware(c)) => {
                            format!("{} {}", c.key, c.version)
                        }
                        Some(Source::Url(s)) => s.url.clone(),
                        Some(Source::Host(h)) => h.path.clone(),
                        Some(Source::Extracted(x)) if x.path.is_empty() => "extracting".into(),
                        Some(Source::Extracted(x)) => format!("extracted: {}", x.path),
                        None => String::new(),
                    };
                    let kind = match hypervisor_name(&i.hypervisor) {
                        h if is_firmware(i) && !h.is_empty() => format!("firmware/{}", h),
                        _ if is_kernel(i) => {
                            match i.kernel.as_option().and_then(|k| k.release.as_deref()) {
                                Some(r) => format!("kernel {r}"),
                                None => "kernel".to_string(),
                            }
                        }
                        _ => "disk".to_string(),
                    };
                    println!(
                        "  {:<20} {:<20} {:<22} {:>9}  {}{}",
                        i.name.cyan(),
                        kind,
                        image_status(i),
                        format_bytes(i.virtual_size_bytes),
                        from.trim(),
                        if i.verified == Some(true) {
                            ""
                        } else {
                            " (unverified)"
                        }
                    );
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        "pull" | "extract-kernel" => {
            let firmware = flag_value(args, "--firmware");
            // A kernel extracted from a disk image (spec/kernel-boot.md §10).
            let kernel_from = flag_value(args, "--kernel-from").or_else(|| {
                (args[0] == "extract-kernel")
                    .then(|| args.get(1).copied().filter(|a| !a.starts_with("--")))
                    .flatten()
            });
            // A kernel copied from host files (administrators).
            let kernel_path = flag_value(args, "--kernel-path");
            let Some(what) = firmware
                .or(kernel_from)
                .or(kernel_path)
                .or_else(|| args.get(1).copied().filter(|a| !a.starts_with("--")))
            else {
                return usage(u);
            };
            let mut req = PullImageRequest::default();
            if firmware.is_some() {
                req.firmware = Some(what.into());
            } else if kernel_from.is_some() {
                req.kernel_from = Some(what.into());
            } else if kernel_path.is_some() {
                req.kernel_path = Some(what.into());
                req.initrd_path = flag_value(args, "--initrd-path").map(String::from);
            } else if what.contains("://") {
                req.url = Some(what.into());
            } else {
                req.catalog = Some(what.into());
            }
            if args.contains(&"--kernel") {
                req.kind = ImageKind::IMAGE_KIND_KERNEL.into();
            }
            if req.url.is_some() || req.kernel_path.is_some() {
                req.initrd_url = flag_value(args, "--initrd-url").map(String::from);
                req.initrd_sha256 = flag_value(args, "--initrd-sha256").map(String::from);
                req.cmdline = flag_value(args, "--cmdline").map(String::from);
            }
            if args.contains(&"--no-kernel") {
                req.with_kernel = Some(false);
            }
            // The server extracts a catalog image's kernel unless asked not to.
            let brings_kernel = req.catalog.is_some() && req.with_kernel != Some(false);
            if let Some(h) = flag_value(args, "--firmware-for") {
                let Some(h) = parse_hypervisor(h) else {
                    return err(format!(
                        "unknown hypervisor '{}' (cloudhypervisor or qemu)",
                        h
                    ));
                };
                req.kind = ImageKind::IMAGE_KIND_FIRMWARE.into();
                req.hypervisor = h.into();
            }
            // A split OVMF build's variable-store template (QEMU).
            req.vars_url = flag_value(args, "--vars-url").map(String::from);
            req.vars_sha256 = flag_value(args, "--vars-sha256").map(String::from);
            req.name = flag_value(args, "--name").map(String::from);
            req.sha256 = flag_value(args, "--sha256").map(String::from);
            let img = match svc.pull_image(req).await.map(|r| r.into_owned()) {
                Ok(r) => r.image.as_option().cloned().unwrap_or_default(),
                Err(e) => return err(rpc_err(e)),
            };
            println!(
                "Pulling {} as {} (Ctrl-C stops watching; the download continues)",
                what,
                img.name.yellow()
            );
            loop {
                match svc
                    .get_image(GetImageRequest {
                        id: img.id.clone(),
                        ..Default::default()
                    })
                    .await
                    .map(|r| r.into_owned())
                {
                    Ok(r) => {
                        let i = r.image.as_option().cloned().unwrap_or_default();
                        let state = image_state(&i);
                        print!("\r  {:<60}", image_status(&i));
                        let _ = io::stdout().flush();
                        if !matches!(
                            state,
                            Some(ImageState::IMAGE_STATE_DOWNLOADING)
                                | Some(ImageState::IMAGE_STATE_VERIFYING)
                                | Some(ImageState::IMAGE_STATE_EXTRACTING)
                        ) {
                            println!();
                            if state == Some(ImageState::IMAGE_STATE_READY) {
                                match i.kernel.as_option() {
                                    Some(k) => println!(
                                        "{} {} ({}{})",
                                        "Kernel ready:".green(),
                                        i.name,
                                        k.release.as_deref().unwrap_or("no release"),
                                        k.cmdline
                                            .as_deref()
                                            .map(|c| format!(", cmdline {c}"))
                                            .unwrap_or_default()
                                    ),
                                    None => println!(
                                        "{} {} ({}, sha256 {})",
                                        "Image ready:".green(),
                                        i.name,
                                        format_bytes(i.virtual_size_bytes),
                                        i.sha256
                                    ),
                                }
                                if brings_kernel {
                                    println!(
                                        "  Its kernel is being extracted as {}-kernel (see 'image list').",
                                        i.name
                                    );
                                }
                            }
                            break;
                        }
                    }
                    Err(e) => {
                        println!();
                        err(rpc_err(e));
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        "retry" => match args.get(1) {
            Some(name) => match svc
                .retry_image(RetryImageRequest {
                    id: name.to_string(),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => println!(
                    "{} {} (follow it with: image list)",
                    "Downloading again:".green(),
                    name
                ),
                Err(e) => err(rpc_err(e)),
            },
            None => usage(u),
        },
        "rm" | "delete" => match args.get(1) {
            Some(name) => match svc
                .delete_image(DeleteImageRequest {
                    id: name.to_string(),
                    wait_secs: Some(60),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) if r.image.is_set() => println!(
                    "{} {} (the control plane finishes it)",
                    "Deleting:".yellow(),
                    name
                ),
                Ok(_) => println!("{} {}", "Image deleted:".green(), name),
                Err(e) => err(rpc_err(e)),
            },
            None => usage(u),
        },
        _ => usage(u),
    }
}

// ---- disks --------------------------------------------------------------------

fn format_name(f: &EnumValue<DiskFormat>) -> &'static str {
    match f.as_known() {
        Some(DiskFormat::DISK_FORMAT_QCOW2) => "qcow2",
        Some(DiskFormat::DISK_FORMAT_RAW) => "raw",
        _ => "",
    }
}

fn clone_name(m: &EnumValue<CloneMode>) -> &'static str {
    match m.as_known() {
        Some(CloneMode::CLONE_MODE_LINKED) => "linked",
        Some(CloneMode::CLONE_MODE_FULL) => "full",
        _ => "",
    }
}

fn table_kind(k: &EnumValue<PartitionTableKind>) -> &'static str {
    match k.as_known() {
        Some(PartitionTableKind::PARTITION_TABLE_KIND_GPT) => "gpt",
        Some(PartitionTableKind::PARTITION_TABLE_KIND_MBR) => "mbr",
        _ => "",
    }
}

/// The `Ready` condition, when it isn't true.
fn not_ready(conditions: &[Condition]) -> Option<&Condition> {
    conditions.iter().find(|c| {
        c.kind == "Ready" && c.status.as_known() != Some(ConditionStatus::CONDITION_STATUS_TRUE)
    })
}

/// A disk after a write the disk controller carries out (spec/
/// reconciliation.md §10.1): its state, and what is still pending.
fn print_disk_result(d: &Disk) {
    println!(
        "  {} {} {} {}",
        if d.name.is_empty() { "?" } else { &d.name }.cyan(),
        format_bytes(d.size_bytes),
        format_name(&d.format),
        d.status
    );
    if let Some(ready) = not_ready(&d.conditions) {
        println!(
            "  {} {}: {}",
            "Not there yet:".yellow(),
            ready.reason,
            ready.message
        );
    }
    for w in &d.warnings {
        println!("  {} {}", "Warning:".yellow(), w);
    }
}

/// The disk of a write's answer; absent when it is gone already.
fn print_written(d: Option<&Disk>, name: &str) {
    match d {
        Some(d) => print_disk_result(d),
        None => println!("  {} is gone", name),
    }
}

/// `disk …`
pub async fn disk(client: &ApiClient, args: &[&str]) {
    let u = "Usage: disk list | show <disk> | create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend] [--datastore D] | resize <disk> <GiB> [--no-extend] | move <disk> --datastore D [--node N] [--flatten] | extend-root <disk> [--on-boot] | rm <disk>";
    let gib = |s: &str| {
        s.parse::<u64>()
            .map_err(|_| format!("{} is not a whole number of GiB", s))
    };
    let t = match rpc(client).await {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let svc = DiskServiceClient::new(t.clone(), t.config());
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match svc
            .list_disks(ListDisksRequest {
                project: client.project(),
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned().disks)
        {
            Ok(disks) if disks.is_empty() => println!("No disks."),
            Ok(disks) => {
                let vms = client.list_vms().await.unwrap_or_default();
                for d in &disks {
                    let vm = d.attached_to.as_deref().map(|id| {
                        vms.iter()
                            .find(|v| v.id == id)
                            .map(|v| v.name.clone())
                            .unwrap_or_else(|| id.to_string())
                    });
                    let origin = match d.origin.as_option().and_then(|o| o.origin.as_ref()) {
                        Some(Origin::Image(i)) => format!("image ({})", clone_name(&i.mode)),
                        _ => "blank".to_string(),
                    };
                    println!(
                        "  {:<20} {:>9} {:<6} {:<14} {:<7} {}",
                        d.name.cyan(),
                        format_bytes(d.size_bytes),
                        format_name(&d.format),
                        origin,
                        d.status,
                        vm.map(|v| format!("→ {}", v)).unwrap_or_default()
                    );
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        "show" | "get" => match args.get(1) {
            Some(name) => match svc
                .get_disk(GetDiskRequest {
                    id: name.to_string(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    let d = r.disk.as_option().cloned().unwrap_or_default();
                    print_disk_result(&d);
                    println!("  datastore: {}", d.datastore);
                    println!("  path: {}", d.path);
                    if let Some(b) = d.info.as_option().and_then(|i| i.backing_filename.as_ref()) {
                        println!("  backing file: {}", b);
                    }
                    if d.pending_growpart == Some(true) {
                        println!("  root partition grows on next boot");
                    }
                    match d.partition_table.as_option() {
                        Some(t) => {
                            println!("  partitions ({}):", table_kind(&t.kind));
                            for p in &t.partitions {
                                println!(
                                    "    {} start {:>9} size {:>9} {}{}",
                                    p.number,
                                    format_bytes(p.start_bytes),
                                    format_bytes(p.size_bytes),
                                    p.r#type,
                                    if p.is_root == Some(true) {
                                        " (root)"
                                    } else {
                                        ""
                                    }
                                );
                            }
                            println!("  free at end: {}", format_bytes(t.free_tail_bytes));
                        }
                        None => println!("  no partition table"),
                    }
                }
                Err(e) => err(rpc_err(e)),
            },
            None => usage(u),
        },
        "create" => {
            let Some(name) = args.get(1).filter(|a| !a.starts_with("--")) else {
                return usage(u);
            };
            let mut req = CreateDiskRequest {
                name: name.to_string(),
                project: client.project(),
                image: flag_value(args, "--image").map(String::from),
                datastore: flag_value(args, "--datastore").map(String::from),
                // Making it may wait for its image to download.
                wait_secs: Some(300),
                ..Default::default()
            };
            if let Some(n) = flag_value(args, "--size-gib") {
                match gib(n) {
                    Ok(n) => req.size_gib = Some(n),
                    Err(e) => return err(e),
                }
            }
            if has_flag(args, "--full") {
                req.clone = CloneMode::CLONE_MODE_FULL.into();
            }
            if has_flag(args, "--raw") {
                req.format = DiskFormat::DISK_FORMAT_RAW.into();
            }
            if has_flag(args, "--no-extend") {
                req.extend_root = Some(false);
            }
            match svc.create_disk(req).await.map(|r| r.into_owned()) {
                Ok(r) => {
                    println!("{}", "Disk created:".green());
                    print_written(r.disk.as_option(), name);
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "move" => {
            let (Some(name), Some(ds)) = (
                args.get(1).filter(|a| !a.starts_with("--")),
                flag_value(args, "--datastore"),
            ) else {
                return usage(u);
            };
            let req = MoveDiskRequest {
                id: name.to_string(),
                datastore: ds.to_string(),
                node: flag_value(args, "--node").map(String::from),
                flatten: has_flag(args, "--flatten").then_some(true),
                // A copy of the whole disk: allow it time.
                wait_secs: Some(300),
                ..Default::default()
            };
            match svc.move_disk(req).await.map(|r| r.into_owned()) {
                Ok(r) if r.settled == Some(false) => {
                    println!(
                        "{} {} is still moving (watch it with 'disk show {}')",
                        "Moving:".yellow(),
                        name,
                        name
                    );
                }
                Ok(r) => {
                    println!("{}", "Disk moved:".green());
                    print_written(r.disk.as_option(), name);
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "resize" => match (args.get(1), args.get(2)) {
            (Some(name), Some(size)) => {
                let size = match gib(size) {
                    Ok(n) => n,
                    Err(e) => return err(e),
                };
                let req = ResizeDiskRequest {
                    id: name.to_string(),
                    size_gib: Some(size),
                    extend_root: has_flag(args, "--no-extend").then_some(false),
                    wait_secs: Some(120),
                    ..Default::default()
                };
                match svc.resize_disk(req).await.map(|r| r.into_owned()) {
                    Ok(r) => {
                        println!("{}", "Disk resize:".green());
                        print_written(r.disk.as_option(), name);
                    }
                    Err(e) => err(rpc_err(e)),
                }
            }
            _ => usage(u),
        },
        "extend-root" => match args.get(1) {
            Some(name) => {
                let mode = if has_flag(args, "--on-boot") {
                    ExtendMode::EXTEND_MODE_ON_BOOT
                } else {
                    ExtendMode::EXTEND_MODE_OFFLINE
                };
                let req = ExtendRootRequest {
                    id: name.to_string(),
                    mode: mode.into(),
                    wait_secs: Some(120),
                    ..Default::default()
                };
                match svc.extend_root(req).await.map(|r| r.into_owned()) {
                    Ok(r) => print_written(r.disk.as_option(), name),
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => usage(u),
        },
        "rm" | "delete" => match args.get(1) {
            Some(name) => {
                if prompt(&format!("Delete disk {} and its data? [y/N]: ", name)).to_lowercase()
                    != "y"
                {
                    println!("Cancelled");
                    return;
                }
                match svc
                    .delete_disk(DeleteDiskRequest {
                        id: name.to_string(),
                        ..Default::default()
                    })
                    .await
                    .map(|r| r.into_owned())
                {
                    Ok(r) if r.disk.is_set() => println!(
                        "{} {} (once the operation on it finishes)",
                        "Deleting:".green(),
                        name
                    ),
                    Ok(_) => println!("{} {}", "Disk deleted:".green(), name),
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => usage(u),
        },
        _ => usage(u),
    }
}

// ---- datastores (spec/storage.md §13) -------------------------------------

const DATASTORE_USAGE: &str = "Usage: datastore list | show <datastore> | create <name> --path P [--shared] [--node-path NODE=P]... [--nodes N,M] [--default] [--no-overcommit] | set <datastore> [--node-path NODE=P]... [--nodes N,M | --all-nodes] [--default | --no-default] [--overcommit | --no-overcommit] | rm <datastore>";

/// `--node-path NODE=PATH`, repeated.
fn node_paths(args: &[&str]) -> Result<Vec<(String, String)>, String> {
    flag_values(args, "--node-path")
        .into_iter()
        .map(|v| {
            v.split_once('=')
                .map(|(n, p)| (n.to_string(), p.to_string()))
                .ok_or_else(|| format!("--node-path {v}: give NODE=PATH"))
        })
        .collect()
}

fn node_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// `n of m nodes reach it`, and the most free space any of them has.
fn reach(d: &Datastore) -> (String, String) {
    let ok: Vec<&DatastoreNode> = d.node_status.iter().filter(|r| r.reachable).collect();
    let free = ok.iter().map(|r| r.free_bytes).max();
    (
        format!("{}/{}", ok.len(), d.node_status.len()),
        free.map(format_bytes).unwrap_or_else(|| "-".into()),
    )
}

fn print_datastore(d: &Datastore) {
    println!(
        "{}{}{}",
        d.name.cyan().bold(),
        if d.builtin { " (built in)" } else { "" },
        if d.default_for_disks {
            " (default for disks)".green().to_string()
        } else {
            String::new()
        }
    );
    println!("  id: {}", d.id);
    if d.topology.as_known() == Some(DatastoreTopology::DATASTORE_TOPOLOGY_SHARED) {
        println!("  shared: the same files from every node (one mount)");
    }
    if !d.builtin {
        println!("  path: {}", d.path);
        let mut np: Vec<_> = d.node_paths.iter().collect();
        np.sort();
        for (n, p) in np {
            println!("  path on {n}: {p}");
        }
        println!(
            "  nodes: {}",
            if d.nodes.is_empty() {
                "all".to_string()
            } else {
                d.nodes.join(", ")
            }
        );
    }
    println!(
        "  overcommit: {}",
        if d.overcommit { "yes (thin)" } else { "no" }
    );
    println!("  disks: {}", d.disks);
    for r in &d.node_status {
        let who = r.node_name.clone().unwrap_or_else(|| r.node.clone());
        if r.reachable {
            println!(
                "  {} {}: {} free of {} at {}",
                "ok".green(),
                who,
                format_bytes(r.free_bytes),
                format_bytes(r.total_bytes),
                r.path
            );
            for w in &r.warnings {
                println!("    {} {w}", "warning:".yellow());
            }
        } else {
            println!(
                "  {} {}: {} {}",
                "unreachable".red(),
                who,
                r.reason,
                r.message
            );
        }
    }
}

pub async fn datastore(client: &ApiClient, args: &[&str]) {
    let u = DATASTORE_USAGE;
    let t = match rpc(client).await {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let svc = DatastoreServiceClient::new(t.clone(), t.config());
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match svc
            .list_datastores(ListDatastoresRequest::default())
            .await
            .map(|r| r.into_owned().datastores)
        {
            Ok(all) => {
                #[derive(Tabled)]
                struct Row {
                    #[tabled(rename = "NAME")]
                    name: String,
                    #[tabled(rename = "DEFAULT")]
                    default: String,
                    #[tabled(rename = "NODES OK")]
                    nodes: String,
                    #[tabled(rename = "FREE")]
                    free: String,
                    #[tabled(rename = "DISKS")]
                    disks: u32,
                    #[tabled(rename = "PATH")]
                    path: String,
                }
                let rows: Vec<Row> = all
                    .iter()
                    .map(|d| {
                        let (nodes, free) = reach(d);
                        Row {
                            name: d.name.clone(),
                            default: if d.default_for_disks {
                                "*".into()
                            } else {
                                String::new()
                            },
                            nodes,
                            free,
                            disks: d.disks,
                            path: if d.builtin {
                                "(each node's disk directory)".into()
                            } else {
                                d.path.clone()
                            },
                        }
                    })
                    .collect();
                println!("{}", Table::new(rows));
            }
            Err(e) => err(rpc_err(e)),
        },
        "show" | "get" => match args.get(1) {
            Some(key) => match svc
                .get_datastore(GetDatastoreRequest {
                    id: key.to_string(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => print_datastore(&r.datastore.as_option().cloned().unwrap_or_default()),
                Err(e) => err(rpc_err(e)),
            },
            None => usage(u),
        },
        "create" => {
            let (Some(name), Some(path)) = (
                args.get(1).filter(|a| !a.starts_with("--")),
                flag_value(args, "--path"),
            ) else {
                return usage(u);
            };
            let node_paths = match node_paths(args) {
                Ok(p) => p,
                Err(e) => return err(e),
            };
            let req = CreateDatastoreRequest {
                name: name.to_string(),
                driver: if has_flag(args, "--shared") {
                    DatastoreDriver::DATASTORE_DRIVER_SHARED_DIR
                } else {
                    DatastoreDriver::DATASTORE_DRIVER_DIR
                }
                .into(),
                path: path.to_string(),
                node_paths: node_paths.into_iter().collect(),
                nodes: flag_value(args, "--nodes")
                    .map(node_list)
                    .unwrap_or_default(),
                default_for_disks: has_flag(args, "--default").then_some(true),
                overcommit: has_flag(args, "--no-overcommit").then_some(false),
                ..Default::default()
            };
            match svc.create_datastore(req).await.map(|r| r.into_owned()) {
                Ok(r) => {
                    println!("{}", "Datastore created:".green());
                    print_datastore(&r.datastore.as_option().cloned().unwrap_or_default());
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "set" => {
            let Some(key) = args.get(1).filter(|a| !a.starts_with("--")) else {
                return usage(u);
            };
            let mut req = UpdateDatastoreRequest {
                id: key.to_string(),
                ..Default::default()
            };
            let mut mask = Vec::new();
            if !flag_values(args, "--node-path").is_empty() {
                match node_paths(args) {
                    Ok(p) => req.node_paths = p.into_iter().collect(),
                    Err(e) => return err(e),
                }
                mask.push("node_paths");
            }
            if let Some(n) = flag_value(args, "--nodes") {
                req.nodes = node_list(n);
                mask.push("nodes");
            } else if has_flag(args, "--all-nodes") {
                mask.push("nodes");
            }
            for (on, off, field) in [
                ("--default", "--no-default", "default_for_disks"),
                ("--overcommit", "--no-overcommit", "overcommit"),
            ] {
                let v = if has_flag(args, on) {
                    Some(true)
                } else if has_flag(args, off) {
                    Some(false)
                } else {
                    continue;
                };
                if field == "default_for_disks" {
                    req.default_for_disks = v;
                } else {
                    req.overcommit = v;
                }
                mask.push(field);
            }
            if mask.is_empty() {
                return usage(u);
            }
            req.update_mask = buffa_types::google::protobuf::FieldMask {
                paths: mask.into_iter().map(String::from).collect(),
                ..Default::default()
            }
            .into();
            match svc.update_datastore(req).await.map(|r| r.into_owned()) {
                Ok(r) => print_datastore(&r.datastore.as_option().cloned().unwrap_or_default()),
                Err(e) => err(rpc_err(e)),
            }
        }
        "rm" | "delete" => match args.get(1) {
            Some(key) => match svc
                .delete_datastore(DeleteDatastoreRequest {
                    id: key.to_string(),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => println!("{} {}", "Datastore deleted:".green(), key),
                Err(e) => err(rpc_err(e)),
            },
            None => usage(u),
        },
        _ => usage(u),
    }
}

#[cfg(test)]
mod datastore_tests {
    use super::*;

    #[test]
    fn node_paths_are_node_equals_path() {
        let args = [
            "create",
            "fast",
            "--node-path",
            "n1=/srv/glidex/a",
            "--node-path",
            "n2=/srv/glidex/b",
        ];
        assert_eq!(
            node_paths(&args).unwrap(),
            [
                ("n1".to_string(), "/srv/glidex/a".to_string()),
                ("n2".to_string(), "/srv/glidex/b".to_string())
            ]
        );
        assert!(node_paths(&["--node-path", "n1"]).is_err());
        assert_eq!(node_list(" a, b,,c "), ["a", "b", "c"]);
    }
}

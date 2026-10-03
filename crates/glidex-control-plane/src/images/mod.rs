//! Images and disks (spec/images.md).
//!
//! *Images* are read-only base cloud images downloaded from the catalog
//! (`catalog.rs`) or a URL and verified (`download.rs`). *Disks* are
//! writable volumes VMs boot from or attach, blank or cloned from an
//! image, that can be grown, shrunk and have their root partition
//! extended (`disk.rs`, `partition.rs`). All host tools are wrapped in
//! `qemu_img.rs`.
//!
//! Records live in the `images` and `disks` tables of the control plane's
//! ReDB file; files live under `<dir>/images` and `<dir>/disks`, named by
//! id only.

pub mod catalog;
pub mod disk;
pub mod download;
pub mod partition;
pub mod qemu_img;

use catalog::{Arch, HashAlgo};
use qemu_img::DiskFormat;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const IMAGES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("images");
pub const DISKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("disks");

pub use disk::{wanted_root_size, MaterializeOutcome};

pub const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    AlreadyExists(String),
    #[error("{0}")]
    InUse(String),
    #[error("{0}")]
    Busy(String),
    #[error("{0}")]
    NotReady(String),
    #[error("invalid image: {0}")]
    InvalidImage(String),
    #[error("invalid disk: {message}")]
    InvalidDisk { message: String, details: serde_json::Value },
    #[error("{tool} is not installed (install {package})")]
    ToolMissing { tool: String, package: String },
    #[error("{0}")]
    Io(String),
    #[error("{tool} failed: {stderr}")]
    Tool { tool: String, stderr: String },
    #[error("download failed: {0}")]
    Download(String),
    #[error("image storage error: {0}")]
    Storage(String),
}

impl ImageError {
    pub fn invalid_disk(message: impl Into<String>) -> Self {
        ImageError::InvalidDisk { message: message.into(), details: serde_json::Value::Null }
    }

    pub fn details(&self) -> serde_json::Value {
        match self {
            ImageError::InvalidDisk { details, .. } => details.clone(),
            _ => serde_json::Value::Null,
        }
    }

    /// Prefix the message (used when a multi-step operation fails part way).
    pub fn context(self, prefix: &str) -> Self {
        match self {
            ImageError::Tool { tool, stderr } => ImageError::Tool { tool, stderr: format!("{}: {}", prefix, stderr) },
            ImageError::Io(m) => ImageError::Io(format!("{}: {}", prefix, m)),
            other => other,
        }
    }
}

fn storage(e: impl std::fmt::Display) -> ImageError {
    ImageError::Storage(e.to_string())
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---- records ------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ImageSource {
    Catalog { key: String, url: String, version: String },
    Url { url: String, expected_sha256: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum ImageStatus {
    Downloading { received_bytes: u64, total_bytes: Option<u64> },
    Verifying,
    Ready,
    Failed { reason: String },
    Missing,
}

/// Download bookkeeping, kept so an interrupted download can resume.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DownloadMeta {
    /// The URL actually fetched (catalog entries resolve to one).
    #[serde(default)]
    pub url: String,
    /// Expected digest from the vendor or the caller.
    #[serde(default)]
    pub expected: Option<(HashAlgo, String)>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub id: String,
    pub name: String,
    pub source: ImageSource,
    pub status: ImageStatus,
    pub format: DiskFormat,
    pub virtual_size_bytes: u64,
    pub file_size_bytes: u64,
    /// sha256 of the stored file (hex); empty until Ready.
    pub sha256: String,
    pub arch: Arch,
    pub created_at: u64,
    #[serde(default)]
    pub download: DownloadMeta,
    /// Bumped by `POST /images/{id}/retry` (D19); a failed image is
    /// downloaded again once this is newer than `applied_retry_seq`.
    #[serde(default)]
    pub retry_seq: u64,
    #[serde(default)]
    pub applied_retry_seq: u64,
}

impl Image {
    /// Whether the file was checked against a digest the caller or vendor
    /// published (catalog images always are).
    pub fn verified(&self) -> bool {
        match &self.source {
            ImageSource::Catalog { .. } => true,
            ImageSource::Url { expected_sha256, .. } => expected_sha256.is_some(),
        }
    }

    pub fn catalog_key(&self) -> Option<&str> {
        match &self.source {
            ImageSource::Catalog { key, .. } => Some(key),
            ImageSource::Url { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CloneMode {
    /// qcow2 overlay with the image as backing file.
    #[default]
    Linked,
    /// Standalone copy.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DiskOrigin {
    Blank,
    Image { image_id: String, mode: CloneMode },
}

/// Where a disk is in its life (spec/reconciliation.md §10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiskPhase {
    /// Recorded; its file is made once the source image is ready.
    Pending,
    Creating,
    #[default]
    Ready,
    Resizing,
    /// The file is gone. Never recreated: the data is gone.
    Missing,
    /// Creating it failed; see its `Ready` condition.
    Failed,
}

/// How a pending disk is to be made (the create request, kept until it is).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskCreateSpec {
    /// Requested size; `None`: the image's size or the default root size.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Grow the root partition into a disk larger than its image.
    #[serde(default)]
    pub extend_root: Option<bool>,
}

/// A one-shot extend-root request (D19): applied once `seq` is newer than
/// the disk's `applied_extend_root_seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtendRootSpec {
    pub mode: ExtendMode,
    pub seq: u64,
}

/// A resize waiting to be applied (§10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeSpec {
    pub size_bytes: u64,
    #[serde(default)]
    pub extend_root: Option<bool>,
}

/// A disk: spec (`format`, `origin`, `create`, `resize`, `extend_root`,
/// `owner`) and status (`phase`, `size_bytes`, `pending_growpart`,
/// `applied_extend_root_seq`, `conditions`). The record keeps its flat
/// shape; the fields added for the controller default, so records from
/// before it load as `Ready` disks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Disk {
    pub id: String,
    pub name: String,
    /// Owning project id (spec/security.md §6); empty in old records
    /// until `VmManager::initialize` assigns the default project.
    #[serde(default)]
    pub project: String,
    pub format: DiskFormat,
    /// Actual virtual size, a multiple of 1 MiB (0 until created).
    pub size_bytes: u64,
    pub origin: DiskOrigin,
    /// VM id; at most one.
    #[serde(default)]
    pub attached_to: Option<String>,
    /// The next generated cloud-init seed grows the root partition.
    #[serde(default)]
    pub pending_growpart: bool,
    pub created_at: u64,
    #[serde(default)]
    pub phase: DiskPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<DiskCreateSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resize: Option<ResizeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extend_root: Option<ExtendRootSpec>,
    #[serde(default)]
    pub applied_extend_root_seq: u64,
    /// The VM this disk was made for (deleted with it, unless kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_requested_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<crate::models::Condition>,
}

impl Disk {
    /// A disk record with the controller fields at their defaults.
    pub fn new(id: String, name: String, project: String, format: DiskFormat, size_bytes: u64, origin: DiskOrigin) -> Self {
        Disk {
            id,
            name,
            project,
            format,
            size_bytes,
            origin,
            attached_to: None,
            pending_growpart: false,
            created_at: now(),
            phase: DiskPhase::Ready,
            create: None,
            resize: None,
            extend_root: None,
            applied_extend_root_seq: 0,
            owner: None,
            deletion_requested_at: None,
            conditions: Vec::new(),
        }
    }

    pub fn is_linked(&self) -> bool {
        matches!(self.origin, DiskOrigin::Image { mode: CloneMode::Linked, .. })
    }

    pub fn is_from_image(&self) -> bool {
        matches!(self.origin, DiskOrigin::Image { .. })
    }
}

// ---- requests / responses ------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PullImageRequest {
    #[serde(default)]
    pub catalog: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CreateDiskRequest {
    pub name: String,
    /// Project id or name; default: the caller's default project. The
    /// control plane resolves it to an id before the disk is created.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub size_gib: Option<u64>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Image id or name.
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub clone: Option<CloneMode>,
    #[serde(default)]
    pub format: Option<DiskFormat>,
    #[serde(default)]
    pub extend_root: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ResizeDiskRequest {
    #[serde(default)]
    pub size_gib: Option<u64>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub extend_root: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExtendMode {
    #[default]
    Offline,
    OnBoot,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ExtendRootRequest {
    #[serde(default)]
    pub mode: ExtendMode,
}

/// What an extend-root step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtendOutcome {
    Grown,
    AlreadyFull,
    /// Deferred to the guest's cloud-init on next boot.
    OnBoot,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageResponse {
    pub id: String,
    pub name: String,
    pub source: ImageSource,
    pub status: ImageStatus,
    pub format: DiskFormat,
    pub virtual_size_bytes: u64,
    pub file_size_bytes: u64,
    pub sha256: String,
    pub arch: Arch,
    pub created_at: u64,
    pub verified: bool,
    pub path: String,
    /// Linked disks that depend on this image.
    pub linked_disks: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<qemu_img::ImgInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskResponse {
    pub id: String,
    pub name: String,
    pub project: String,
    pub format: DiskFormat,
    pub size_bytes: u64,
    pub origin: DiskOrigin,
    pub attached_to: Option<String>,
    pub pending_growpart: bool,
    /// `pending`, `creating`, `ready`, `resizing`, `busy`, `missing` or
    /// `failed`.
    pub status: String,
    pub phase: DiskPhase,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<crate::models::Condition>,
    /// A resize not applied yet (the disk is in use, §10.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deleting: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy_op: Option<String>,
    pub path: String,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<qemu_img::ImgInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_table: Option<partition::PartitionTable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extend_root: Option<ExtendOutcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogItem {
    pub key: String,
    pub distro: String,
    pub release: String,
    pub arch: Arch,
    pub url: String,
    pub downloaded_image_id: Option<String>,
}

// ---- manager ---------------------------------------------------------------

/// Tunables, from the environment by default.
#[derive(Debug, Clone)]
pub struct ImageSettings {
    pub image_dir: PathBuf,
    pub disk_dir: PathBuf,
    pub max_image_size: u64,
    pub default_root_bytes: u64,
    pub max_downloads: usize,
    pub allow_private_urls: bool,
}

impl ImageSettings {
    /// `base` is the directory holding the database (normally `~/.glidex`).
    pub fn from_env(base: &Path) -> Self {
        let dir = |var: &str, sub: &str| {
            std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| base.join(sub))
        };
        Self {
            image_dir: dir("GLIDEX_IMAGE_DIR", "images"),
            disk_dir: dir("GLIDEX_DISK_DIR", "disks"),
            max_image_size: std::env::var("GLIDEX_MAX_IMAGE_SIZE")
                .ok()
                .and_then(|v| parse_size(&v))
                .unwrap_or(64 * GIB),
            default_root_bytes: std::env::var("GLIDEX_DEFAULT_ROOT_GIB")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(10)
                * GIB,
            max_downloads: std::env::var("GLIDEX_MAX_DOWNLOADS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(2),
            allow_private_urls: std::env::var("GLIDEX_ALLOW_PRIVATE_IMAGE_URLS").is_ok_and(|v| v == "1"),
        }
    }
}

/// `123`, `64G`, `512M`, `1T` (binary units) → bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'G' => (&s[..s.len() - 1], 1 << 30),
        'T' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mult)
}

/// Names of images and disks: what users type in CLIs and URLs.
pub fn validate_name(kind: &str, name: &str) -> Result<(), ImageError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !name.starts_with('.');
    if ok {
        Ok(())
    } else {
        let msg = format!("{} name must be 1-64 characters of A-Z a-z 0-9 . _ - (not starting with '.')", kind);
        Err(if kind == "image" { ImageError::InvalidImage(msg) } else { ImageError::invalid_disk(msg) })
    }
}

pub struct ImageManager {
    db: Arc<Database>,
    pub settings: ImageSettings,
    images: RwLock<HashMap<String, Image>>,
    disks: RwLock<HashMap<String, Disk>>,
    /// Disk id → operation in progress.
    busy: Mutex<HashMap<String, &'static str>>,
    /// Image id → disks being cloned from it but not yet committed.
    holds: Mutex<HashMap<String, usize>>,
    downloads: Arc<tokio::sync::Semaphore>,
    tasks: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    http: reqwest::Client,
}

/// Marks a disk busy for as long as it lives.
pub struct BusyGuard {
    mgr: Arc<ImageManager>,
    id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.mgr.busy.lock().unwrap().remove(&self.id);
    }
}

/// Keeps an image from being deleted while a disk cloned from it is
/// created but not yet in the disk cache (which `delete_image` checks).
/// Drop it after the disk record is committed and cached.
pub struct ImageHold {
    mgr: Arc<ImageManager>,
    image_id: String,
}

impl Drop for ImageHold {
    fn drop(&mut self) {
        let mut holds = self.mgr.holds.lock().unwrap();
        if let Some(n) = holds.get_mut(&self.image_id) {
            *n -= 1;
            if *n == 0 {
                holds.remove(&self.image_id);
            }
        }
    }
}

fn mkdir_private(dir: &Path) -> Result<(), ImageError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| ImageError::Io(format!("{}: {}", dir.display(), e)))
}

impl ImageManager {
    pub fn new(db: Arc<Database>, settings: ImageSettings) -> Result<Arc<Self>, ImageError> {
        let txn = db.begin_write().map_err(storage)?;
        txn.open_table(IMAGES_TABLE).map_err(storage)?;
        txn.open_table(DISKS_TABLE).map_err(storage)?;
        txn.commit().map_err(storage)?;
        mkdir_private(&settings.image_dir)?;
        mkdir_private(&settings.disk_dir)?;

        let mgr = Self {
            downloads: Arc::new(tokio::sync::Semaphore::new(settings.max_downloads)),
            http: download::http_client(settings.allow_private_urls)?,
            db,
            settings,
            images: RwLock::new(HashMap::new()),
            disks: RwLock::new(HashMap::new()),
            busy: Mutex::new(HashMap::new()),
            holds: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
        };
        *mgr.images.write().unwrap() = mgr.load(IMAGES_TABLE)?;
        *mgr.disks.write().unwrap() = mgr.load(DISKS_TABLE)?;
        Ok(Arc::new(mgr))
    }

    fn load<T: serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &[u8]>,
    ) -> Result<HashMap<String, T>, ImageError> {
        let txn = self.db.begin_read().map_err(storage)?;
        let t = txn.open_table(table).map_err(storage)?;
        let mut out = HashMap::new();
        for row in t.iter().map_err(storage)? {
            let (k, v) = row.map_err(storage)?;
            match serde_json::from_slice(v.value()) {
                Ok(rec) => {
                    out.insert(k.value().to_string(), rec);
                }
                Err(e) => tracing::warn!(id = k.value(), "skipping unreadable {} record: {}", table, e),
            }
        }
        Ok(out)
    }

    /// Reconcile records with files and resume interrupted downloads.
    /// Never deletes anything: orphans are only logged, and records whose
    /// file is gone are marked missing (spec §2).
    pub fn initialize(self: &Arc<Self>) {
        let mut resume = Vec::new();
        let mut known = std::collections::HashSet::new();
        let images: Vec<Image> = self.images.read().unwrap().values().cloned().collect();
        for mut img in images {
            known.insert(self.image_path(&img.id));
            known.insert(self.part_path(&img.id));
            let exists = self.image_path(&img.id).exists();
            let new_status = match &img.status {
                ImageStatus::Ready | ImageStatus::Missing if !exists => Some(ImageStatus::Missing),
                ImageStatus::Missing if exists => Some(ImageStatus::Ready),
                ImageStatus::Downloading { .. } | ImageStatus::Verifying => {
                    resume.push(img.id.clone());
                    None
                }
                _ => None,
            };
            if let Some(s) = new_status {
                if s != img.status {
                    tracing::warn!(image = %img.name, "image status {:?} -> {:?}", img.status, s);
                    img.status = s;
                    let _ = self.put_image(&img);
                }
            }
        }
        let disks: Vec<Disk> = self.disks.read().unwrap().values().cloned().collect();
        for d in &disks {
            let path = self.disk_path(d);
            if !path.exists() {
                tracing::warn!(disk = %d.name, path = %path.display(), "disk file is missing");
            }
            known.insert(path);
        }
        for dir in [&self.settings.image_dir, &self.settings.disk_dir] {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if !known.contains(&p) {
                        tracing::warn!(path = %p.display(), "file without a glidex record left in place");
                    }
                }
            }
        }
        for id in resume {
            tracing::info!(image = %id, "resuming interrupted image download");
            self.spawn_download(id);
        }
    }

    // ---- paths -----------------------------------------------------------

    pub fn image_path(&self, id: &str) -> PathBuf {
        self.settings.image_dir.join(format!("{}.qcow2", id))
    }

    pub fn part_path(&self, id: &str) -> PathBuf {
        self.settings.image_dir.join(format!("{}.qcow2.part", id))
    }

    pub fn disk_path(&self, d: &Disk) -> PathBuf {
        self.settings.disk_dir.join(format!("{}.{}", d.id, d.format.extension()))
    }

    // ---- persistence -------------------------------------------------------

    pub fn put_image(&self, img: &Image) -> Result<(), ImageError> {
        let bytes = serde_json::to_vec(img).map_err(storage)?;
        let txn = self.db.begin_write().map_err(storage)?;
        txn.open_table(IMAGES_TABLE)
            .map_err(storage)?
            .insert(img.id.as_str(), bytes.as_slice())
            .map_err(storage)?;
        txn.commit().map_err(storage)?;
        self.images.write().unwrap().insert(img.id.clone(), img.clone());
        Ok(())
    }

    /// Update the cached record only (download progress between persists).
    fn cache_image(&self, img: &Image) {
        self.images.write().unwrap().insert(img.id.clone(), img.clone());
    }

    fn remove_image_record(&self, id: &str) -> Result<(), ImageError> {
        let txn = self.db.begin_write().map_err(storage)?;
        txn.open_table(IMAGES_TABLE).map_err(storage)?.remove(id).map_err(storage)?;
        txn.commit().map_err(storage)?;
        self.images.write().unwrap().remove(id);
        Ok(())
    }

    pub fn put_disk(&self, d: &Disk) -> Result<(), ImageError> {
        let txn = self.db.begin_write().map_err(storage)?;
        write_disk(&txn, d)?;
        txn.commit().map_err(storage)?;
        self.cache_disk(d);
        Ok(())
    }

    /// After a transaction that wrote `d` (see `persistence::VmStore::commit`).
    pub fn cache_disk(&self, d: &Disk) {
        self.disks.write().unwrap().insert(d.id.clone(), d.clone());
    }

    pub fn uncache_disk(&self, id: &str) {
        self.disks.write().unwrap().remove(id);
    }

    // ---- lookup ------------------------------------------------------------

    pub fn list_images(&self) -> Vec<Image> {
        let mut v: Vec<Image> = self.images.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.name.cmp(&b.name)));
        v
    }

    /// By id, then by name.
    pub fn get_image(&self, key: &str) -> Result<Image, ImageError> {
        let images = self.images.read().unwrap();
        images
            .get(key)
            .or_else(|| images.values().find(|i| i.name == key))
            .cloned()
            .ok_or_else(|| ImageError::NotFound(format!("image not found: {}", key)))
    }

    pub fn list_disks(&self) -> Vec<Disk> {
        let mut v: Vec<Disk> = self.disks.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.name.cmp(&b.name)));
        v
    }

    pub fn get_disk(&self, key: &str) -> Result<Disk, ImageError> {
        let disks = self.disks.read().unwrap();
        disks
            .get(key)
            .or_else(|| disks.values().find(|d| d.name == key))
            .cloned()
            .ok_or_else(|| ImageError::NotFound(format!("disk not found: {}", key)))
    }

    pub fn disk_name_taken(&self, name: &str) -> bool {
        self.disks.read().unwrap().values().any(|d| d.name == name)
    }

    pub fn linked_disks(&self, image_id: &str) -> Vec<Disk> {
        self.disks
            .read()
            .unwrap()
            .values()
            .filter(|d| matches!(&d.origin, DiskOrigin::Image { image_id: i, mode: CloneMode::Linked } if i == image_id))
            .cloned()
            .collect()
    }

    // ---- busy marks ----------------------------------------------------------

    /// Mark a disk busy with `op`; `409` if another operation holds it.
    pub fn begin(self: &Arc<Self>, id: &str, op: &'static str) -> Result<BusyGuard, ImageError> {
        let mut busy = self.busy.lock().unwrap();
        if let Some(other) = busy.get(id) {
            return Err(ImageError::Busy(format!("disk {} is busy ({})", id, other)));
        }
        busy.insert(id.to_string(), op);
        Ok(BusyGuard { mgr: self.clone(), id: id.to_string() })
    }

    pub fn busy_op(&self, id: &str) -> Option<&'static str> {
        self.busy.lock().unwrap().get(id).copied()
    }

    /// See `ImageHold`. Taken under the same lock `delete_image` checks.
    pub fn hold_image(self: &Arc<Self>, image_id: &str) -> ImageHold {
        *self.holds.lock().unwrap().entry(image_id.to_string()).or_insert(0) += 1;
        ImageHold { mgr: self.clone(), image_id: image_id.to_string() }
    }

    // ---- responses -------------------------------------------------------------

    pub fn image_response(&self, img: &Image, with_info: bool) -> ImageResponse {
        let path = self.image_path(&img.id);
        let info = (with_info && img.status == ImageStatus::Ready)
            .then(|| qemu_img::info(&path, Some(DiskFormat::Qcow2)).ok())
            .flatten();
        ImageResponse {
            id: img.id.clone(),
            name: img.name.clone(),
            source: img.source.clone(),
            status: img.status.clone(),
            format: img.format,
            virtual_size_bytes: img.virtual_size_bytes,
            file_size_bytes: img.file_size_bytes,
            sha256: img.sha256.clone(),
            arch: img.arch,
            created_at: img.created_at,
            verified: img.verified(),
            path: path.to_string_lossy().into_owned(),
            linked_disks: self.linked_disks(&img.id).into_iter().map(|d| d.name).collect(),
            info,
        }
    }

    /// `with_detail` runs `qemu-img info` and reads the partition table;
    /// blocking, so only for single-disk responses.
    pub fn disk_response(&self, d: &Disk, with_detail: bool) -> DiskResponse {
        let path = self.disk_path(d);
        let busy_op = self.busy_op(&d.id);
        let status = match d.phase {
            DiskPhase::Pending => "pending",
            DiskPhase::Creating => "creating",
            DiskPhase::Failed => "failed",
            _ if busy_op.is_some() => "busy",
            DiskPhase::Resizing => "resizing",
            _ if !path.exists() => "missing",
            _ => "ready",
        };
        let (info, table) = if with_detail && status == "ready" {
            (
                qemu_img::info(&path, Some(d.format)).ok(),
                partition::read_table(&path, d.format, d.size_bytes).ok().flatten(),
            )
        } else {
            (None, None)
        };
        DiskResponse {
            id: d.id.clone(),
            name: d.name.clone(),
            project: d.project.clone(),
            format: d.format,
            size_bytes: d.size_bytes,
            origin: d.origin.clone(),
            attached_to: d.attached_to.clone(),
            pending_growpart: d.pending_growpart,
            status: status.to_string(),
            phase: d.phase,
            conditions: d.conditions.clone(),
            pending_size_bytes: d.resize.map(|r| r.size_bytes),
            owner: d.owner.clone(),
            deleting: d.deletion_requested_at.is_some(),
            busy_op: busy_op.map(str::to_string),
            path: path.to_string_lossy().into_owned(),
            created_at: d.created_at,
            info,
            partition_table: table,
            extend_root: None,
            warnings: Vec::new(),
        }
    }

    pub fn catalog(&self) -> Vec<CatalogItem> {
        let Some(arch) = Arch::host() else { return Vec::new() };
        let images = self.images.read().unwrap();
        catalog::for_arch(arch)
            .map(|(e, img)| CatalogItem {
                key: e.key.to_string(),
                distro: e.distro.to_string(),
                release: e.release.to_string(),
                arch,
                url: img.url.to_string(),
                downloaded_image_id: images
                    .values()
                    .filter(|i| i.catalog_key() == Some(e.key) && i.status == ImageStatus::Ready)
                    .max_by_key(|i| i.created_at)
                    .map(|i| i.id.clone()),
            })
            .collect()
    }

    // ---- images ------------------------------------------------------------------

    /// Delete an image, or cancel its download. Refused while linked disks
    /// depend on it.
    pub fn delete_image(&self, key: &str) -> Result<(), ImageError> {
        let img = self.get_image(key)?;
        // Held across the check and the record removal, so a clone that
        // starts now either sees the image gone or blocks the delete.
        let holds = self.holds.lock().unwrap();
        if holds.contains_key(&img.id) {
            return Err(ImageError::InUse(format!("image {} is being cloned into a new disk", img.name)));
        }
        let linked = self.linked_disks(&img.id);
        if !linked.is_empty() {
            return Err(ImageError::InUse(format!(
                "image {} is the backing file of linked disk(s): {}",
                img.name,
                linked.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        if let Some(task) = self.tasks.lock().unwrap().remove(&img.id) {
            task.abort();
        }
        self.remove_image_record(&img.id)?;
        drop(holds);
        for p in [self.image_path(&img.id), self.part_path(&img.id)] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %p.display(), "could not remove image file: {}", e),
            }
        }
        tracing::info!(image = %img.name, "image deleted");
        Ok(())
    }
}

/// Write a disk record inside a caller's transaction, so it can commit
/// together with the VM that references it.
pub fn write_disk(txn: &WriteTransaction, d: &Disk) -> Result<(), ImageError> {
    let bytes = serde_json::to_vec(d).map_err(storage)?;
    txn.open_table(DISKS_TABLE)
        .map_err(storage)?
        .insert(d.id.as_str(), bytes.as_slice())
        .map_err(storage)?;
    Ok(())
}

pub fn delete_disk_record(txn: &WriteTransaction, id: &str) -> Result<(), ImageError> {
    txn.open_table(DISKS_TABLE).map_err(storage)?.remove(id).map_err(storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("64G"), Some(64 * GIB));
        assert_eq!(parse_size("512m"), Some(512 << 20));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn names() {
        assert!(validate_name("disk", "web-1_root.v2").is_ok());
        for bad in ["", ".hidden", "a/b", "../x", "a b", &"x".repeat(65)] {
            assert!(validate_name("disk", bad).is_err(), "{:?}", bad);
        }
    }

    #[test]
    fn image_cannot_be_deleted_while_a_clone_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.db")).unwrap());
        let mgr = ImageManager::new(db, ImageSettings::from_env(dir.path())).unwrap();
        let img = Image {
            id: "img-1".into(),
            name: "base".into(),
            source: ImageSource::Url { url: "https://example.com/x.qcow2".into(), expected_sha256: None },
            status: ImageStatus::Ready,
            format: DiskFormat::Qcow2,
            virtual_size_bytes: GIB,
            file_size_bytes: 0,
            sha256: String::new(),
            arch: Arch::X86_64,
            created_at: 0,
            download: DownloadMeta::default(),
            retry_seq: 0,
            applied_retry_seq: 0,
        };
        mgr.put_image(&img).unwrap();
        let hold = mgr.hold_image("img-1");
        let second = mgr.hold_image("img-1");
        assert!(matches!(mgr.delete_image("base"), Err(ImageError::InUse(_))));
        drop(hold);
        assert!(matches!(mgr.delete_image("base"), Err(ImageError::InUse(_))), "one hold left");
        drop(second);
        mgr.delete_image("base").unwrap();
        assert!(matches!(mgr.get_image("base"), Err(ImageError::NotFound(_))));
    }

    #[test]
    fn record_json_shape() {
        let d = Disk::new("i".into(), "n".into(), String::new(), DiskFormat::Qcow2, GIB, DiskOrigin::Image { image_id: "img".into(), mode: CloneMode::Linked });
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["origin"], serde_json::json!({"kind": "image", "image_id": "img", "mode": "linked"}));
        // Records from before the disk controller load as ready disks.
        let old: Disk = serde_json::from_value(serde_json::json!({
            "id": "i", "name": "n", "format": "qcow2", "size_bytes": 1, "origin": {"kind": "blank"}, "created_at": 0
        }))
        .unwrap();
        assert_eq!((old.phase, old.applied_extend_root_seq), (DiskPhase::Ready, 0));
        assert_eq!(v["format"], "qcow2");
        let s = serde_json::to_value(ImageStatus::Downloading { received_bytes: 1, total_bytes: None }).unwrap();
        assert_eq!(s["state"], "downloading");
        assert_eq!(serde_json::to_value(ExtendMode::OnBoot).unwrap(), "on-boot");
    }
}

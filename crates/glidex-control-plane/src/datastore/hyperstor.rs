//! The `hyperstor` driver (spec/24-storage.md §11.2): each disk is a volume of
//! a hyperstor cluster, served to its VM over vhost-user-blk by the node's
//! `hyperstor-vhost`.
//!
//! - **Managing volumes** (create, clone, expand, delete) goes to the
//!   cluster API over mutual TLS, as the `mgmt` identity glidex enrolled
//!   with a token (S20). Only servers hold it, so a hyperstor disk's worker
//!   is a server (S22).
//! - **Disk bytes** (filling a volume from an image, partition edits) go
//!   through `hyperstorctl write-volume`/`read-volume`, straight to the map
//!   service, with no root and no block device (S21). For the partition
//!   code a volume is the path `hyperstor://<map>/<volume>`.
//! - **Staging** asks the node's `hyperstor-vhost` to attach the volume
//!   through its control socket and gives the hypervisor the socket it
//!   opens; a VM re-placed from a fenced node preempts (S23). Stopping the
//!   VM detaches.
//! - **Images**: one volume per image (its id derived from the image's),
//!   filled from the verified image file, with its snapshot `golden`; a
//!   linked disk is a clone of `golden`, instant and sharing its data.

use super::{Caps, Prepare, StorageDriver, VolumeBinding, VolumeSource};
use crate::images::qemu_img::{self, DiskFormat};
use crate::images::{Disk, DiskOrigin, ImageError};
use hyperstor_api::proto::hyperstor::v1 as pb;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The scheme of the paths that name volumes for the partition code.
pub const SCHEME: &str = "hyperstor://";
/// The snapshot of an image volume that disks are cloned from.
pub const GOLDEN: &str = "golden";
/// `hyperstor-vhost`'s directory, unless `GLIDEX_HYPERSTOR_VHOST_DIR` says
/// otherwise (hyperstor's default).
const VHOST_DIR: &str = "/run/hyperstor/vhost";
/// How long an API call may take.
const API_TIMEOUT: Duration = Duration::from_secs(30);

/// The certificate glidex enrolled with, opened (S20).
#[derive(Clone)]
pub struct Identity {
    pub cert: String,
    pub key: zeroize::Zeroizing<String>,
    pub root: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Identity(..)")
    }
}

#[derive(Debug)]
pub struct HyperstorDriver {
    /// The cluster API's addresses (`host:port`), tried in turn.
    api: Vec<String>,
    /// The map service's addresses, for `hyperstorctl --map`.
    map: String,
    pool: u32,
    /// The datastore record's id: image volumes are named for it, so that
    /// another glidex cluster's in the same pool are told apart.
    datastore: String,
    /// Only on servers (S22).
    identity: Option<Identity>,
    vhost_dir: PathBuf,
    ctl: PathBuf,
    /// This node's id, named to `hyperstor-vhost` at attach so its lease
    /// says which node holds the volume (hyperstor D153).
    node: String,
}

/// `hyperstor-vhost`'s directory on this node.
pub fn vhost_dir() -> PathBuf {
    std::env::var_os("GLIDEX_HYPERSTOR_VHOST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(VHOST_DIR))
}

/// `hyperstorctl`, unless `GLIDEX_HYPERSTORCTL` names another.
pub fn ctl_path() -> PathBuf {
    std::env::var_os("GLIDEX_HYPERSTORCTL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("hyperstorctl"))
}

/// A disk's volume id: its UUID's 32 hex digits, or a hash of its id.
pub fn volume_id(disk: &Disk) -> String {
    id_of(&disk.id, "glidex-disk:")
}

/// A refused `hyperstor-vhost` request: the error, and for `held` the
/// label of the lease that holds the volume (hyperstor D153).
#[derive(Debug)]
struct VhostRefusal {
    error: Box<ImageError>,
    holder: Option<String>,
}

impl From<ImageError> for VhostRefusal {
    fn from(error: ImageError) -> Self {
        VhostRefusal {
            error: Box::new(error),
            holder: None,
        }
    }
}

/// Whether a hold labelled `holder` is another node's `hyperstor-vhost`
/// (`vhost:<node>`), as a VM starting on node `me` finds a stale one
/// (S26). A tool's (`hyperstorctl`), an unnamed service's (`vhost`) and
/// this node's own are not.
pub fn stale_hold(holder: &str, me: &str) -> bool {
    holder
        .strip_prefix("vhost:")
        .is_some_and(|n| !n.is_empty() && n != me)
}

/// The volumes attached here (`attached`) that this node should let go
/// (S26): each a glidex disk's (`disks`: volume and the node its VM is
/// placed on, if any) whose VM isn't placed here. Volumes glidex doesn't
/// know are left alone.
pub fn stale_here(
    attached: &[String],
    disks: &[(String, Option<String>)],
    me: &str,
) -> Vec<String> {
    attached
        .iter()
        .filter(|v| {
            disks
                .iter()
                .find(|(d, _)| d == *v)
                .is_some_and(|(_, node)| node.as_deref() != Some(me))
        })
        .cloned()
        .collect()
}

/// An image's volume id, derived from the image's (one per datastore: the
/// datastore is another cluster, or another pool).
pub fn image_volume_id(image_id: &str) -> String {
    hyperstor_pki::sha256_hex(format!("glidex-image:{image_id}").as_bytes())[..32].to_string()
}

/// `id` as a volume id: a UUID as it is, anything else hashed under
/// `domain` (so a disk and an image named alike get different ids).
fn id_of(id: &str, domain: &str) -> String {
    match uuid::Uuid::parse_str(id) {
        Ok(u) => u.simple().to_string(),
        Err(_) => hyperstor_pki::sha256_hex(format!("{domain}{id}").as_bytes())[..32].to_string(),
    }
}

/// The path the partition code reads and writes `volume` through.
pub fn tool_path(map: &str, volume: &str) -> String {
    format!("{SCHEME}{map}/{volume}")
}

/// `(map, volume)` of a path [`tool_path`] made.
pub fn parse_tool_path(p: &Path) -> Option<(String, String)> {
    let s = p.to_str()?.strip_prefix(SCHEME)?;
    let (map, vol) = s.rsplit_once('/')?;
    (!map.is_empty() && vol.len() == 32).then(|| (map.to_string(), vol.to_string()))
}

/// The disk's serial number as its guest sees it (frontends.md §11.1):
/// the first 20 characters of its id without dashes.
pub fn serial(disk: &Disk) -> String {
    disk.id.replace('-', "").chars().take(20).collect()
}

fn ctl_err(what: &str, e: std::io::Error) -> ImageError {
    ImageError::NotReady(format!("hyperstorctl ({what}): {e}"))
}

/// Reads `len` bytes of `path`'s volume at `offset` into `out`.
pub fn read_region(path: &Path, offset: u64, len: u64, out: &Path) -> Result<(), ImageError> {
    read_region_with(&ctl_path(), path, offset, len, out)
}

fn read_region_with(
    ctl: &Path,
    path: &Path,
    offset: u64,
    len: u64,
    out: &Path,
) -> Result<(), ImageError> {
    let (map, vol) = parse_tool_path(path)
        .ok_or_else(|| ImageError::Io(format!("{}: not a volume", path.display())))?;
    run_ctl(
        ctl,
        &[
            "--map",
            &map,
            "read-volume",
            "--id",
            &vol,
            "--offset",
            &offset.to_string(),
            "--size",
            &len.to_string(),
            "--out",
            &out.to_string_lossy(),
        ],
    )
}

/// Writes `file` into `path`'s volume at `offset`, holding its lease.
pub fn write_region(path: &Path, file: &Path, offset: u64, sparse: bool) -> Result<(), ImageError> {
    write_region_with(&ctl_path(), path, file, offset, sparse)
}

fn write_region_with(
    ctl: &Path,
    path: &Path,
    file: &Path,
    offset: u64,
    sparse: bool,
) -> Result<(), ImageError> {
    let (map, vol) = parse_tool_path(path)
        .ok_or_else(|| ImageError::Io(format!("{}: not a volume", path.display())))?;
    let off = offset.to_string();
    let f = file.to_string_lossy();
    let mut args = vec![
        "--map",
        &map,
        "write-volume",
        "--id",
        &vol,
        "--offset",
        &off,
        "--file",
        &f,
    ];
    if sparse {
        args.push("--sparse");
    }
    run_ctl(ctl, &args)
}

fn run_ctl(ctl: &Path, args: &[&str]) -> Result<(), ImageError> {
    let out = std::process::Command::new(ctl)
        .args(args)
        .output()
        .map_err(|e| ctl_err(&ctl.to_string_lossy(), e))?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    // Another client has the volume (its VM runs elsewhere).
    if msg.contains("attached by") {
        return Err(ImageError::Busy(msg));
    }
    Err(ImageError::Tool {
        tool: "hyperstorctl".into(),
        stderr: msg,
    })
}

/// Runs `f` on a runtime of its own, on a thread of its own: the driver's
/// methods block, and may be called from inside another runtime. A Tokio
/// worker waits for it in `block_in_place` (up to the API's timeout per
/// address), so the worker's runtime keeps driving its I/O.
fn block_on<T: Send>(f: impl std::future::Future<Output = T> + Send) -> T {
    glidex_raftstore::wait::blocking(|| {
        std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime")
                    .block_on(f)
            })
            .join()
            .expect("the API call's thread")
        })
    })
}

/// An API error as the image code's.
/// Whether hyperstor's answer says the volume isn't there (any more).
fn is_gone(m: &str) -> bool {
    m.contains("no such volume") || m.contains("not found") || m.contains("no volume ")
}

fn api_err(e: connectrpc::ConnectError) -> ImageError {
    use connectrpc::ErrorCode as C;
    let msg = e.message.clone().unwrap_or_else(|| "request failed".into());
    match e.code {
        C::Unavailable | C::DeadlineExceeded => ImageError::NotReady(format!("hyperstor: {msg}")),
        C::FailedPrecondition => ImageError::Busy(format!("hyperstor: {msg}")),
        C::InvalidArgument | C::OutOfRange => ImageError::invalid_disk(format!("hyperstor: {msg}")),
        _ => ImageError::Io(format!("hyperstor: {msg}")),
    }
}

impl HyperstorDriver {
    pub fn new(
        api: Vec<String>,
        map: String,
        pool: u32,
        datastore: String,
        identity: Option<Identity>,
    ) -> Self {
        Self {
            api,
            map,
            pool,
            datastore,
            identity,
            vhost_dir: vhost_dir(),
            ctl: ctl_path(),
            node: String::new(),
        }
    }

    /// The driver as node `node` uses it: its attaches name the node.
    pub fn on_node(mut self, node: &str) -> Self {
        self.node = node.to_string();
        self
    }

    /// `ds`'s driver, if it is a hyperstor datastore.
    pub fn of(ds: &super::record::Datastore, identity: Option<Identity>) -> Option<Self> {
        let super::record::DriverSpec::Hyperstor { api, map, pool, .. } = &ds.driver else {
            return None;
        };
        Some(Self::new(
            api.clone(),
            map.clone(),
            *pool,
            ds.id.clone(),
            identity,
        ))
    }

    /// What image `image_id`'s volume is called.
    fn image_volume_name(&self, image_id: &str) -> String {
        format!("glidex image {image_id} on {}", self.datastore)
    }

    /// Deletes this datastore's image volumes whose image `live` says is
    /// gone: those a disk's clone kept (in hyperstor's trash) when the
    /// image went, once the clone is purged (spec/24-storage.md S24).
    /// Returns how many went.
    pub fn collect_image_volumes(&self, live: impl Fn(&str) -> bool) -> Result<usize, ImageError> {
        let suffix = format!(" on {}", self.datastore);
        let mut gone = 0;
        for v in self.volumes()? {
            let Some(image) = v
                .name
                .strip_prefix("glidex image ")
                .and_then(|n| n.strip_suffix(&suffix))
            else {
                continue;
            };
            // One being purged is on its way already.
            if live(image) || v.id != image_volume_id(image) || v.state != "active" {
                continue;
            }
            match self.delete(&v.id) {
                Ok(()) => gone += 1,
                // Clones still need it.
                Err(ImageError::Busy(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(gone)
    }

    /// TLS as glidex's identity.
    fn tls(&self) -> Result<std::sync::Arc<connectrpc::rustls::ClientConfig>, ImageError> {
        let id = self.identity.as_ref().ok_or_else(|| {
            ImageError::NotReady(
                "hyperstor volumes are managed from glidex's servers, which hold its identity"
                    .into(),
            )
        })?;
        let tls = hyperstor_pki::client_config(&id.cert, &id.key, &id.root)
            .map_err(|e| ImageError::Io(format!("hyperstor identity: {e}")))?;
        Ok(std::sync::Arc::new(tls))
    }

    /// The pool's usable total and free bytes (hyperstor D156), from the
    /// first address whose member knows them: only the map service's leader
    /// hears the devices' reports. `None` when none does (an older hyperstor,
    /// or a device yet to report).
    pub fn space(&self) -> Result<Option<(u64, u64)>, ImageError> {
        let tls = self.tls()?;
        block_on(async {
            let (mut answered, mut last) = (false, None);
            for a in &self.api {
                let Ok(uri) = format!("https://{a}").parse::<http::Uri>() else {
                    continue;
                };
                let client = hyperstor_api::ClusterServiceClient::new(
                    connectrpc::client::HttpClient::with_tls(tls.clone()),
                    connectrpc::client::ClientConfig::new(uri).with_default_timeout(API_TIMEOUT),
                );
                match client.get_status(pb::GetStatusRequest::default()).await {
                    Ok(s) => {
                        answered = true;
                        if let Some(space) = pool_space(&s.into_owned().pools, self.pool) {
                            return Ok(Some(space));
                        }
                    }
                    Err(e) => last = Some(e),
                }
            }
            match last {
                Some(e) if !answered => Err(api_err(e)),
                _ => Ok(None),
            }
        })
    }

    /// Calls the cluster API through each address in turn while one is
    /// unreachable.
    fn call<T, F, Fut>(&self, f: F) -> Result<T, ImageError>
    where
        T: Send,
        F: Fn(hyperstor_api::ClusterServiceClient<connectrpc::client::HttpClient>) -> Fut + Sync,
        Fut: std::future::Future<Output = Result<T, connectrpc::ConnectError>> + Send,
    {
        self.call_on(hyperstor_api::ClusterServiceClient::new, f)
    }

    /// [`Self::call`] on the trash's service.
    fn trash<T, F, Fut>(&self, f: F) -> Result<T, ImageError>
    where
        T: Send,
        F: Fn(hyperstor_api::TrashServiceClient<connectrpc::client::HttpClient>) -> Fut + Sync,
        Fut: std::future::Future<Output = Result<T, connectrpc::ConnectError>> + Send,
    {
        self.call_on(hyperstor_api::TrashServiceClient::new, f)
    }

    fn call_on<C, T, F, Fut>(
        &self,
        make: fn(connectrpc::client::HttpClient, connectrpc::client::ClientConfig) -> C,
        f: F,
    ) -> Result<T, ImageError>
    where
        T: Send,
        F: Fn(C) -> Fut + Sync,
        Fut: std::future::Future<Output = Result<T, connectrpc::ConnectError>> + Send,
    {
        let tls = self.tls()?;
        block_on(async {
            let mut last = None;
            for a in &self.api {
                let Ok(uri) = format!("https://{a}").parse::<http::Uri>() else {
                    continue;
                };
                let config =
                    connectrpc::client::ClientConfig::new(uri).with_default_timeout(API_TIMEOUT);
                let client = make(
                    connectrpc::client::HttpClient::with_tls(tls.clone()),
                    config,
                );
                match f(client).await {
                    Err(e) if e.code == connectrpc::ErrorCode::Unavailable => last = Some(e),
                    r => return r.map_err(api_err),
                }
            }
            Err(last.map_or_else(
                || ImageError::NotReady("no hyperstor API address".into()),
                api_err,
            ))
        })
    }

    fn volumes(&self) -> Result<Vec<pb::Volume>, ImageError> {
        let pool = self.pool;
        self.call(|c| async move {
            c.list_volumes(pb::ListVolumesRequest {
                pool,
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned().volumes)
        })
    }

    fn volume(&self, id: &str) -> Result<Option<pb::Volume>, ImageError> {
        Ok(self.volumes()?.into_iter().find(|v| v.id == id))
    }

    fn create(&self, id: &str, name: &str, size: u64) -> Result<(), ImageError> {
        let (pool, id, name) = (self.pool, id.to_string(), name.to_string());
        self.call(|c| {
            let (id, name) = (id.clone(), name.clone());
            async move {
                c.create_volume(pb::CreateVolumeRequest {
                    pool,
                    size,
                    name,
                    id,
                    ..Default::default()
                })
                .await
                .map(|_| ())
            }
        })
    }

    fn delete(&self, id: &str) -> Result<(), ImageError> {
        let id = id.to_string();
        let r = self.call(|c| {
            let id = id.clone();
            async move {
                c.delete_volume(pb::DeleteVolumeRequest {
                    id,
                    ..Default::default()
                })
                .await
                .map(|_| ())
            }
        });
        match r {
            // Gone already.
            Err(ImageError::Io(m)) if is_gone(&m) => Ok(()),
            r => r,
        }
    }

    /// Whether volume `id` is in hyperstor's trash, where its id stays
    /// taken until it is purged.
    fn in_trash(&self, id: &str) -> Result<bool, ImageError> {
        let pool = self.pool;
        let items = self.trash(|c| async move {
            c.list_trash(pb::ListTrashRequest {
                pool,
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned().items)
        })?;
        Ok(items.iter().any(|i| i.kind == "volume" && i.id == id))
    }

    /// Purges volume `id` from the trash; it goes in the background.
    fn purge(&self, id: &str) -> Result<(), ImageError> {
        let id = id.to_string();
        let r = self.trash(|c| {
            let id = id.clone();
            async move {
                c.purge_from_trash(pb::PurgeFromTrashRequest {
                    id,
                    ..Default::default()
                })
                .await
                .map(|_| ())
            }
        });
        match r {
            // Not in the trash (purged already, or never deleted).
            Err(ImageError::Busy(_)) => Ok(()),
            Err(ImageError::Io(m)) if is_gone(&m) => Ok(()),
            r => r,
        }
    }

    /// Removes a volume a creation left (it never held a disk's data) for
    /// good, so that its id can be made again (spec/24-storage.md S25):
    /// deleted, it would stay in hyperstor's trash, and its id with it.
    fn discard(&self, id: &str) -> Result<(), ImageError> {
        self.delete(id)?;
        self.purge(id)
    }

    /// Makes `id` free to create (S25): a volume a creation left under it
    /// is deleted and purged, and the purge waited for. A volume being
    /// purged (`deleting`) still holds the id, out of the trash.
    fn free_id(&self, id: &str) -> Result<(), ImageError> {
        let (mut deleted, mut purged) = (false, false);
        for _ in 0..120 {
            match self.volume(id)? {
                Some(v) if v.state == "active" && !deleted => {
                    self.delete(id)?;
                    deleted = true;
                    continue;
                }
                // Being purged (or a delete still landing): wait.
                Some(_) => {}
                None if !self.in_trash(id)? => return Ok(()),
                None if !purged => {
                    self.purge(id)?;
                    purged = true;
                }
                None => {}
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Err(ImageError::NotReady(format!(
            "hyperstor volume {id} is still being deleted; its id is taken until then"
        )))
    }

    fn expand(&self, id: &str, size: u64) -> Result<(), ImageError> {
        let id = id.to_string();
        self.call(|c| {
            let id = id.clone();
            async move {
                c.expand_volume(pb::ExpandVolumeRequest {
                    id,
                    size,
                    ..Default::default()
                })
                .await
                .map(|_| ())
            }
        })
    }

    fn golden(&self, id: &str) -> Result<Option<u64>, ImageError> {
        let id = id.to_string();
        let snaps = self.call(|c| {
            let id = id.clone();
            async move {
                c.list_snapshots(pb::ListSnapshotsRequest {
                    id,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned().snapshots)
            }
        })?;
        Ok(snaps
            .into_iter()
            .find(|s| s.name == GOLDEN && s.state != "deleting")
            .map(|s| s.epoch))
    }

    /// A `size`-byte volume `id` holding image `file` (any format qemu-img
    /// reads), written with zero blocks skipped.
    fn fill_from_image(
        &self,
        id: &str,
        name: &str,
        file: &Path,
        size: u64,
    ) -> Result<(), ImageError> {
        // Converted first, so that the volume exists only while it is
        // written (holding its lease: another worker sees it filling).
        let tmp = tempfile::Builder::new()
            .prefix(".gx-hyperstor")
            .tempdir()
            .map_err(|e| ImageError::Io(format!("temporary directory: {e}")))?;
        let raw = tmp.path().join("image.raw");
        qemu_img::convert(file, DiskFormat::Qcow2, &raw, DiskFormat::Raw)?;
        self.create(id, name, size)?;
        let r = write_region_with(
            &self.ctl,
            Path::new(&tool_path(&self.map, id)),
            &raw,
            0,
            true,
        );
        if r.is_err() {
            let _ = self.discard(id);
        }
        r
    }

    /// The image volume for `image_id`, with its `golden` snapshot, made
    /// from `file` when there is none yet. Two workers making it at once
    /// is settled by the volume's id: the second waits for the first's
    /// snapshot, or redoes a volume the first left unfinished.
    fn image_volume(
        &self,
        image_id: &str,
        file: &Path,
        size: u64,
    ) -> Result<(String, u64), ImageError> {
        let id = image_volume_id(image_id);
        let deadline = std::time::Instant::now() + Duration::from_secs(30 * 60);
        // Each round makes progress or waits; a few rounds of another
        // worker's volume coming and going, and it's that worker's error.
        let mut rounds = 0;
        loop {
            rounds += 1;
            if rounds > 20 {
                return Err(ImageError::Busy(format!(
                    "image {image_id}'s volume keeps changing under this worker"
                )));
            }
            match self.volume(&id)? {
                Some(_) => {
                    if let Some(epoch) = self.golden(&id)? {
                        return Ok((id, epoch));
                    }
                    let v = self.volume(&id)?;
                    let filling = v.as_ref().is_some_and(|v| !v.attached_by.is_empty());
                    if !filling {
                        // Left unfinished by a worker that stopped: redo it.
                        self.free_id(&id)?;
                        continue;
                    }
                    // Waiting doesn't count as a round.
                    rounds -= 1;
                    if std::time::Instant::now() > deadline {
                        return Err(ImageError::Busy(format!(
                            "image {image_id}'s volume is still being filled"
                        )));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                None => {
                    // Free if a deleted one is still in the trash.
                    self.free_id(&id)?;
                    match self.fill_from_image(&id, &self.image_volume_name(image_id), file, size) {
                        Ok(()) => {}
                        // Another worker made it first.
                        Err(ImageError::Io(m)) if m.contains("exists") => continue,
                        Err(e) => return Err(e),
                    }
                    let (vid, name) = (id.clone(), GOLDEN.to_string());
                    let snap = self.call(|c| {
                        let (id, name) = (vid.clone(), name.clone());
                        async move {
                            c.create_snapshot(pb::CreateSnapshotRequest {
                                id,
                                name,
                                ..Default::default()
                            })
                            .await
                            .map(|r| r.into_owned().snapshot.into_option().map(|s| s.epoch))
                        }
                    })?;
                    let epoch =
                        snap.ok_or_else(|| ImageError::Io("no snapshot in the answer".into()))?;
                    return Ok((id, epoch));
                }
            }
        }
    }

    /// Delete image `image_id`'s volume (its golden snapshot with it); one
    /// gone already is fine, one that disks still clone from is refused.
    pub fn delete_image_volume(&self, image_id: &str) -> Result<(), ImageError> {
        self.delete(&image_volume_id(image_id))
    }

    /// The binding the partition code works on, for `id`.
    fn tool_binding_of(&self, id: &str) -> VolumeBinding {
        VolumeBinding::VhostUserBlk {
            socket: String::new(),
            volume: id.to_string(),
            map: self.map.clone(),
        }
    }

    /// One request to the node's `hyperstor-vhost` (hyperstor D117).
    fn vhost(&self, req: serde_json::Value) -> Result<serde_json::Value, ImageError> {
        self.vhost_call(req).map_err(|e| *e.error)
    }

    /// [`Self::vhost`], keeping a `held` answer's holder: the label of the
    /// lease that holds the volume (hyperstor D153).
    fn vhost_call(&self, req: serde_json::Value) -> Result<serde_json::Value, VhostRefusal> {
        // Up to a minute: off the worker on a Tokio runtime's.
        glidex_raftstore::wait::blocking(|| self.vhost_call_now(req))
    }

    fn vhost_call_now(&self, req: serde_json::Value) -> Result<serde_json::Value, VhostRefusal> {
        let path = self.vhost_dir.join("control.sock");
        let unavailable = |e: std::io::Error| {
            VhostRefusal::from(ImageError::NotReady(format!(
                "hyperstor-vhost ({}): {e}",
                path.display()
            )))
        };
        let mut s = std::os::unix::net::UnixStream::connect(&path).map_err(unavailable)?;
        s.set_read_timeout(Some(Duration::from_secs(60)))
            .map_err(unavailable)?;
        writeln!(s, "{req}").map_err(unavailable)?;
        let mut line = String::new();
        BufReader::new(s)
            .read_line(&mut line)
            .map_err(unavailable)?;
        let v: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| ImageError::Io(format!("hyperstor-vhost's answer: {e}")))?;
        if let Some(e) = v.get("error") {
            let code = e["code"].as_str().unwrap_or("");
            let msg = format!("hyperstor-vhost: {}", e["message"].as_str().unwrap_or(""));
            return Err(VhostRefusal {
                holder: (code == "held").then(|| e["holder"].as_str().unwrap_or("").to_string()),
                error: Box::new(match code {
                    "held" => ImageError::Busy(msg),
                    "unavailable" => ImageError::NotReady(msg),
                    "not_found" => ImageError::Io(msg),
                    _ => ImageError::Io(msg),
                }),
            });
        }
        Ok(v["ok"].clone())
    }

    /// Attaches `disk`'s volume on this node, preempting its holder when
    /// `preempt` (a VM re-placed from a fenced node, S23).
    pub fn attach(&self, disk: &Disk, preempt: bool) -> Result<VolumeBinding, ImageError> {
        self.attach_take(disk, if preempt { "preempt" } else { "attach" })
            .map_err(|e| *e.error)
    }

    /// Attaches `disk`'s volume for a VM starting here, which glidex
    /// records as running nowhere (S26): a hold by another node's
    /// `hyperstor-vhost` is one left by a crash or a failed detach, and is
    /// preempted, which cuts off that volume's lease only (hyperstor D150,
    /// D153). A tool's hold (a disk operation on a server) is waited for.
    pub fn attach_starting(&self, disk: &Disk, preempt: bool) -> Result<VolumeBinding, ImageError> {
        if preempt {
            return self.attach(disk, true);
        }
        match self.attach_take(disk, "attach") {
            Err(VhostRefusal {
                holder: Some(h), ..
            }) if stale_hold(&h, &self.node) => {
                tracing::warn!(
                    disk = %disk.name,
                    holder = %h,
                    "a VM's disk is held by another node's hyperstor-vhost: preempting it (S26)"
                );
                self.attach(disk, true)
            }
            r => r.map_err(|e| *e.error),
        }
    }

    fn attach_take(&self, disk: &Disk, take: &str) -> Result<VolumeBinding, VhostRefusal> {
        let volume = volume_id(disk);
        let mut args = serde_json::json!({
            "volume": volume,
            "serial": serial(disk),
            "take": take,
        });
        if !self.node.is_empty() {
            args["holder"] = self.node.clone().into();
        }
        let ok = self.vhost_call(serde_json::json!({
            "id": 1,
            "op": "attach",
            "args": args,
        }))?;
        let socket = ok["socket"].as_str().ok_or_else(|| {
            VhostRefusal::from(ImageError::Io("hyperstor-vhost gave no socket".into()))
        })?;
        Ok(VolumeBinding::VhostUserBlk {
            socket: socket.to_string(),
            volume,
            map: self.map.clone(),
        })
    }

    /// Detaches `disk`'s volume from this node; one not attached here is
    /// fine.
    pub fn detach(&self, disk: &Disk) -> Result<(), ImageError> {
        match self.vhost(serde_json::json!({
            "id": 1,
            "op": "detach",
            "args": { "volume": volume_id(disk) },
        })) {
            Ok(_) => Ok(()),
            Err(ImageError::Io(m)) if m.contains("isn't attached") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The volumes attached to this node's `hyperstor-vhost`.
    pub fn attached_volumes(&self) -> Result<Vec<String>, ImageError> {
        let ok = self.vhost(serde_json::json!({"id": 1, "op": "list"}))?;
        Ok(ok["disks"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| d["volume"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Detaches volume `volume` from this node; one not attached here is
    /// fine.
    pub fn detach_volume(&self, volume: &str) -> Result<(), ImageError> {
        match self.vhost(serde_json::json!({
            "id": 1,
            "op": "detach",
            "args": { "volume": volume },
        })) {
            Ok(_) => Ok(()),
            Err(ImageError::Io(m)) if m.contains("isn't attached") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether this node's `hyperstor-vhost` answers.
    pub fn vhost_answers(&self) -> Result<(), ImageError> {
        self.vhost(serde_json::json!({"id": 1, "op": "hello"}))
            .map(|_| ())
    }

    /// Whether the cluster API answers to the identity (servers only).
    pub fn api_answers(&self) -> Result<(), ImageError> {
        self.volumes().map(|_| ())
    }

    pub fn has_identity(&self) -> bool {
        self.identity.is_some()
    }

    pub fn ctl(&self) -> &Path {
        &self.ctl
    }

    /// Where `hyperstor-vhost`'s sockets are, and which `hyperstorctl`
    /// runs (tests).
    pub fn with_paths(mut self, vhost_dir: PathBuf, ctl: PathBuf) -> Self {
        self.vhost_dir = vhost_dir;
        self.ctl = ctl;
        self
    }
}

impl StorageDriver for HyperstorDriver {
    fn capabilities(&self) -> Caps {
        Caps {
            linked_clones: true,
            shrink: false,
        }
    }

    fn binding(&self, disk: &Disk) -> VolumeBinding {
        let volume = volume_id(disk);
        VolumeBinding::VhostUserBlk {
            socket: self
                .vhost_dir
                .join(format!("{volume}.sock"))
                .to_string_lossy()
                .into_owned(),
            volume,
            map: self.map.clone(),
        }
    }

    fn exists(&self, disk: &Disk) -> bool {
        self.volume(&volume_id(disk)).is_ok_and(|v| v.is_some())
    }

    fn create_volume(
        &self,
        disk: &Disk,
        size: u64,
        source: VolumeSource<'_>,
        prepare: Prepare<'_>,
    ) -> Result<(), ImageError> {
        let id = volume_id(disk);
        // Left by an attempt that didn't finish: there is no rename to
        // build it aside (a volume under the disk's id is not yet the
        // disk's until this returns).
        self.free_id(&id)?;
        let made = match source {
            VolumeSource::Blank => self.create(&id, &disk.name, size),
            VolumeSource::Image {
                path,
                size_bytes,
                linked: false,
            } => self.fill_from_image(&id, &disk.name, path, size.max(size_bytes)),
            VolumeSource::Image {
                path,
                size_bytes,
                linked: true,
            } => {
                let DiskOrigin::Image { image_id, .. } = &disk.origin else {
                    return Err(ImageError::invalid_disk("a linked disk names its image"));
                };
                let (origin, epoch) = self.image_volume(image_id, path, size_bytes)?;
                let (new_id, name) = (id.clone(), disk.name.clone());
                self.call(|c| {
                    let (origin, new_id, name) = (origin.clone(), new_id.clone(), name.clone());
                    async move {
                        c.clone_volume(pb::CloneVolumeRequest {
                            id: origin,
                            epoch,
                            name,
                            new_id,
                            ..Default::default()
                        })
                        .await
                        .map(|_| ())
                    }
                })
                .and_then(|()| {
                    if size > size_bytes {
                        self.expand(&id, size)
                    } else {
                        Ok(())
                    }
                })
            }
        };
        let r = made.and_then(|()| prepare(&self.tool_binding_of(&id)));
        if r.is_err() {
            let _ = self.discard(&id);
        }
        r
    }

    fn copy_in(
        &self,
        disk: &Disk,
        _from: &Path,
        _from_format: DiskFormat,
        _flatten: bool,
        _prepare: Prepare<'_>,
    ) -> Result<(), ImageError> {
        Err(ImageError::invalid_disk(format!(
            "disk {} can't be moved onto a hyperstor datastore yet",
            disk.name
        )))
    }

    fn delete_volume(&self, disk: &Disk) -> Result<(), ImageError> {
        self.delete(&volume_id(disk))
    }

    fn expand_volume(&self, disk: &Disk, size: u64) -> Result<(), ImageError> {
        self.expand(&volume_id(disk), size)
    }

    fn shrink_volume(&self, disk: &Disk, _: u64, _: Prepare<'_>) -> Result<(), ImageError> {
        Err(ImageError::invalid_disk(format!(
            "disk {} is a hyperstor volume, which can't be made smaller",
            disk.name
        )))
    }

    fn volume_size(&self, disk: &Disk) -> Option<u64> {
        self.volume(&volume_id(disk)).ok().flatten().map(|v| v.size)
    }

    fn stored_bytes(&self, _disk: &Disk) -> Option<u64> {
        // hyperstor doesn't report a volume's allocated bytes yet.
        None
    }

    fn stage(&self, disk: &Disk) -> Result<VolumeBinding, ImageError> {
        self.attach(disk, false)
    }

    fn tool_binding(&self, disk: &Disk) -> Result<VolumeBinding, ImageError> {
        Ok(self.tool_binding_of(&volume_id(disk)))
    }

    fn attach_for_vm(&self, disk: &Disk, preempt: bool) -> Result<VolumeBinding, ImageError> {
        self.attach_starting(disk, preempt)
    }

    fn unstage(&self, disk: &Disk) -> Result<(), ImageError> {
        self.detach(disk)
    }
}

/// What enrolling with a `mgmt` token gave (S20).
pub struct Enrolled {
    pub identity: super::record::HyperstorIdentity,
    pub key: zeroize::Zeroizing<String>,
    /// The map service's addresses.
    pub map: String,
}

fn identity_record(cert: &str, root: &str) -> Result<super::record::HyperstorIdentity, String> {
    let name = hyperstor_pki::identity_in(cert)
        .map(|i| i.uri())
        .ok_or("the certificate names no hyperstor identity")?;
    let not_after = hyperstor_pki::not_after(cert)?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    Ok(super::record::HyperstorIdentity {
        cert: cert.to_string(),
        root: root.to_string(),
        name,
        not_after,
    })
}

/// Enrolls with `token` through the first of `api` that answers: a key made
/// here (it never leaves glidex), the API checked against the root the
/// token names.
pub async fn enroll(api: &[String], token: &str) -> Result<Enrolled, String> {
    let (pin, _) = hyperstor_pki::parse_join_token(token).ok_or(
        "not a hyperstor token (hsj1-…): make one with hyperstorctl token create --role mgmt",
    )?;
    let (key, csr) = hyperstor_pki::new_request()?;
    let tls = std::sync::Arc::new(hyperstor_pki::pinned_client_config(pin)?);
    let mut last = String::from("no hyperstor API address");
    for a in api {
        let Ok(uri) = format!("https://{a}").parse::<http::Uri>() else {
            last = format!("{a}: not host:port");
            continue;
        };
        let config = connectrpc::client::ClientConfig::new(uri).with_default_timeout(API_TIMEOUT);
        let client = hyperstor_api::TokenServiceClient::new(
            connectrpc::client::HttpClient::with_tls(tls.clone()),
            config,
        );
        let r = client
            .join(pb::JoinRequest {
                token: token.trim().to_string(),
                csr: csr.clone(),
                ..Default::default()
            })
            .await;
        match r {
            Ok(r) => {
                let r = r.into_owned();
                return Ok(Enrolled {
                    identity: identity_record(&r.certificate, &r.root)?,
                    key: zeroize::Zeroizing::new(key),
                    map: r.map_addresses,
                });
            }
            Err(e) if e.code == connectrpc::ErrorCode::Unavailable => {
                last = format!("{a}: {}", e.message.unwrap_or_default());
            }
            Err(e) => {
                return Err(format!(
                    "{a}: {}",
                    e.message.unwrap_or_else(|| "refused".into())
                ));
            }
        }
    }
    Err(last)
}

fn cluster_client(
    a: &str,
    id: &Identity,
) -> Result<hyperstor_api::ClusterServiceClient<connectrpc::client::HttpClient>, String> {
    let tls = std::sync::Arc::new(hyperstor_pki::client_config(&id.cert, &id.key, &id.root)?);
    let uri = format!("https://{a}")
        .parse::<http::Uri>()
        .map_err(|_| format!("{a}: not host:port"))?;
    Ok(hyperstor_api::ClusterServiceClient::new(
        connectrpc::client::HttpClient::with_tls(tls),
        connectrpc::client::ClientConfig::new(uri).with_default_timeout(API_TIMEOUT),
    ))
}

/// Whether `pool` is one of the cluster's data pools, asked as `id`.
pub async fn check_pool(api: &[String], id: &Identity, pool: u32) -> Result<(), String> {
    let mut last = String::from("no hyperstor API address");
    for a in api {
        let r = cluster_client(a, id)?
            .get_status(pb::GetStatusRequest::default())
            .await;
        match r {
            Ok(s) => {
                let s = s.into_owned();
                return if s.pools.iter().any(|p| p.id == pool && p.id != 0) {
                    Ok(())
                } else {
                    Err(format!(
                        "pool {pool} isn't a data pool of the cluster (it has {})",
                        s.pools
                            .iter()
                            .filter(|p| p.id != 0)
                            .map(|p| format!("{} ({})", p.id, p.name))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                };
            }
            Err(e) => last = format!("{a}: {}", e.message.unwrap_or_default()),
        }
    }
    Err(last)
}

/// A renewed certificate for `id`, with a new key (S20); `None` when it
/// isn't due yet (a third of its life left).
pub fn renew(api: &[String], id: &Identity) -> Result<Option<Enrolled>, String> {
    if !hyperstor_pki::due_for_renewal(&id.cert, 30) {
        return Ok(None);
    }
    let (key, csr) = hyperstor_pki::new_request()?;
    let tls = std::sync::Arc::new(hyperstor_pki::client_config(&id.cert, &id.key, &id.root)?);
    block_on(async {
        let mut last = String::from("no hyperstor API address");
        for a in api {
            let Ok(uri) = format!("https://{a}").parse::<http::Uri>() else {
                continue;
            };
            let client = hyperstor_api::CaServiceClient::new(
                connectrpc::client::HttpClient::with_tls(tls.clone()),
                connectrpc::client::ClientConfig::new(uri).with_default_timeout(API_TIMEOUT),
            );
            match client
                .renew_certificate(pb::RenewCertificateRequest {
                    csr: csr.clone(),
                    ..Default::default()
                })
                .await
            {
                Ok(r) => {
                    let r = r.into_owned();
                    return Ok(Some(Enrolled {
                        identity: identity_record(&r.certificate, &r.root)?,
                        key: zeroize::Zeroizing::new(key.clone()),
                        map: String::new(),
                    }));
                }
                Err(e) => last = format!("{a}: {}", e.message.unwrap_or_default()),
            }
        }
        Err(last)
    })
}

/// `pool`'s usable total and free bytes in a status answer, when the member
/// that gave it knows them (hyperstor D156).
fn pool_space(pools: &[pb::Pool], pool: u32) -> Option<(u64, u64)> {
    let p = pools.iter().find(|p| p.id == pool)?;
    Some((p.total_bytes?, p.free_bytes?))
}

/// What this node finds of a hyperstor datastore (§4.1): its
/// `hyperstor-vhost` answers (`VhostUnavailable` if not), and, on a server,
/// the cluster API answers to glidex's identity (`ApiUnavailable`) and
/// gives the pool's space (S27). Other nodes, and a cluster that doesn't
/// report it, show 0.
pub fn probe(driver: &HyperstorDriver, map: &str, now: u64) -> super::record::NodeRow {
    let mut row = super::record::NodeRow {
        reachable: true,
        path: map.to_string(),
        checked_at: now,
        ..Default::default()
    };
    if let Err(e) = driver.vhost_answers() {
        row.reachable = false;
        row.reason = "VhostUnavailable".into();
        row.message = e.to_string();
        return row;
    }
    if driver.has_identity()
        && let Err(e) = driver.api_answers()
    {
        row.reachable = false;
        row.reason = "ApiUnavailable".into();
        row.message = e.to_string();
        return row;
    }
    if driver.has_identity() {
        match driver.space() {
            Ok(Some((total, free))) => (row.total_bytes, row.free_bytes) = (total, free),
            Ok(None) => {}
            Err(e) => tracing::debug!("hyperstor pool space: {e}"),
        }
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::{DiskOrigin, DiskPhase};
    use std::os::unix::net::UnixListener;

    fn disk(id: &str) -> Disk {
        let mut d = Disk::new(
            id.into(),
            "data".into(),
            String::new(),
            DiskFormat::Raw,
            1 << 30,
            DiskOrigin::Blank,
        );
        d.phase = DiskPhase::Pending;
        d
    }

    #[test]
    fn volumes_are_named_from_disks_and_images() {
        let d = disk("6f1c0d2e-9a4b-4c1d-8e2f-0123456789ab");
        assert_eq!(volume_id(&d), "6f1c0d2e9a4b4c1d8e2f0123456789ab");
        assert_eq!(serial(&d), "6f1c0d2e9a4b4c1d8e2f");
        // Not a UUID: a hash, still 32 hex digits, still the same each time.
        let odd = volume_id(&disk("legacy-disk"));
        assert_eq!(odd.len(), 32);
        assert_eq!(odd, volume_id(&disk("legacy-disk")));
        assert_ne!(image_volume_id("img-1"), image_volume_id("img-2"));
        assert_eq!(image_volume_id("img-1").len(), 32);
        let p = tool_path("10.0.0.5:8849,10.0.0.6:8849", &odd);
        assert_eq!(
            parse_tool_path(Path::new(&p)),
            Some(("10.0.0.5:8849,10.0.0.6:8849".into(), odd.clone()))
        );
        assert_eq!(parse_tool_path(Path::new("/disks/root.qcow2")), None);
        assert_eq!(parse_tool_path(Path::new("hyperstor://map/short")), None);
        let b = VolumeBinding::VhostUserBlk {
            socket: String::new(),
            volume: odd.clone(),
            map: "m:1".into(),
        };
        assert_eq!(b.path().to_str().unwrap(), format!("hyperstor://m:1/{odd}"));
        assert_eq!(b.vhost_socket(), None, "not staged");
    }

    #[test]
    fn pool_space_is_the_pools_when_the_member_knows_it() {
        let pool = |id, space: Option<(u64, u64)>| pb::Pool {
            id,
            total_bytes: space.map(|s| s.0),
            free_bytes: space.map(|s| s.1),
            ..Default::default()
        };
        let pools = [
            pool(0, Some((9, 9))),
            pool(1, Some((100, 40))),
            pool(2, None),
        ];
        assert_eq!(pool_space(&pools, 1), Some((100, 40)));
        // Not the leader, or a device yet to report.
        assert_eq!(pool_space(&pools, 2), None);
        assert_eq!(pool_space(&pools, 3), None, "no such pool");
    }

    /// A stand-in hyperstor-vhost: answers each request line with what
    /// `answer` makes of it, and records the requests.
    fn fake_vhost(
        dir: &Path,
        answer: impl Fn(&serde_json::Value) -> serde_json::Value + Send + 'static,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
        let l = UnixListener::bind(dir.join("control.sock")).unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(mut c) = c else { return };
                let mut line = String::new();
                BufReader::new(c.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                let reply = answer(&req);
                s2.lock().unwrap().push(req);
                writeln!(c, "{reply}").unwrap();
            }
        });
        seen
    }

    #[test]
    fn staging_attaches_through_hyperstor_vhost_and_preempts_only_when_told() {
        let tmp = tempfile::tempdir().unwrap();
        let seen = fake_vhost(tmp.path(), |r| match r["op"].as_str().unwrap() {
            "hello" => serde_json::json!({"id": 1, "ok": {"protocol": 1}}),
            // Held by a tool: always.
            "attach" if r["args"]["volume"] == "00000000000000000000000000000bad" => {
                serde_json::json!({"id": 1, "error": {
                    "code": "held", "message": "attached by 1", "holder": "hyperstorctl"}})
            }
            // Held by another node's hyperstor-vhost, until preempted.
            "attach"
                if r["args"]["volume"] == "000000000000000000000000000000b0"
                    && r["args"]["take"] == "attach" =>
            {
                serde_json::json!({"id": 1, "error": {
                    "code": "held", "message": "attached by 2", "holder": "vhost:node-b"}})
            }
            "list" => serde_json::json!({"id": 1, "ok": {"disks": [
                {"volume": "000000000000000000000000000000b0"},
                {"volume": "ffff"},
            ]}}),
            "attach" => serde_json::json!({"id": 1, "ok": {
                "socket": format!("/run/hyperstor/vhost/{}.sock", r["args"]["volume"].as_str().unwrap()),
                "size": 1}}),
            "detach" => {
                serde_json::json!({"id": 1, "error": {"code": "not_attached", "message": "volume x isn't attached here"}})
            }
            _ => serde_json::json!({"id": 1, "error": {"code": "invalid", "message": "?"}}),
        });
        let drv = HyperstorDriver::new(
            vec!["127.0.0.1:1".into()],
            "m:1".into(),
            1,
            "ds".into(),
            None,
        )
        .with_paths(tmp.path().to_path_buf(), PathBuf::from("/nonexistent"))
        .on_node("node-a");
        drv.vhost_answers().unwrap();
        let d = disk("6f1c0d2e-9a4b-4c1d-8e2f-0123456789ab");
        let b = drv.stage(&d).unwrap();
        assert_eq!(
            b.vhost_socket(),
            Some("/run/hyperstor/vhost/6f1c0d2e9a4b4c1d8e2f0123456789ab.sock")
        );
        drv.attach_for_vm(&d, true).unwrap();
        // Held by a tool: busy, not a failure of the disk, even for a VM
        // starting.
        let held = disk("00000000-0000-0000-0000-000000000bad");
        assert!(matches!(drv.stage(&held), Err(ImageError::Busy(_))));
        assert!(matches!(
            drv.attach_for_vm(&held, false),
            Err(ImageError::Busy(_))
        ));
        // Held by another node's hyperstor-vhost: busy to stage, but a VM
        // starting here preempts the stale hold (S26).
        let stale = disk("00000000-0000-0000-0000-0000000000b0");
        assert!(matches!(drv.stage(&stale), Err(ImageError::Busy(_))));
        drv.attach_for_vm(&stale, false).unwrap();
        // Detaching one not attached here is fine.
        drv.unstage(&d).unwrap();
        assert_eq!(
            drv.attached_volumes().unwrap(),
            ["000000000000000000000000000000b0", "ffff"]
        );
        let reqs = seen.lock().unwrap();
        let takes: Vec<&str> = reqs
            .iter()
            .filter(|r| r["op"] == "attach")
            .map(|r| r["args"]["take"].as_str().unwrap())
            .collect();
        assert_eq!(
            takes,
            [
                "attach", "preempt", "attach", "attach", "attach", "attach", "preempt"
            ]
        );
        assert_eq!(reqs[1]["args"]["serial"], "6f1c0d2e9a4b4c1d8e2f");
        // Every attach names this node, for whoever finds the volume held.
        assert!(
            reqs.iter()
                .filter(|r| r["op"] == "attach")
                .all(|r| r["args"]["holder"] == "node-a")
        );
        // Without hyperstor-vhost: not ready (the node can't run it now).
        let none = HyperstorDriver::new(vec![], "m:1".into(), 1, "ds".into(), None)
            .with_paths(tmp.path().join("absent"), PathBuf::from("/nonexistent"));
        assert!(matches!(none.stage(&d), Err(ImageError::NotReady(_))));
        // Without the identity (an agent): API calls aren't made.
        assert!(matches!(none.api_answers(), Err(ImageError::NotReady(_))));
    }

    /// Which holds are stale (S26): another node's hyperstor-vhost's for a
    /// VM starting here; and, on the node holding them, those of disks
    /// whose VM is placed elsewhere or nowhere.
    #[test]
    fn stale_holds_are_another_nodes_vhost_and_disks_placed_elsewhere() {
        assert!(stale_hold("vhost:node-b", "node-a"));
        assert!(!stale_hold("vhost:node-a", "node-a"), "this node's own");
        assert!(!stale_hold("vhost", "node-a"), "unnamed");
        assert!(!stale_hold("vhost:", "node-a"));
        assert!(!stale_hold("hyperstorctl", "node-a"), "a tool's");
        assert!(!stale_hold("", "node-a"));
        let attached: Vec<String> = ["v1", "v2", "v3", "v4"].map(String::from).to_vec();
        let disks = vec![
            ("v1".to_string(), Some("node-a".to_string())),
            ("v2".to_string(), Some("node-b".to_string())),
            ("v3".to_string(), None),
        ];
        assert_eq!(stale_here(&attached, &disks, "node-a"), ["v2", "v3"]);
    }

    /// The partition code's reads and writes of a volume go to hyperstorctl
    /// with the volume, the range and the map service; a held volume is
    /// busy.
    #[test]
    fn regions_go_through_hyperstorctl() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("args");
        let ctl = tmp.path().join("hyperstorctl");
        std::fs::write(
            &ctl,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$*\" in *read-volume*) shift 11; head -c 8 /dev/zero > \"$1\";; esac\ncase \"$*\" in *busy*) echo 'the volume is attached by client instance 1' >&2; exit 1;; esac\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let vol = "0123456789abcdef0123456789abcdef";
        let p = PathBuf::from(tool_path("m:1", vol));
        let out = tmp.path().join("out");
        read_region_with(&ctl, &p, 4096, 8192, &out).unwrap();
        write_region_with(&ctl, &p, &out, 512, false).unwrap();
        let busy = PathBuf::from(tool_path("busy", vol));
        assert!(matches!(
            write_region_with(&ctl, &busy, &out, 0, true),
            Err(ImageError::Busy(_))
        ));
        let args = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        assert_eq!(
            lines[0],
            format!(
                "--map m:1 read-volume --id {vol} --offset 4096 --size 8192 --out {}",
                out.display()
            )
        );
        assert_eq!(
            lines[1],
            format!(
                "--map m:1 write-volume --id {vol} --offset 512 --file {}",
                out.display()
            )
        );
        assert!(lines[2].ends_with("--sparse"), "{}", lines[2]);
    }
}

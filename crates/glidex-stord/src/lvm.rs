//! LVM-thin operations (spec/storage.md §11.1), run through [`Exec`] so
//! tests can stand in for the host. Names are checked by the caller
//! ([`crate::names`]); this module only builds argv (never a shell string).

use crate::proto::{Activated, PoolStatus, ProbeResult, StordError, Volume};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One finished command.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// What stord does to the host: commands, and handing a device node to
/// the glidex user.
pub trait Exec: Send + Sync {
    fn run(&self, prog: &str, args: &[&str]) -> Output;
    /// Make `dev` (a device node) `uid:gid 0600`.
    fn own(&self, dev: &Path, uid: u32, gid: u32) -> Result<(), String>;
    /// Where a `/dev/<vg>/<lv>` link points.
    fn resolve(&self, path: &Path) -> Result<PathBuf, String>;
}

pub struct SystemExec;

impl Exec for SystemExec {
    fn run(&self, prog: &str, args: &[&str]) -> Output {
        match std::process::Command::new(prog)
            .args(args)
            .env("LC_ALL", "C")
            // stord's own sockets are open; LVM warns about every inherited fd.
            .env("LVM_SUPPRESS_FD_WARNINGS", "1")
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(o) => Output {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            },
            Err(e) => Output {
                ok: false,
                stdout: String::new(),
                stderr: format!("{prog}: {e}"),
            },
        }
    }

    fn own(&self, dev: &Path, uid: u32, gid: u32) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        nix::unistd::chown(
            dev,
            Some(nix::unistd::Uid::from_raw(uid)),
            Some(nix::unistd::Gid::from_raw(gid)),
        )
        .map_err(|e| format!("{}: {e}", dev.display()))?;
        std::fs::set_permissions(dev, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", dev.display()))
    }

    fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
        std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

fn failed(what: &str, o: &Output) -> StordError {
    let msg = o.stderr.trim();
    StordError::Failed(format!(
        "{what}: {}",
        if msg.is_empty() { "failed" } else { msg }
    ))
}

/// Run an LVM command; its stderr becomes the error.
fn lvm(ex: &dyn Exec, what: &str, args: &[&str]) -> Result<Output, StordError> {
    let o = ex.run("lvm", args);
    if o.ok { Ok(o) } else { Err(failed(what, &o)) }
}

/// The rows of a `--reportformat json` report (`report[0].<key>`).
fn rows(out: &str, key: &str) -> Result<Vec<Value>, StordError> {
    let v: Value = serde_json::from_str(out)
        .map_err(|e| StordError::Failed(format!("unreadable LVM report: {e}")))?;
    Ok(v["report"][0][key].as_array().cloned().unwrap_or_default())
}

fn num(v: &Value) -> u64 {
    v.as_str()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| v.as_u64())
        .unwrap_or(0)
}

fn pct(v: &Value) -> f64 {
    v.as_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

pub fn probe(ex: &dyn Exec) -> ProbeResult {
    let v = ex.run("lvm", &["version"]);
    ProbeResult {
        lvm: if v.ok {
            v.stdout.lines().next().unwrap_or("").trim().to_string()
        } else {
            String::new()
        },
        thin_tools: ex.run("thin_check", &["-V"]).ok,
    }
}

const LV_FIELDS: &str = "lv_name,lv_size,origin,lv_attr,data_percent,metadata_percent,pool_lv";

/// Every LV of `vg` (all of them; the caller filters).
fn lvs(ex: &dyn Exec, vg: &str) -> Result<Vec<Value>, StordError> {
    let o = ex.run(
        "lvm",
        &[
            "lvs",
            "--reportformat",
            "json",
            "--units",
            "b",
            "--nosuffix",
            "-o",
            LV_FIELDS,
            vg,
        ],
    );
    if !o.ok {
        return Err(if o.stderr.contains("not found") {
            StordError::NotFound(format!("volume group {vg} not found"))
        } else {
            failed("lvs", &o)
        });
    }
    rows(&o.stdout, "lv")
}

/// `lv_attr` letters: type, permissions, …, state (5th), open (6th),
/// …, activation skip (10th).
fn attr(row: &Value, i: usize) -> char {
    row["lv_attr"]
        .as_str()
        .and_then(|a| a.chars().nth(i))
        .unwrap_or('-')
}

fn volume_of(row: &Value) -> Volume {
    let size = num(&row["lv_size"]);
    let origin = row["origin"].as_str().filter(|s| !s.is_empty());
    Volume {
        name: row["lv_name"].as_str().unwrap_or("").to_string(),
        size_bytes: size,
        origin: origin.map(str::to_string),
        active: attr(row, 4) == 'a',
        open: attr(row, 5) == 'o',
        read_only: attr(row, 1) == 'r',
        used_bytes: (size as f64 * pct(&row["data_percent"]) / 100.0) as u64,
    }
}

pub fn pool_status(ex: &dyn Exec, vg: &str, pool: &str) -> PoolStatus {
    let no = |reason: &str, message: String| PoolStatus {
        reason: reason.into(),
        message,
        ..Default::default()
    };
    let all = match lvs(ex, vg) {
        Ok(a) => a,
        Err(StordError::NotFound(m)) => return no("NoVolumeGroup", m),
        Err(e) => return no("NoVolumeGroup", e.to_string()),
    };
    let Some(row) = all
        .iter()
        .find(|r| r["lv_name"] == pool && attr(r, 0) == 't')
    else {
        return no("NoThinPool", format!("{vg} has no thin pool {pool}"));
    };
    let size = num(&row["lv_size"]);
    PoolStatus {
        usable: true,
        size_bytes: size,
        used_bytes: (size as f64 * pct(&row["data_percent"]) / 100.0) as u64,
        metadata_percent: pct(&row["metadata_percent"]),
        ..Default::default()
    }
}

/// glidex's volumes in `vg` (`gx-d-…`, `gx-i-…`).
pub fn list_volumes(ex: &dyn Exec, vg: &str) -> Result<Vec<Volume>, StordError> {
    Ok(lvs(ex, vg)?
        .iter()
        .map(volume_of)
        .filter(|v| crate::names::volume(&v.name).is_ok())
        .collect())
}

fn find(ex: &dyn Exec, vg: &str, name: &str) -> Result<Volume, StordError> {
    list_volumes(ex, vg)?
        .into_iter()
        .find(|v| v.name == name)
        .ok_or_else(|| StordError::NotFound(format!("no volume {vg}/{name}")))
}

fn bytes_arg(size: u64) -> String {
    format!("{size}b")
}

pub fn create_volume(
    ex: &dyn Exec,
    vg: &str,
    pool: &str,
    name: &str,
    size: u64,
) -> Result<(), StordError> {
    if size == 0 {
        return Err(StordError::Invalid("a volume needs a size".into()));
    }
    lvm(
        ex,
        "lvcreate",
        &[
            "lvcreate",
            "-q",
            "--type",
            "thin",
            "-V",
            &bytes_arg(size),
            "--thinpool",
            pool,
            "--setactivationskip",
            "n",
            "-n",
            name,
            vg,
        ],
    )
    .map(|_| ())
}

/// A thin snapshot of image volume `origin`, never skipped at activation
/// (§19, H4), then grown when `size` is larger than the origin.
pub fn snapshot_volume(
    ex: &dyn Exec,
    vg: &str,
    origin: &str,
    name: &str,
    size: u64,
) -> Result<(), StordError> {
    let o = find(ex, vg, origin)?;
    if !o.read_only {
        return Err(StordError::Invalid(format!(
            "{origin} is not sealed: an image volume is read-only before it is cloned"
        )));
    }
    lvm(
        ex,
        "lvcreate",
        &[
            "lvcreate",
            "-q",
            "-s",
            "--setactivationskip",
            "n",
            "-n",
            name,
            &format!("{vg}/{origin}"),
        ],
    )?;
    // A snapshot of a read-only origin is read-only too until told.
    lvm(
        ex,
        "lvchange",
        &["lvchange", "-q", "-p", "rw", &format!("{vg}/{name}")],
    )
    .ok();
    if size > o.size_bytes {
        extend_volume(ex, vg, name, size)?;
    }
    Ok(())
}

pub fn seal_volume(ex: &dyn Exec, vg: &str, name: &str) -> Result<(), StordError> {
    find(ex, vg, name)?;
    lvm(
        ex,
        "lvchange",
        &["lvchange", "-q", "-p", "r", &format!("{vg}/{name}")],
    )
    .map(|_| ())
}

pub fn extend_volume(ex: &dyn Exec, vg: &str, name: &str, size: u64) -> Result<(), StordError> {
    let v = find(ex, vg, name)?;
    if size < v.size_bytes {
        return Err(StordError::Invalid(format!(
            "{name} is {} bytes: volumes never shrink",
            v.size_bytes
        )));
    }
    if size == v.size_bytes {
        return Ok(());
    }
    lvm(
        ex,
        "lvextend",
        &[
            "lvextend",
            "-q",
            "-L",
            &bytes_arg(size),
            &format!("{vg}/{name}"),
        ],
    )
    .map(|_| ())
}

/// Activate and hand to `uid:gid` (every activation makes a fresh
/// `root:disk` node, §19 H5).
pub fn activate(
    ex: &dyn Exec,
    vg: &str,
    name: &str,
    uid: u32,
    gid: u32,
) -> Result<Activated, StordError> {
    let v = find(ex, vg, name)?;
    lvm(
        ex,
        "lvchange",
        &["lvchange", "-q", "-ay", "-K", &format!("{vg}/{name}")],
    )?;
    let path = PathBuf::from(format!("/dev/{vg}/{name}"));
    let device = ex.resolve(&path).map_err(StordError::Failed)?;
    ex.own(&device, uid, gid).map_err(StordError::Failed)?;
    Ok(Activated {
        path: path.to_string_lossy().into_owned(),
        device: device.to_string_lossy().into_owned(),
        size_bytes: v.size_bytes,
    })
}

pub fn deactivate(ex: &dyn Exec, vg: &str, name: &str) -> Result<(), StordError> {
    let v = find(ex, vg, name)?;
    if v.open {
        return Err(StordError::Busy(format!("{name} is open")));
    }
    lvm(
        ex,
        "lvchange",
        &["lvchange", "-q", "-an", &format!("{vg}/{name}")],
    )
    .map(|_| ())
}

pub fn delete_volume(ex: &dyn Exec, vg: &str, name: &str) -> Result<(), StordError> {
    let all = list_volumes(ex, vg)?;
    let Some(v) = all.iter().find(|v| v.name == name) else {
        // Already gone: deleting is idempotent.
        return Ok(());
    };
    if v.open {
        return Err(StordError::Busy(format!("{name} is open")));
    }
    let users: Vec<&str> = all
        .iter()
        .filter(|o| o.origin.as_deref() == Some(name))
        .map(|o| o.name.as_str())
        .collect();
    if !users.is_empty() {
        return Err(StordError::Busy(format!(
            "{name} is the origin of {}",
            users.join(", ")
        )));
    }
    lvm(
        ex,
        "lvremove",
        &["lvremove", "-q", "-y", &format!("{vg}/{name}")],
    )
    .map(|_| ())
}

#[cfg(test)]
pub mod fake {
    //! A host in memory: `lvm` commands against a table of LVs.

    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    pub struct Lv {
        pub size: u64,
        pub origin: String,
        pub thin_pool: bool,
        pub read_only: bool,
        pub active: bool,
        pub open: bool,
    }

    #[derive(Default)]
    pub struct Fake {
        pub vgs: Mutex<BTreeMap<String, BTreeMap<String, Lv>>>,
        pub calls: Mutex<Vec<String>>,
        pub owned: Mutex<Vec<(PathBuf, u32, u32)>>,
    }

    impl Fake {
        pub fn with_pool(vg: &str, pool: &str, size: u64) -> Self {
            let f = Fake::default();
            f.vgs.lock().unwrap().entry(vg.into()).or_default().insert(
                pool.into(),
                Lv {
                    size,
                    origin: String::new(),
                    thin_pool: true,
                    read_only: false,
                    active: true,
                    open: false,
                },
            );
            f
        }

        pub fn lv(&self, vg: &str, name: &str) -> Option<Lv> {
            self.vgs.lock().unwrap().get(vg)?.get(name).cloned()
        }

        pub fn set_open(&self, vg: &str, name: &str, open: bool) {
            if let Some(l) = self
                .vgs
                .lock()
                .unwrap()
                .get_mut(vg)
                .and_then(|v| v.get_mut(name))
            {
                l.open = open;
            }
        }
    }

    fn size_of(s: &str) -> u64 {
        s.trim_end_matches('b').parse().unwrap()
    }

    fn split(t: &str) -> (String, String) {
        let (a, b) = t.split_once('/').unwrap();
        (a.into(), b.into())
    }

    impl Exec for Fake {
        fn run(&self, prog: &str, args: &[&str]) -> Output {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{prog} {}", args.join(" ")));
            let ok = |stdout: String| Output {
                ok: true,
                stdout,
                stderr: String::new(),
            };
            let err = |m: &str| Output {
                ok: false,
                stdout: String::new(),
                stderr: m.to_string(),
            };
            if prog != "lvm" {
                return ok(String::new());
            }
            let flag = |f: &str| args.iter().position(|a| *a == f).map(|i| args[i + 1]);
            let mut vgs = self.vgs.lock().unwrap();
            match args[0] {
                "version" => ok("  LVM version:     2.03.16(2)\n".into()),
                "lvs" => {
                    let vg = args.last().unwrap();
                    let Some(lvs) = vgs.get(*vg) else {
                        return err(&format!("  Volume group \"{vg}\" not found"));
                    };
                    let rows: Vec<Value> = lvs
                        .iter()
                        .map(|(n, l)| {
                            let attr = format!(
                                "{}{}i-{}{}t{}--",
                                if l.thin_pool { 't' } else { 'V' },
                                if l.read_only { 'r' } else { 'w' },
                                if l.active { 'a' } else { '-' },
                                if l.open { 'o' } else { '-' },
                                'z'
                            );
                            serde_json::json!({
                                "lv_name": n, "lv_size": l.size.to_string(), "origin": l.origin,
                                "lv_attr": attr, "data_percent": "50.00", "metadata_percent": "10.00",
                                "pool_lv": "",
                            })
                        })
                        .collect();
                    ok(serde_json::json!({ "report": [{ "lv": rows }] }).to_string())
                }
                "lvcreate" => {
                    let snap = args.contains(&"-s");
                    let name = flag("-n").unwrap().to_string();
                    let target = args.last().unwrap();
                    let (vg, size, origin) = if snap {
                        let (vg, origin) = split(target);
                        let size = vgs[&vg][&origin].size;
                        (vg, size, origin)
                    } else {
                        (
                            target.to_string(),
                            size_of(flag("-V").unwrap()),
                            String::new(),
                        )
                    };
                    let lvs = vgs.get_mut(&vg).unwrap();
                    if lvs.contains_key(&name) {
                        return err("  Logical Volume already exists");
                    }
                    lvs.insert(
                        name,
                        Lv {
                            size,
                            origin,
                            thin_pool: false,
                            read_only: snap,
                            active: true,
                            open: false,
                        },
                    );
                    ok(String::new())
                }
                "lvchange" | "lvextend" | "lvremove" => {
                    let (vg, name) = split(args.last().unwrap());
                    let Some(lvs) = vgs.get_mut(&vg) else {
                        return err("no vg");
                    };
                    if args[0] == "lvremove" {
                        lvs.remove(&name);
                        return ok(String::new());
                    }
                    let Some(l) = lvs.get_mut(&name) else {
                        return err("no lv");
                    };
                    match args[0] {
                        "lvextend" => l.size = size_of(flag("-L").unwrap()),
                        _ => {
                            if let Some(p) = flag("-p") {
                                l.read_only = p == "r";
                            }
                            if args.contains(&"-ay") {
                                l.active = true;
                            }
                            if args.contains(&"-an") {
                                l.active = false;
                            }
                        }
                    }
                    ok(String::new())
                }
                other => err(&format!("unexpected lvm {other}")),
            }
        }

        fn own(&self, dev: &Path, uid: u32, gid: u32) -> Result<(), String> {
            self.owned.lock().unwrap().push((dev.into(), uid, gid));
            Ok(())
        }

        fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
            Ok(PathBuf::from(format!(
                "/dev/dm-{}",
                path.to_string_lossy().len()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Fake;
    use super::*;

    const G: u64 = 1 << 30;

    #[test]
    fn volumes_live_in_the_pool_and_snapshots_grow_past_their_image() {
        let f = Fake::with_pool("vg0", "pool", 10 * G);
        let st = pool_status(&f, "vg0", "pool");
        assert!(st.usable, "{st:?}");
        assert_eq!(st.size_bytes, 10 * G);
        assert_eq!(pool_status(&f, "vg0", "other").reason, "NoThinPool");
        assert_eq!(pool_status(&f, "nope", "pool").reason, "NoVolumeGroup");

        create_volume(&f, "vg0", "pool", "gx-i-img", 2 * G).unwrap();
        // An image is sealed before it is cloned.
        assert!(matches!(
            snapshot_volume(&f, "vg0", "gx-i-img", "gx-d-1", 4 * G),
            Err(StordError::Invalid(_))
        ));
        seal_volume(&f, "vg0", "gx-i-img").unwrap();
        snapshot_volume(&f, "vg0", "gx-i-img", "gx-d-1", 4 * G).unwrap();
        let d = f.lv("vg0", "gx-d-1").unwrap();
        assert_eq!(
            (d.size, d.origin.as_str(), d.read_only),
            (4 * G, "gx-i-img", false)
        );
        let calls = f.calls.lock().unwrap().join("\n");
        assert!(
            calls.contains("lvcreate -q -s --setactivationskip n -n gx-d-1 vg0/gx-i-img"),
            "{calls}"
        );

        // The pool itself isn't one of glidex's volumes.
        let names: Vec<String> = list_volumes(&f, "vg0")
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(names, ["gx-d-1", "gx-i-img"]);
    }

    #[test]
    fn activation_hands_the_device_to_glidex() {
        let f = Fake::with_pool("vg0", "pool", 10 * G);
        create_volume(&f, "vg0", "pool", "gx-d-1", G).unwrap();
        let a = activate(&f, "vg0", "gx-d-1", 996, 985).unwrap();
        assert_eq!(a.path, "/dev/vg0/gx-d-1");
        assert_eq!(a.size_bytes, G);
        let owned = f.owned.lock().unwrap().clone();
        assert_eq!(owned, vec![(PathBuf::from(&a.device), 996, 985)]);
        assert!(
            f.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == "lvm lvchange -q -ay -K vg0/gx-d-1")
        );
    }

    #[test]
    fn volumes_never_shrink_and_busy_ones_stay() {
        let f = Fake::with_pool("vg0", "pool", 10 * G);
        create_volume(&f, "vg0", "pool", "gx-i-img", G).unwrap();
        seal_volume(&f, "vg0", "gx-i-img").unwrap();
        snapshot_volume(&f, "vg0", "gx-i-img", "gx-d-1", G).unwrap();
        extend_volume(&f, "vg0", "gx-d-1", 2 * G).unwrap();
        assert_eq!(f.lv("vg0", "gx-d-1").unwrap().size, 2 * G);
        assert!(matches!(
            extend_volume(&f, "vg0", "gx-d-1", G),
            Err(StordError::Invalid(_))
        ));
        // The image stays while a disk is a snapshot of it.
        assert!(matches!(
            delete_volume(&f, "vg0", "gx-i-img"),
            Err(StordError::Busy(_))
        ));
        f.set_open("vg0", "gx-d-1", true);
        assert!(matches!(
            delete_volume(&f, "vg0", "gx-d-1"),
            Err(StordError::Busy(_))
        ));
        assert!(matches!(
            deactivate(&f, "vg0", "gx-d-1"),
            Err(StordError::Busy(_))
        ));
        f.set_open("vg0", "gx-d-1", false);
        delete_volume(&f, "vg0", "gx-d-1").unwrap();
        delete_volume(&f, "vg0", "gx-d-1").unwrap();
        delete_volume(&f, "vg0", "gx-i-img").unwrap();
        assert!(list_volumes(&f, "vg0").unwrap().is_empty());
    }
}

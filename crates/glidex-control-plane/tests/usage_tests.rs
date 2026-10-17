//! The usage family on its RPCs and REST twins (spec/grpc.md §9.1, G2;
//! spec/metering.md §9): the same answers over REST, Connect and gRPC,
//! project scoping, and the CSV exports REST keeps.

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use buffa::Message;
use glidex_api::proto::glidex::usage::v1 as pb;
use glidex_control_plane::api::{self, App, AppState};
use glidex_control_plane::auth::{AuthService, store::TokenKind};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
use glidex_control_plane::metering::{Origin, Subject, SubjectKind};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

struct H {
    router: Router,
    app: AppState,
    _dir: TempDir,
}

fn harness() -> H {
    let dir = TempDir::new().unwrap();
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run")) };
    let manager = common::with_db_path(dir.path().join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    manager.start_metering(&cfg.metering).unwrap();
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    H {
        router: api::router(app.clone()),
        app,
        _dir: dir,
    }
}

impl H {
    fn user(&self, name: &str) -> String {
        self.app
            .auth
            .store
            .user_for_identity("pam", name, name, None, true)
            .unwrap()
            .unwrap()
            .id
    }

    fn token(&self, user: &str) -> String {
        self.app
            .auth
            .create_token(
                "t",
                TokenKind::Personal { owner: user.into() },
                user,
                None,
                None,
                None,
            )
            .unwrap()
            .0
    }

    fn link(&self, role: &str, user: &str, on: Ent) {
        self.app
            .auth
            .add_link(role, Ent::User(user.into()), on, "test")
            .unwrap();
    }

    fn admin(&self, name: &str) -> String {
        let id = self.user(name);
        self.link("role.system-admin", &id, Ent::Cluster);
        self.app
            .auth
            .make_administrator(&Ent::User(id.clone()), "test")
            .unwrap();
        id
    }

    fn project(&self, name: &str) -> String {
        self.app
            .manager
            .projects()
            .create(name, String::new(), None)
            .unwrap()
            .id
    }

    /// Status, headers and body bytes.
    async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let b = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, headers, bytes.to_vec())
    }

    async fn rest(&self, path: &str, token: &str) -> (StatusCode, Value) {
        let (s, h, b) = self.raw("GET", path, None, token).await;
        if s == StatusCode::OK {
            assert_eq!(h["glidex-api-shape"], "proto", "{path}");
        }
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    /// The Connect protocol with JSON: what curl or a script sends.
    async fn connect(&self, method: &str, body: Value, token: &str) -> (StatusCode, Value) {
        let req = Request::post(format!("/{method}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// gRPC with a binary message; returns grpc-status and the reply.
    async fn grpc<M: Message>(&self, path: &str, msg: &M, token: &str) -> (String, Vec<u8>) {
        let bytes = msg.encode_to_vec();
        let mut body = vec![0u8];
        body.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(&bytes);
        let req = Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/grpc")
            .body(Body::from(body))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let headers = resp.headers().clone();
        let collected = resp.into_body().collect().await.unwrap();
        let status = collected
            .trailers()
            .and_then(|t| t.get("grpc-status"))
            .or_else(|| headers.get("grpc-status"))
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let b = collected.to_bytes();
        let msg = if b.len() >= 5 {
            b[5..5 + u32::from_be_bytes([b[1], b[2], b[3], b[4]]) as usize].to_vec()
        } else {
            Vec::new()
        };
        (status, msg)
    }

    /// A VM in `project` (its desired state only; nothing runs).
    async fn vm(&self, name: &str, project: &str, token: &str) -> String {
        let (s, _, b) = self
            .raw(
                "POST",
                "/vms",
                Some(json!({
                    "name": name, "project": project, "vcpu_count": 1, "mem_size_mib": 128,
                    "kernel_image_path": "/tmp/vmlinux", "rootfs_path": "/tmp/rootfs.ext4",
                })),
                token,
            )
            .await;
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["vm"]["id"].as_str().unwrap().to_string()
    }

    /// An hour of CPU on `vm`, and 10 Mbit/s for 10 minutes on its NIC and
    /// 100 read IOPS on a disk of it, in the last 15 minutes.
    fn record(&self, project: &str, vm: &str, disk: &str) {
        let meter = self.app.manager.meter().unwrap();
        let now = glidex_control_plane::metering::now_ms();
        let mut r = meter.ledger().begin_round(now).unwrap();
        let s = Subject::new(SubjectKind::Vm, vm, "web", Some(project.into()));
        r.counter(&s, "cpu.used", "i1", 0, now - 60_000, Origin::Unknown)
            .unwrap();
        r.counter(
            &s,
            "cpu.used",
            "i1",
            3_600_000_000,
            now - 30_000,
            Origin::Unknown,
        )
        .unwrap();
        meter.ledger().commit(r).unwrap();
        let t0 = now / 300_000 * 300_000 - 900_000;
        let mut r = meter.ledger().begin_round(t0 + 600_000).unwrap();
        let mut nic = Subject::new(
            SubjectKind::Nic,
            format!("{vm}.0"),
            "web/nic0",
            Some(project.into()),
        );
        nic.vm_id = Some(vm.to_string());
        let mut d = Subject::new(SubjectKind::Disk, disk, "data", Some(project.into()));
        d.vm_id = Some(vm.to_string());
        for (s, m) in [(&nic, "net.rx_bytes"), (&d, "disk.read_ops")] {
            r.counter(s, m, "port", 0, t0, Origin::Unknown).unwrap();
            for i in 1..=20u64 {
                let v = if m == "disk.read_ops" {
                    i * 3_000
                } else {
                    i * 37_500_000
                };
                r.counter(s, m, "port", v, t0 + i * 30_000, Origin::Unknown)
                    .unwrap();
            }
        }
        meter.ledger().commit(r).unwrap();
    }
}

const USAGE: &str = "glidex.usage.v1.UsageService";
const METRICS: &str = "glidex.usage.v1.MetricsService";

#[tokio::test(flavor = "multi_thread")]
async fn usage_over_rest_connect_and_grpc() {
    let h = harness();
    let t = h.token(&h.admin("ada"));
    let p = h.project("p");
    let vm = h.vm("web", &p, &t).await;
    h.record(&p, &vm, "disk-1");

    // GET /usage and GetUsage give the same answer.
    let (s, rest) = h
        .rest("/usage?project=p&granularity=month&group_by=project,vm", &t)
        .await;
    assert_eq!(s, StatusCode::OK, "{rest}");
    assert_eq!(rest["granularity"], "GRANULARITY_MONTH");
    assert_eq!(
        rest["group_by"],
        json!(["GROUP_KEY_PROJECT", "GROUP_KEY_VM"])
    );
    let row = &rest["rows"][0];
    assert_eq!(row["project"], json!({"id": p, "name": "p"}), "{rest}");
    assert_eq!(row["vm"]["id"], vm.as_str());
    assert_eq!(
        row["meters"]["cpu.used"],
        json!({"raw": "3600000000", "value": 1.0, "unit": "core-hours"})
    );
    for k in ["from", "to", "complete_through", "metering_started_at"] {
        assert!(rest[k].as_str().unwrap().ends_with('Z'), "{k}: {rest}");
    }
    let (s, rpc) = h
        .connect(
            &format!("{USAGE}/GetUsage"),
            json!({"projects": ["p"], "query": {"granularity": "GRANULARITY_MONTH",
                   "group_by": ["GROUP_KEY_PROJECT", "GROUP_KEY_VM"]}}),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{rpc}");
    assert_eq!(rpc, rest);

    // The per-subject shortcuts, over gRPC.
    let (st, b) = h
        .grpc(
            &format!("/{USAGE}/GetVmUsage"),
            &pb::GetVmUsageRequest {
                id: vm.clone(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "0");
    let r = pb::GetVmUsageResponse::decode_from_slice(&b).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].vm.as_option().unwrap().id, vm);
    assert_eq!(r.rows[0].meters["cpu.used"].raw, Some(3_600_000_000));
    let (st, b) = h
        .grpc(
            &format!("/{USAGE}/GetProjectUsage"),
            &pb::GetProjectUsageRequest {
                id: "p".into(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "0");
    let r = pb::GetProjectUsageResponse::decode_from_slice(&b).unwrap();
    assert_eq!(r.rows[0].project.as_option().unwrap().id, p);
    let (s, v) = h.rest("/disks/disk-1/usage", &t).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "no such disk: {v}");

    // CSV stays REST, as before.
    let (s, hd, b) = h
        .raw(
            "GET",
            "/usage?project=p&granularity=month&format=csv",
            None,
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        hd[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    let csv = String::from_utf8(b).unwrap();
    assert!(
        csv.starts_with("start,end,project_id,project_name,meter,raw,value,unit,flags\n"),
        "{csv}"
    );
    assert!(
        csv.contains(&format!(",{p},p,cpu.used,3600000000,1,core-hours,")),
        "{csv}"
    );

    // Bad queries.
    let (s, v) = h
        .rest(
            "/usage?from=2020-01-01T00:00:00Z&to=2026-01-01T00:00:00Z",
            &t,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("range_too_large"))
    );
    let (s, v) = h.rest("/usage?group_by=vms", &t).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid"))
    );
    let (s, v) = h
        .connect(
            &format!("{USAGE}/GetUsage"),
            json!({"query": {"from": "2020-01-01T00:00:00Z", "to": "2026-01-01T00:00:00Z"}}),
            &t,
        )
        .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_argument")),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_series_and_stats() {
    let h = harness();
    let t = h.token(&h.admin("ada"));
    let p = h.project("p");
    let vm = h.vm("web", &p, &t).await;
    h.record(&p, &vm, "disk-1");

    // Bandwidth report: REST and Connect agree.
    let (s, rest) = h.rest("/usage/bandwidth?project=p&group_by=vm", &t).await;
    assert_eq!(s, StatusCode::OK, "{rest}");
    assert_eq!(rest["final"], false);
    assert_eq!(rest["from_final_figures"], false);
    assert!(rest["month"].as_str().unwrap().len() == 7, "{rest}");
    let row = &rest["rows"][0];
    assert_eq!(row["vm"]["id"], vm.as_str(), "{rest}");
    assert_eq!(row["p95"]["rx_mbps"], 10.0, "{rest}");
    assert!(
        row["p95"].get("slots").is_none(),
        "slots are the row's: {rest}"
    );
    assert_eq!(row["slots"], json!({"counted": 2, "interpolated": 0}));
    assert_eq!(row["peak"]["resolution_secs"], 30, "{rest}");
    let (s, rpc) = h
        .connect(
            &format!("{USAGE}/GetBandwidthReport"),
            json!({"projects": ["p"], "query": {"group_by": ["GROUP_KEY_VM"]}}),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{rpc}");
    assert_eq!(rpc, rest);

    // Disk I/O: latency source by name; CSV keeps the old words and columns.
    let (s, rest) = h.rest("/usage/disk-io?project=p&group_by=disk", &t).await;
    assert_eq!(s, StatusCode::OK, "{rest}");
    let row = &rest["rows"][0];
    assert_eq!(row["disk"]["id"], "disk-1", "{rest}");
    assert_eq!(row["latency_source"], "LATENCY_SOURCE_NONE");
    assert_eq!(row["p95"]["read_iops"], 100.0, "{rest}");
    let (s, _, b) = h
        .raw(
            "GET",
            "/usage/disk-io?project=p&group_by=disk&format=csv",
            None,
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let csv = String::from_utf8(b).unwrap();
    let (head, line) = csv.split_once('\n').unwrap();
    let cols: Vec<&str> = head.split(',').collect();
    let vals: Vec<&str> = line.trim_end().split(',').collect();
    let col = |c: &str| vals[cols.iter().position(|x| *x == c).expect(c)];
    assert_eq!(col("latency_source"), "none", "{csv}");
    assert_eq!(col("p95.read_iops"), "100.0", "{csv}");
    assert_eq!(col("slots.counted"), "2", "{csv}");
    assert_eq!(col("disk.id"), "disk-1", "{csv}");
    let (s, v) = h
        .connect(
            &format!("{USAGE}/GetDiskIoReport"),
            json!({"query": {"group_by": ["GROUP_KEY_NIC"]}}),
            &t,
        )
        .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_argument")),
        "{v}"
    );

    // Compute over gRPC.
    let (st, b) = h
        .grpc(
            &format!("/{USAGE}/GetComputeReport"),
            &pb::GetComputeReportRequest {
                projects: vec![p.clone()],
                query: pb::ReportQuery {
                    group_by: vec![pb::GroupKey::GROUP_KEY_VM.into()],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "0");
    let r = pb::GetComputeReportResponse::decode_from_slice(&b).unwrap();
    assert_eq!(r.rows[0].vm.as_option().unwrap().id, vm);
    assert!(r.rows[0].p95.as_option().unwrap().cpu_cores.unwrap() > 0.0);
    assert_eq!(r.rows[0].p95.as_option().unwrap().cpu_percent, None);

    // The 5-minute series, with their p95.
    let (s, rest) = h.rest(&format!("/vms/{vm}/bandwidth?p95=true"), &t).await;
    assert_eq!(s, StatusCode::OK, "{rest}");
    let points = rest["points"].as_array().unwrap();
    assert_eq!(points.len(), 2, "{rest}");
    assert_eq!(points[0]["rx_mbps"], 10.0, "{rest}");
    assert!(points[0]["slot"].as_str().unwrap().ends_with('Z'));
    assert_eq!(rest["p95"]["rx_mbps"], 10.0, "{rest}");
    assert_eq!(rest["p95"]["slots"]["counted"], 2, "{rest}");
    let (s, rpc) = h
        .connect(
            &format!("{METRICS}/GetVmBandwidth"),
            json!({"id": vm, "query": {"p95": true}}),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{rpc}");
    assert_eq!(rpc, rest);
    let (s, rest) = h.rest(&format!("/vms/{vm}/io"), &t).await;
    assert_eq!(s, StatusCode::OK, "{rest}");
    assert_eq!(rest["points"][0]["read_iops"], 100.0, "{rest}");
    assert!(rest.get("p95").is_none(), "only with p95=true: {rest}");
    let (st, b) = h
        .grpc(
            &format!("/{METRICS}/GetVmCompute"),
            &pb::GetVmComputeRequest {
                id: vm.clone(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "0");
    let r = pb::GetVmComputeResponse::decode_from_slice(&b).unwrap();
    assert!(r.from.is_set() && r.to.is_set());

    // Live rates: nothing sampled here.
    let (s, rest) = h.rest(&format!("/vms/{vm}/stats"), &t).await;
    assert_eq!(s, StatusCode::OK, "{rest}");
    assert_eq!(rest, json!({"resolution_secs": 30}));
    let (s, rpc) = h
        .connect(&format!("{METRICS}/GetVmStats"), json!({"id": vm}), &t)
        .await;
    assert_eq!((s, &rpc), (StatusCode::OK, &rest));
    let (st, _) = h
        .grpc(
            &format!("/{METRICS}/GetVmStats"),
            &pb::GetVmStatsRequest {
                id: "nope".into(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "5", "not found");
    let (s, _) = h.rest("/networks/nope/stats", &t).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn rpcs_are_project_scoped() {
    let h = harness();
    let admin = h.token(&h.admin("ada"));
    let (pa, pb_) = (h.project("pa"), h.project("pb"));
    let vm_b = h.vm("web-b", &pb_, &admin).await;
    let vm_a = h.vm("web-a", &pa, &admin).await;
    h.record(&pa, &vm_a, "disk-a");
    h.record(&pb_, &vm_b, "disk-b");
    let (viewer, nobody) = (h.user("viewer"), h.user("nobody"));
    h.link("role.viewer", &viewer, Ent::Project(pa.clone()));
    let (v, n) = (h.token(&viewer), h.token(&nobody));

    // Without projects: only the caller's own.
    let (s, r) = h.connect(&format!("{USAGE}/GetUsage"), json!({}), &v).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let ids: Vec<&str> = r["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["project"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![pa.as_str()]);
    // Another project: not found, by every route.
    for (m, body) in [
        ("GetUsage", json!({"projects": ["pb"]})),
        ("GetProjectUsage", json!({"id": "pb"})),
        ("GetBandwidthReport", json!({"projects": ["pb"]})),
        ("GetComputeReport", json!({"projects": ["pb"]})),
    ] {
        let (s, r) = h.connect(&format!("{USAGE}/{m}"), body, &v).await;
        assert_eq!(
            (s, r["code"].as_str()),
            (StatusCode::NOT_FOUND, Some("not_found")),
            "{m}: {r}"
        );
    }
    let (st, _) = h
        .grpc(
            &format!("/{METRICS}/GetVmBandwidth"),
            &pb::GetVmBandwidthRequest {
                id: vm_b.clone(),
                ..Default::default()
            },
            &v,
        )
        .await;
    assert_eq!(st, "5");
    let (st, _) = h
        .grpc(
            &format!("/{METRICS}/GetVmBandwidth"),
            &pb::GetVmBandwidthRequest {
                id: vm_a.clone(),
                ..Default::default()
            },
            &v,
        )
        .await;
    assert_eq!(st, "0");
    // No links at all: forbidden.
    let (st, _) = h
        .grpc(
            &format!("/{USAGE}/GetUsage"),
            &pb::GetUsageRequest::default(),
            &n,
        )
        .await;
    assert_eq!(st, "7", "permission denied");
}

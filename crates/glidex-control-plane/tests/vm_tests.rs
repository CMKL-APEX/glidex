//! The VM family on its RPCs and REST twins (spec/grpc.md §9.1, G2):
//! proto-shaped answers, `UpdateVm` with a field mask next to the old
//! merge patch, versions, and deletes.

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use buffa::Message;
use glidex_api::proto::glidex::vm::v1 as pb;
use glidex_control_plane::api::{self, App, AppState};
use glidex_control_plane::auth::{AuthService, store::TokenKind};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
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
    cfg.auth.allowed_origins = vec![ORIGIN.into()];
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    H {
        router: api::router(app.clone()),
        app,
        _dir: dir,
    }
}

const ORIGIN: &str = "https://localhost:5173";

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

    /// A system administrator as bootstrap or migration makes one.
    fn admin(&self, name: &str) -> String {
        let id = self.user(name);
        self.app
            .auth
            .add_link(
                "role.system-admin",
                Ent::User(id.clone()),
                Ent::Cluster,
                "test",
            )
            .unwrap();
        self.app
            .auth
            .make_administrator(&Ent::User(id.clone()), "test")
            .unwrap();
        id
    }

    async fn rest(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: &str,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
        let b = match token
            .strip_prefix("session:")
            .and_then(|s| s.split_once('|'))
        {
            Some((cookie, csrf)) => b
                .header(
                    header::COOKIE,
                    format!("{}={}", glidex_control_plane::auth::SESSION_COOKIE, cookie),
                )
                .header(glidex_control_plane::auth::CSRF_HEADER, csrf)
                .header(header::ORIGIN, ORIGIN),
            None => b.header(header::AUTHORIZATION, format!("Bearer {token}")),
        };
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
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
}

fn vm_body(name: &str, project: &str) -> Value {
    json!({
        "name": name, "project": project, "vcpu_count": 1, "mem_size_mib": 128,
        "kernel_image_path": "/tmp/vmlinux", "rootfs_path": "/tmp/rootfs.ext4",
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn vms_over_rest_in_the_proto_shape() {
    let h = harness();
    let t = h.token(&h.admin("ada"));
    let p = h
        .app
        .manager
        .projects()
        .create("p", String::new(), None)
        .unwrap()
        .id;

    // The old enum values are refused; the answer is proto.
    let mut b = vm_body("web", &p);
    b["restart_policy"] = json!("never");
    let (st, v) = h.rest("POST", "/vms", Some(b.clone()), &t).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid")),
        "{v}"
    );
    b["hypervisor"] = json!("HYPERVISOR_CLOUD_HYPERVISOR");
    b["restart_policy"] = json!("RESTART_POLICY_NEVER");
    let (st, v) = h.rest("POST", "/vms", Some(b), &t).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let vm = &v["vm"];
    let id = vm["id"].as_str().unwrap().to_string();
    assert_eq!(vm["state"], "VM_STATE_CREATED", "{v}");
    assert_eq!(vm["desired_state"], "POWER_STATE_STOPPED");
    assert_eq!(vm["hypervisor"], "HYPERVISOR_CLOUD_HYPERVISOR");
    assert_eq!(vm["restart_policy"], "RESTART_POLICY_NEVER");
    assert_eq!(vm["generation"], "1", "64-bit numbers are strings");
    let (_, v) = h.rest("GET", "/vms", None, &t).await;
    assert_eq!(v["vms"][0]["id"], json!(id));

    // PATCH, the new shape: a change and its mask.
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/vms/{id}"),
            Some(json!({"change": {"vcpu_count": 2, "stop_grace_secs": 30}, "update_mask": "vcpu_count"})),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["vm"]["vcpu_count"], 2);
    assert!(
        v["vm"].get("stop_grace_secs").is_none(),
        "not in the mask: {v}"
    );
    // Without a mask, the keys of `change`.
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/vms/{id}"),
            Some(json!({"change": {"stop_grace_secs": 30}})),
            &t,
        )
        .await;
    assert_eq!(
        (st, v["vm"]["stop_grace_secs"].as_u64()),
        (StatusCode::ACCEPTED, Some(30)),
        "{v}"
    );
    // The merge patch from before the port is not read any more.
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/vms/{id}"),
            Some(json!({"config": {"mem_size_mib": 256}})),
            &t,
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid")),
        "{v}"
    );
    // A field the change can't touch.
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/vms/{id}"),
            Some(json!({"change": {}, "update_mask": "rootfs_path"})),
            &t,
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_config")),
        "{v}"
    );

    // Events and console info.
    let (st, v) = h.rest("GET", &format!("/vms/{id}/events"), None, &t).await;
    assert_eq!(st, StatusCode::OK);
    assert!(v["events"][0]["at"].as_str().unwrap().ends_with('Z'), "{v}");
    let (_, v) = h.rest("GET", &format!("/vms/{id}/console"), None, &t).await;
    assert_eq!(
        v["websocket"],
        json!(format!("/vms/{id}/console/ws")),
        "{v}"
    );

    // A never-started VM goes at once.
    let (st, _) = h.rest("DELETE", &format!("/vms/{id}"), None, &t).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn vm_service_over_connect_and_grpc() {
    let h = harness();
    let admin = h.admin("ada");
    let t = h.token(&admin);
    let p = h
        .app
        .manager
        .projects()
        .create("p", String::new(), None)
        .unwrap()
        .id;

    let req = pb::CreateVmRequest {
        name: "g".into(),
        project: Some(p.clone()),
        vcpu_count: 1,
        mem_size_mib: 128,
        kernel_image_path: "/tmp/vmlinux".into(),
        rootfs_path: "/tmp/rootfs.ext4".into(),
        ..Default::default()
    };
    let (st, bytes) = h.grpc("/glidex.vm.v1.VmService/CreateVm", &req, &t).await;
    assert_eq!(st, "0");
    let vm = pb::CreateVmResponse::decode_from_slice(&bytes)
        .unwrap()
        .vm
        .as_option()
        .unwrap()
        .clone();
    assert_eq!(vm.state.as_known(), Some(pb::VmState::VM_STATE_CREATED));

    // UpdateVm with a stale version is refused; with the current one it goes.
    let stale = pb::UpdateVmRequest {
        id: vm.id.clone(),
        change: pb::VmChange {
            vcpu_count: 2,
            ..Default::default()
        }
        .into(),
        update_mask: buffa_types::google::protobuf::FieldMask {
            paths: vec!["vcpu_count".into()],
            ..Default::default()
        }
        .into(),
        expected_resource_version: Some(vm.resource_version + 5),
        ..Default::default()
    };
    let (st, _) = h.grpc("/glidex.vm.v1.VmService/UpdateVm", &stale, &t).await;
    assert_ne!(st, "0", "a stale version");
    let mut ok = stale.clone();
    ok.expected_resource_version = Some(vm.resource_version);
    let (st, bytes) = h.grpc("/glidex.vm.v1.VmService/UpdateVm", &ok, &t).await;
    assert_eq!(st, "0");
    let changed = pb::UpdateVmResponse::decode_from_slice(&bytes).unwrap();
    assert_eq!(changed.vm.as_option().unwrap().vcpu_count, 2);

    // Connect JSON: list, get, console ticket.
    let (st, v) = h
        .connect("glidex.vm.v1.VmService/ListVms", json!({"project": p}), &t)
        .await;
    assert_eq!(
        (st, v["vms"][0]["name"].as_str()),
        (StatusCode::OK, Some("g")),
        "{v}"
    );
    let (st, v) = h
        .connect("glidex.vm.v1.VmService/GetVm", json!({"id": vm.id}), &t)
        .await;
    assert_eq!(
        (st, v["vm"]["vcpu_count"].as_u64()),
        (StatusCode::OK, Some(2)),
        "{v}"
    );
    let (st, v) = h
        .connect(
            "glidex.vm.v1.VmService/CreateConsoleTicket",
            json!({"id": vm.id}),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["ticket"].as_str().is_some_and(|t| !t.is_empty()));
    let (st, v) = h
        .connect("glidex.vm.v1.VmService/GetVm", json!({"id": "nope"}), &t)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");

    // Someone without a role on the project sees nothing.
    let bob = h.user("bob");
    let (st, _) = h
        .connect(
            "glidex.vm.v1.VmService/GetVm",
            json!({"id": vm.id}),
            &h.token(&bob),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    h.app
        .auth
        .add_link(
            "role.viewer",
            Ent::User(bob.clone()),
            Ent::Project(p.clone()),
            "test",
        )
        .unwrap();
    let (st, _) = h
        .connect(
            "glidex.vm.v1.VmService/GetVm",
            json!({"id": vm.id}),
            &h.token(&bob),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = h
        .connect(
            "glidex.vm.v1.VmService/StartVm",
            json!({"id": vm.id}),
            &h.token(&bob),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "a viewer can't start it");

    // Delete: gone at once, so no VM in the answer.
    let (st, bytes) = h
        .grpc(
            "/glidex.vm.v1.VmService/DeleteVm",
            &pb::DeleteVmRequest {
                id: vm.id.clone(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "0");
    assert!(
        pb::DeleteVmResponse::decode_from_slice(&bytes)
            .unwrap()
            .vm
            .as_option()
            .is_none()
    );
}

/// Hugepages asked for are counted in their size on a standalone host too
/// (spec/host-topology.md §8a.3); a bad size is refused.
#[tokio::test(flavor = "multi_thread")]
async fn hugepage_sizes_are_checked_and_recorded_with_the_placement() {
    let h = harness();
    let t = h.token(&h.admin("ada"));
    let p = h
        .app
        .manager
        .projects()
        .create("p", String::new(), None)
        .unwrap()
        .id;
    let body = |name: &str, extra: Value| {
        let mut b = vm_body(name, &p);
        b["hypervisor"] = json!("HYPERVISOR_CLOUD_HYPERVISOR");
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    for (extra, why) in [
        (
            json!({"hugepages": true, "hugepage_size": "3M"}),
            "use 2M or 1G",
        ),
        (
            json!({"hugepage_size": "2M"}),
            "hugepage_size needs hugepages",
        ),
        (
            json!({"hugepages": true, "hugepage_size": "1G", "mem_size_mib": 1536}),
            "multiple of 1024 MiB",
        ),
    ] {
        let (st, v) = h.rest("POST", "/vms", Some(body("bad", extra)), &t).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert!(v.to_string().contains(why), "{why}: {v}");
    }

    let (st, v) = h
        .rest(
            "POST",
            "/vms",
            Some(body(
                "huge",
                json!({"hugepages": true, "hugepage_size": "2M"}),
            )),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let vm = &v["vm"];
    assert_eq!(
        (&vm["hugepages"], &vm["hugepage_size"], &vm["page_kib"]),
        (&json!(true), &json!("2M"), &json!("2048")),
        "{v}"
    );
    let id = vm["id"].as_str().unwrap().to_string();

    // Ordinary memory: nothing counted ahead; the node decides at start.
    let (st, v) = h
        .rest("POST", "/vms", Some(body("plain", json!({}))), &t)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert!(v["vm"].get("page_kib").is_none(), "{v}");

    // A change of size is checked against the memory, and counted again.
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/vms/{id}"),
            Some(json!({"change": {"hugepage_size": "1G"}, "update_mask": "hugepage_size"})),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/vms/{id}"),
            Some(
                json!({"change": {"hugepage_size": "1G", "mem_size_mib": 1024},
                        "update_mask": "hugepage_size,mem_size_mib"}),
            ),
            &t,
        )
        .await;
    assert!(st.is_success(), "{st} {v}");
    assert_eq!(v["vm"]["page_kib"], json!("1048576"), "{v}");
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/vms/{id}"),
            Some(json!({"change": {"hugepages": false, "hugepage_size": ""},
                        "update_mask": "hugepages,hugepage_size"})),
            &t,
        )
        .await;
    assert!(st.is_success(), "{st} {v}");
    assert!(v["vm"].get("page_kib").is_none(), "{v}");
    assert!(v["vm"].get("hugepages").is_none(), "{v}");
}

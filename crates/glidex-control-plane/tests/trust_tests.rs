//! Trust between organizations (spec/tenancy.md §12.1, D63–D66): two
//! control planes in one process, each serving its API over HTTPS on a
//! port of its own — one the cluster, the other the tenant's directory —
//! redeeming an offer, refusing what they should, and ending trust.

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use glidex_control_plane::api::{self, App, AppState};
use glidex_control_plane::auth::{self, AuthService};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::{Config, TlsConfig, TlsSetting};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "https://localhost:5173";

struct Cp {
    router: Router,
    app: AppState,
    base: String,
    _dir: TempDir,
}

/// A control plane serving its API on `https://127.0.0.1:<port>`, with a
/// certificate of its own; `issuer_base` and `cluster.api_url` are that.
async fn cp() -> Cp {
    let dir = TempDir::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    );
    let (cert, key) = glidex_tls::generate(&glidex_tls::LocalNames::default()).unwrap();
    let cert_path = dir.path().join("api.crt");
    std::fs::write(&cert_path, &cert).unwrap();
    let manager = common::with_db_path(dir.path().join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    cfg.auth.allowed_origins = vec![ORIGIN.into()];
    cfg.auth.secrets_key = Some(dir.path().join("secrets.kek"));
    cfg.auth.issuer_base = Some(base.clone());
    cfg.cluster.api_url = Some(base.clone());
    cfg.tls = TlsSetting::Files(TlsConfig {
        cert: cert_path,
        key: None,
    });
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    let router = api::router(app.clone());
    // HTTPS for the other control plane's calls.
    use rustls_pki_types::pem::PemObject as _;
    let certs = vec![rustls_pki_types::CertificateDer::from_pem_slice(cert.as_bytes()).unwrap()];
    let key = rustls_pki_types::PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap();
    let tls = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let served = router.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, router) = (acceptor.clone(), served.clone());
            tokio::spawn(async move {
                let Ok(s) = acceptor.accept(tcp).await else {
                    return;
                };
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        router.clone().oneshot(req.map(Body::new))
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(s), svc)
                    .await;
            });
        }
    });
    Cp {
        router,
        app,
        base,
        _dir: dir,
    }
}

impl Cp {
    /// A fresh browser session of a system administrator.
    fn admin(&self) -> String {
        let id = self
            .app
            .auth
            .store
            .user_for_identity("pam", "root", "root", None, true)
            .unwrap()
            .unwrap()
            .id;
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
        let u = self.app.auth.store.user(&id).unwrap().unwrap();
        let (cookie, csrf) = self.app.auth.create_session(&u, auth::Method::Pam).unwrap();
        format!("{cookie}|{csrf}")
    }

    async fn rest(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        session: &str,
    ) -> (StatusCode, Value) {
        let (cookie, csrf) = session.split_once('|').unwrap();
        let b = Request::builder()
            .method(method)
            .uri(path)
            .header(
                header::COOKIE,
                format!("{}={}", auth::SESSION_COOKIE, cookie),
            )
            .header(auth::CSRF_HEADER, csrf)
            .header(header::ORIGIN, ORIGIN);
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let s = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

/// D63–D66 end to end: an offer redeemed once, by its tenant at its
/// directory; both sides' records; what is refused; opting out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_offer_is_redeemed_between_two_control_planes() {
    let north = cp().await; // the cluster
    let acme = cp().await; // the tenant's directory
    let op = north.admin();
    let admin = acme.admin();
    let (s, v) = acme
        .rest(
            "POST",
            "/tenants",
            Some(json!({"slug": "acme", "display_name": "Acme"})),
            &admin,
        )
        .await;
    assert!(s.is_success(), "{v}");
    let tenant = v["tenant"]["id"].as_str().unwrap().to_string();
    // Its administrator opts it in (`manageTenantClusters` on the tenant).
    let root = acme
        .app
        .auth
        .store
        .user_for_identity("pam", "root", "root", None, false)
        .unwrap()
        .unwrap()
        .id;
    acme.app
        .auth
        .add_link(
            "role.tenant-admin",
            Ent::User(root),
            Ent::Tenant(tenant.clone()),
            "test",
        )
        .unwrap();
    let issuer = format!("{}/d/local", acme.base);
    let offer = |tenant: &'static str, directory: String| {
        let (north, op) = (&north, op.clone());
        async move {
            let (s, v) = north
                .rest(
                    "POST",
                    "/admissions/offers",
                    Some(json!({"tenant": tenant, "directory": directory})),
                    &op,
                )
                .await;
            assert_eq!(s, StatusCode::CREATED, "{v}");
            v["token"].as_str().unwrap().to_string()
        }
    };
    let opt_in = |token: String| {
        let (acme, admin) = (&acme, admin.clone());
        async move {
            acme.rest(
                "POST",
                "/tenants/acme/clusters",
                Some(json!({"offer": token})),
                &admin,
            )
            .await
        }
    };

    // Refused by the directory: another tenant's, another directory's.
    let (s, v) = opt_in(offer("beta", issuer.clone()).await).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("offer_refused")),
        "{v}"
    );
    let (s, _) = opt_in(offer("acme", "https://other.example/d/x".into()).await).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Redeemed.
    let token = offer("acme", issuer.clone()).await;
    let (s, v) = opt_in(token.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["cluster"]["api"], north.base.as_str(), "{v}");
    let (_, v) = acme
        .rest("GET", "/tenants/acme/clusters", None, &admin)
        .await;
    assert_eq!(v["clusters"].as_array().unwrap().len(), 1, "{v}");
    // The cluster's side: the directory trusted, the tenant admitted.
    let (_, v) = north
        .rest("GET", "/admissions/directories", None, &op)
        .await;
    assert_eq!(v["directories"][0]["issuer"], issuer.as_str(), "{v}");
    assert_eq!(
        v["directories"][0]["tenants"][0]["id"],
        tenant.as_str(),
        "{v}"
    );
    let (_, v) = north.rest("GET", "/admissions", None, &op).await;
    let a = v["admissions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["tenant_id"] == tenant.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (a["tenant_slug"].as_str(), a["status"].as_str()),
        (Some("acme"), Some("ADMISSION_STATUS_ADMITTED")),
        "{v}"
    );
    // The directory's issuer is fixed now, as the cluster pinned it.
    let d = acme
        .app
        .auth
        .tenants
        .get_directory("local")
        .unwrap()
        .unwrap();
    assert_eq!(d.issuer.as_deref(), Some(issuer.as_str()));

    // Twice: refused by the cluster.
    let (s, v) = opt_in(token).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("offer_refused")),
        "{v}"
    );
    // A redeem without the directory's signature: refused, the same way.
    let t = glidex_control_plane::trust::OfferToken::decode(&offer("acme", issuer.clone()).await)
        .unwrap();
    let r = reqwest_free_post(
        &north.base,
        &t.tls_sha256,
        &t.offer,
        &t.secret,
        &issuer,
        &tenant,
    )
    .await;
    assert_eq!(r, 403, "an unsigned redeem");
    // Expired (at the cluster): the record says so even if the token doesn't.
    let token = offer("acme", issuer.clone()).await;
    let t = glidex_control_plane::trust::OfferToken::decode(&token).unwrap();
    let mut o = north
        .app
        .auth
        .tenants
        .offers()
        .unwrap()
        .into_iter()
        .find(|o| o.id == t.offer)
        .unwrap();
    o.expires_at = 1;
    north.app.auth.tenants.put_offer(&o).unwrap();
    let (s, _) = opt_in(token).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Opting out ends it on both sides.
    let cluster = glidex_control_plane::api::op::cluster_id(&north.app);
    let (s, v) = acme
        .rest(
            "DELETE",
            &format!("/tenants/acme/clusters/{cluster}"),
            None,
            &admin,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let (_, v) = acme
        .rest("GET", "/tenants/acme/clusters", None, &admin)
        .await;
    assert!(v["clusters"].as_array().is_none_or(|a| a.is_empty()), "{v}");
    let (_, v) = north
        .rest("GET", "/admissions/directories", None, &op)
        .await;
    assert!(
        v["directories"].as_array().is_none_or(|a| a.is_empty()),
        "the cluster was told: {v}"
    );
    let (_, v) = north.rest("GET", "/admissions", None, &op).await;
    assert!(!v.to_string().contains(&tenant), "{v}");
}

/// A redeem with the offer's secret but no assertion (as a thief of the
/// token, not the directory, would send it).
async fn reqwest_free_post(
    base: &str,
    pin: &str,
    offer: &str,
    secret: &str,
    issuer: &str,
    tenant: &str,
) -> u16 {
    let body = json!({
        "offer": offer, "secret": secret, "issuer": issuer, "directory": "local",
        "tenant": {"id": tenant, "slug": "acme"}, "jwks": {"keys": []},
        "api": base, "tls_sha256": pin,
    });
    // `post` always signs; an empty assertion stands for none.
    glidex_control_plane::directory::peer::post(base, pin, "/trust/v1/redeem", "", &body)
        .await
        .unwrap()
        .0
}

/// D67–D69: after the handshake, the cluster pulls the directory's feed:
/// its admitted tenant's users, projects and roles appear in the replica,
/// a member's access token from the directory works at the cluster, a
/// non-member gets none, and disabling the user reaches the cluster.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_feed_brings_a_trusted_directorys_people_to_the_cluster() {
    let north = cp().await;
    let acme = cp().await;
    let op = north.admin();
    let admin = acme.admin();
    let (_, v) = acme
        .rest(
            "POST",
            "/tenants",
            Some(json!({"slug": "acme", "display_name": "Acme"})),
            &admin,
        )
        .await;
    let tenant = v["tenant"]["id"].as_str().unwrap().to_string();
    let store = &acme.app.auth.store;
    let root = store
        .user_for_identity("pam", "root", "root", None, false)
        .unwrap()
        .unwrap()
        .id;
    acme.app
        .auth
        .add_link(
            "role.tenant-admin",
            Ent::User(root.clone()),
            Ent::Tenant(tenant.clone()),
            "test",
        )
        .unwrap();
    // Erin works in acme; Bob is in the directory but not in acme.
    let erin = store
        .user_for_identity("pam", "erin", "Erin", None, true)
        .unwrap()
        .unwrap()
        .id;
    let bob = store
        .user_for_identity("pam", "bob", "Bob", None, true)
        .unwrap()
        .unwrap()
        .id;
    acme.app
        .auth
        .add_link(
            "role.tenant-admin",
            Ent::User(erin.clone()),
            Ent::Tenant(tenant.clone()),
            "test",
        )
        .unwrap();
    let (s, v) = acme
        .rest(
            "POST",
            "/projects",
            Some(json!({"name": "web", "tenant": "acme"})),
            &admin,
        )
        .await;
    assert!(s.is_success(), "{v}");
    let project = v["project"]["id"].as_str().unwrap().to_string();
    // She owns it: a project link the feed carries too.
    acme.app
        .auth
        .add_link(
            "role.owner",
            Ent::User(erin.clone()),
            Ent::Project(project.clone()),
            "test",
        )
        .unwrap();

    let issuer = format!("{}/d/local", acme.base);
    let (_, v) = north
        .rest(
            "POST",
            "/admissions/offers",
            Some(json!({"tenant": "acme", "directory": issuer})),
            &op,
        )
        .await;
    let (s, v) = acme
        .rest(
            "POST",
            "/tenants/acme/clusters",
            Some(json!({"offer": v["token"]})),
            &admin,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let north_id = glidex_control_plane::api::op::cluster_id(&north.app);

    // Erin's tokens for north, from her directory.
    let token_for = |user: String| {
        let acme = &acme;
        let north_id = north_id.clone();
        async move {
            let grant = glidex_control_plane::directory::op::Grant {
                user_id: user,
                auth_time: glidex_control_plane::auth::store::now(),
                amr: vec!["pwd".into()],
                mfa: false,
            };
            let (refresh, _) = acme
                .app
                .auth
                .refresh_tokens
                .start(
                    "local",
                    "gxctl",
                    "openid",
                    &grant,
                    86_400,
                    glidex_control_plane::auth::store::now(),
                )
                .unwrap();
            let body = format!(
                "grant_type=refresh_token&client_id=gxctl&refresh_token={refresh}&resource=urn:glidex:cluster:{north_id}"
            );
            let resp = acme
                .router
                .clone()
                .oneshot(
                    Request::post("/d/local/token")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };
    let v = token_for(bob.clone()).await;
    assert_eq!(v["error"], "invalid_target", "bob isn't in acme: {v}");
    let v = token_for(erin.clone()).await;
    let access = v["access_token"]
        .as_str()
        .expect("an access token for north")
        .to_string();
    let whoami = |tok: String| {
        let north = &north;
        async move {
            let resp = north
                .router
                .clone()
                .oneshot(
                    Request::get("/auth/whoami")
                        .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let st = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (
                st,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    // Before a feed, north doesn't know her.
    assert_eq!(whoami(access.clone()).await.0, StatusCode::UNAUTHORIZED);

    let d = north
        .app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .unwrap()
        .unwrap();
    assert!(
        glidex_control_plane::api::trust::pull_feed(&north.app, &d)
            .await
            .unwrap()
    );
    assert!(
        !glidex_control_plane::api::trust::pull_feed(&north.app, &d)
            .await
            .unwrap(),
        "304 the second time"
    );
    let u = north
        .app
        .auth
        .store
        .user(&erin)
        .unwrap()
        .expect("erin in the replica");
    assert_eq!(
        (u.directory_id.as_str(), u.display_name.as_str()),
        (d.id.as_str(), "Erin")
    );
    assert!(
        north.app.auth.store.user(&bob).unwrap().is_none(),
        "only acme's people"
    );
    assert!(
        u.email.is_none() && u.managed.is_none(),
        "cut to what a cluster needs"
    );

    let (s, me) = whoami(access.clone()).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(
        (me["user"]["id"].as_str(), me["method"].as_str()),
        (Some(erin.as_str()), Some("directory")),
        "{me}"
    );
    // Her role holds at north: acme's project is hers to see.
    let resp = north
        .router
        .clone()
        .oneshot(
            Request::get("/projects")
                .header(header::AUTHORIZATION, format!("Bearer {access}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let projects: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(projects.to_string().contains(&project), "{projects}");

    // Disabled at her directory: refused at north after the next pull.
    let mut eu = store.user(&erin).unwrap().unwrap();
    eu.disabled = true;
    store.put_user(&eu).unwrap();
    assert!(
        glidex_control_plane::api::trust::pull_feed(&north.app, &d)
            .await
            .unwrap()
    );
    assert_eq!(whoami(access).await.0, StatusCode::UNAUTHORIZED);
}

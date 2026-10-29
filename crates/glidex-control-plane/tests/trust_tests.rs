//! Trust between organizations (spec/19-tenancy.md §12.1, D63–D66): two
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

/// Two organizations after the handshake: north, a cluster, admitting
/// tenant acme of directory acme (its `local`). Erin is acme's tenant
/// admin and owns its project web; Bob is in the directory but not in
/// acme. No feed pulled yet.
struct Admitted {
    north: Cp,
    acme: Cp,
    erin: String,
    bob: String,
    project: String,
    issuer: String,
}

async fn admitted() -> Admitted {
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
    Admitted {
        north,
        acme,
        erin,
        bob,
        project,
        issuer,
    }
}

/// D67–D69: after the handshake, the cluster pulls the directory's feed:
/// its admitted tenant's users, projects and roles appear in the replica,
/// a member's access token from the directory works at the cluster, a
/// non-member gets none, and disabling the user reaches the cluster.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_feed_brings_a_trusted_directorys_people_to_the_cluster() {
    let Admitted {
        north,
        acme,
        erin,
        bob,
        project,
        issuer,
    } = admitted().await;
    let store = &acme.app.auth.store;
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
                session: None,
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

/// A GET with `cookies`: its status, `Location` and `Set-Cookie`s.
async fn browse(
    router: &Router,
    path: &str,
    cookies: &[String],
) -> (StatusCode, String, Vec<String>) {
    let mut b = Request::get(path).header(header::ORIGIN, ORIGIN);
    if !cookies.is_empty() {
        b = b.header(header::COOKIE, cookies.join("; "));
    }
    let resp = router
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let set = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect();
    (resp.status(), location, set)
}

/// The path and query of `url`, as the browser would ask its host.
fn path_of(url: &str) -> String {
    let u = url::Url::parse(url).unwrap();
    format!("{}?{}", u.path(), u.query().unwrap_or(""))
}

/// The same, for the API behind the web UI's `/api`.
fn ui_path(url: &str) -> String {
    path_of(url).strip_prefix("/api").unwrap().to_string()
}

fn query_of(url: &str, key: &str) -> Option<String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// D69's browser login: north's login page offers acme's directory; the
/// code flow goes there and back; the code is exchanged with north's
/// client assertion; the cluster session is Erin's and ends when she logs
/// out at her directory (the next feed carries the chain's end). Bob, not
/// in acme, is refused at his directory; a wrong callback, a missing
/// client assertion and another browser's callback are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_browser_logs_in_to_the_cluster_at_its_directory() {
    let Admitted {
        north,
        acme,
        erin,
        bob,
        issuer,
        ..
    } = admitted().await;
    let d = north
        .app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .unwrap()
        .unwrap();
    glidex_control_plane::api::trust::pull_feed(&north.app, &d)
        .await
        .unwrap();
    let c = acme
        .app
        .auth
        .trust
        .clusters("local")
        .unwrap()
        .pop()
        .unwrap();
    let callback = format!("{ORIGIN}/api/auth/t/{}/callback", d.id);
    assert_eq!(
        c.ui_callback.as_deref(),
        Some(callback.as_str()),
        "registered at the redeem"
    );

    // The login page.
    let resp = north
        .router
        .clone()
        .oneshot(Request::get("/auth/methods").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let m: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(m["trusted_directories"][0]["id"], d.id.as_str(), "{m}");
    assert_eq!(m["trusted_directories"][0]["tenants"][0], "Acme", "{m}");

    // A session at acme, as each user's browser has after logging in there.
    let at_acme = |user: &str| {
        let u = acme.app.auth.store.user(user).unwrap().unwrap();
        let (cookie, csrf) = acme.app.auth.create_session(&u, auth::Method::Pam).unwrap();
        (
            format!("{}={cookie}", auth::SESSION_COOKIE),
            format!("{cookie}|{csrf}"),
        )
    };
    let start = |browser: &'static str| {
        let north = &north;
        let d = d.clone();
        async move {
            let (s, to, set) = browse(
                &north.router,
                &format!("/auth/t/{}/start?return_to=/vms", d.id),
                &[],
            )
            .await;
            assert_eq!(s, StatusCode::SEE_OTHER, "{browser}");
            assert!(to.starts_with(&format!("{}/authorize?", d.issuer)), "{to}");
            (
                to,
                set.into_iter()
                    .find(|c| c.starts_with("gx_tlogin="))
                    .unwrap(),
            )
        }
    };

    // Bob isn't in acme: his directory says no.
    let (to, _) = start("bob").await;
    let (bob_cookie, _) = at_acme(&bob);
    let (s, back, _) = browse(&acme.router, &path_of(&to), &[bob_cookie]).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(back.starts_with(&callback), "{back}");
    assert_eq!(query_of(&back, "error").as_deref(), Some("access_denied"));

    // Only the registered callback.
    let (erin_cookie, erin_session) = at_acme(&erin);
    let wrong = to.replace(
        &url::form_urlencoded::byte_serialize(callback.as_bytes()).collect::<String>(),
        "https%3A%2F%2Fevil.example%2Fcb",
    );
    assert_ne!(wrong, to);
    let (s, _, _) = browse(
        &acme.router,
        &path_of(&wrong),
        std::slice::from_ref(&erin_cookie),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "another redirect_uri");

    // Erin: there and back.
    let (to, tlogin) = start("erin").await;
    let (s, back, _) = browse(
        &acme.router,
        &path_of(&to),
        std::slice::from_ref(&erin_cookie),
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let code = query_of(&back, "code").expect(&back);
    // Without north's client assertion, the code is no good to anyone.
    let resp = acme
        .router
        .clone()
        .oneshot(
            Request::post("/d/local/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=authorization_code&client_id=cluster:{}&code=x&redirect_uri=x",
                    c.cluster_id
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "no client assertion"
    );
    // Another browser (no cookie) can't finish it.
    let (s, _, _) = browse(&north.router, &ui_path(&back), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "another browser");
    // The state was spent by that try: start again.
    let (to, tlogin2) = start("erin again").await;
    assert_ne!(tlogin, tlogin2);
    let (_, back, _) = browse(
        &acme.router,
        &path_of(&to),
        std::slice::from_ref(&erin_cookie),
    )
    .await;
    assert_ne!(query_of(&back, "code"), Some(code));
    let (s, home, set) = browse(&north.router, &ui_path(&back), &[tlogin2]).await;
    assert_eq!(s, StatusCode::SEE_OTHER, "{home}");
    assert_eq!(home, "/vms");
    let session = set
        .into_iter()
        .find(|c| c.starts_with(&format!("{}=", auth::SESSION_COOKIE)))
        .expect("a session at north");
    let whoami = |cookie: String| {
        let north = &north;
        async move {
            let resp = north
                .router
                .clone()
                .oneshot(
                    Request::get("/auth/whoami")
                        .header(header::COOKIE, cookie)
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
    let (s, me) = whoami(session.clone()).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(
        (me["user"]["id"].as_str(), me["method"].as_str()),
        (Some(erin.as_str()), Some("directory")),
        "{me}"
    );

    // She logs out at acme: the chain ends, and with the next feed her
    // session at north.
    let (s, _) = acme.rest("POST", "/auth/logout", None, &erin_session).await;
    assert!(s.is_success());
    assert!(
        glidex_control_plane::api::trust::pull_feed(&north.app, &d)
            .await
            .unwrap(),
        "the ended chain is news"
    );
    assert_eq!(whoami(session).await.0, StatusCode::UNAUTHORIZED);
}

/// Erin's browser logs in to north at acme (D69): north's session cookie
/// (`gx_session=…`) and its secret.
async fn erin_at_north(
    north: &Cp,
    acme: &Cp,
    erin: &str,
    d: &glidex_control_plane::trust::TrustedDirectory,
) -> (String, String) {
    let u = acme.app.auth.store.user(erin).unwrap().unwrap();
    let (cookie, _) = acme.app.auth.create_session(&u, auth::Method::Pam).unwrap();
    let at_acme = format!("{}={cookie}", auth::SESSION_COOKIE);
    let (_, to, set) = browse(
        &north.router,
        &format!("/auth/t/{}/start?return_to=/vms", d.id),
        &[],
    )
    .await;
    let tlogin = set
        .into_iter()
        .find(|c| c.starts_with("gx_tlogin="))
        .unwrap();
    let (s, back, _) = browse(&acme.router, &path_of(&to), &[at_acme]).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(query_of(&back, "code").is_some(), "{back}");
    let (s, _, set) = browse(&north.router, &ui_path(&back), &[tlogin]).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let session = set
        .into_iter()
        .find(|c| c.starts_with(&format!("{}=", auth::SESSION_COOKIE)))
        .expect("a session at north");
    let secret = session.split_once('=').unwrap().1.to_string();
    (session, secret)
}

/// D72: logging out at the cluster ends the chain at the directory that
/// signed the session in; a chain of another cluster can't be ended that
/// way, and an unsigned request is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logging_out_at_the_cluster_ends_the_directorys_chain() {
    let Admitted {
        north,
        acme,
        erin,
        issuer,
        ..
    } = admitted().await;
    let d = north
        .app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .unwrap()
        .unwrap();
    glidex_control_plane::api::trust::pull_feed(&north.app, &d)
        .await
        .unwrap();
    let (cookie, secret) = erin_at_north(&north, &acme, &erin, &d).await;
    let hash = auth::sha256_hex(secret.as_bytes());
    let sid = north
        .app
        .auth
        .store
        .session(&hash)
        .unwrap()
        .unwrap()
        .sid
        .expect("the session carries its chain");
    let t = auth::store::now();
    assert!(acme.app.auth.refresh_tokens.family_alive(&sid, t).unwrap());

    // Unsigned: refused, and the chain lives on.
    let north_id = glidex_control_plane::api::op::cluster_id(&north.app);
    let body = json!({ "cluster_id": north_id, "sid": sid });
    let (s, _) = glidex_control_plane::directory::peer::post(
        &acme.base,
        &d.tls_sha256,
        "/d/local/trust/v1/logout",
        "",
        &body,
    )
    .await
    .unwrap();
    assert_eq!(s, 403);
    assert!(acme.app.auth.refresh_tokens.family_alive(&sid, t).unwrap());

    // Erin logs out at north.
    let resp = north
        .router
        .clone()
        .oneshot(
            Request::post("/auth/logout")
                .header(header::COOKIE, cookie)
                .header(header::ORIGIN, ORIGIN)
                .header(
                    auth::CSRF_HEADER,
                    north.app.auth.store.session(&hash).unwrap().unwrap().csrf,
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    glidex_control_plane::api::trust::end_chains(&north.app).await;
    assert!(
        !acme.app.auth.refresh_tokens.family_alive(&sid, t).unwrap(),
        "the chain ended at acme"
    );
    let ended = acme
        .app
        .auth
        .refresh_tokens
        .ended_since("local", &format!("cluster:{north_id}"), 0)
        .unwrap();
    assert_eq!(ended, vec![sid.clone()]);

    // Another of her chains, gxctl's, stays: north can end only its own.
    let grant = glidex_control_plane::directory::op::Grant {
        user_id: erin.clone(),
        auth_time: t,
        amr: vec!["pwd".into()],
        mfa: false,
        session: None,
    };
    let (_, rt) = acme
        .app
        .auth
        .refresh_tokens
        .start("local", "gxctl", "openid", &grant, 86_400, t)
        .unwrap();
    let other_sid = rt.family;
    let a = glidex_control_plane::directory::peer::assertion(
        &north.app.auth.connectors,
        glidex_control_plane::directory::issuer::CLUSTER_OWNER,
        &north_id,
        &issuer,
        glidex_control_plane::trust::LOGOUT,
        auth::store::now(),
    )
    .unwrap();
    let body = json!({ "cluster_id": north_id, "sid": other_sid });
    let (s, _) = glidex_control_plane::directory::peer::post(
        &acme.base,
        &d.tls_sha256,
        "/d/local/trust/v1/logout",
        &a,
        &body,
    )
    .await
    .unwrap();
    assert_eq!(s, 204);
    assert!(
        acme.app
            .auth
            .refresh_tokens
            .family_alive(&other_sid, t)
            .unwrap(),
        "not north's chain"
    );
}

/// D73: when the cluster's web UI address changes, its next report
/// registers the new callback at the directory, and logins come back
/// there; a callback that isn't an http(s) URL is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_web_ui_address_is_registered_again() {
    let Admitted {
        north,
        acme,
        erin,
        issuer,
        ..
    } = admitted().await;
    let d = north
        .app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .unwrap()
        .unwrap();
    glidex_control_plane::api::trust::pull_feed(&north.app, &d)
        .await
        .unwrap();
    let callback = |acme: &Cp| {
        acme.app
            .auth
            .trust
            .clusters("local")
            .unwrap()
            .pop()
            .unwrap()
            .ui_callback
    };
    assert_eq!(
        callback(&acme).as_deref(),
        Some(format!("{ORIGIN}/api/auth/t/{}/callback", d.id).as_str())
    );
    north
        .app
        .manager
        .set_ui_url(Some("https://north.example:5173".into()));
    glidex_control_plane::api::trust::push_report(&north.app, &d)
        .await
        .unwrap();
    let moved = format!("https://north.example:5173/api/auth/t/{}/callback", d.id);
    assert_eq!(callback(&acme).as_deref(), Some(moved.as_str()));

    // A login comes back to the new address.
    let u = acme.app.auth.store.user(&erin).unwrap().unwrap();
    let (cookie, _) = acme.app.auth.create_session(&u, auth::Method::Pam).unwrap();
    let (_, to, _) = browse(&north.router, &format!("/auth/t/{}/start", d.id), &[]).await;
    let (s, back, _) = browse(
        &acme.router,
        &path_of(&to),
        &[format!("{}={cookie}", auth::SESSION_COOKIE)],
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(back.starts_with(&moved), "{back}");

    // Not a URL: the registered one stays.
    let north_id = glidex_control_plane::api::op::cluster_id(&north.app);
    let report = glidex_control_plane::api::trust::report_for(&north.app, &d)
        .await
        .unwrap();
    let a = glidex_control_plane::directory::peer::assertion(
        &north.app.auth.connectors,
        glidex_control_plane::directory::issuer::CLUSTER_OWNER,
        &north_id,
        &issuer,
        glidex_control_plane::trust::REPORT,
        auth::store::now(),
    )
    .unwrap();
    let body =
        json!({ "cluster_id": north_id, "report": report, "ui_callback": "javascript:alert(1)" });
    let (s, _) = glidex_control_plane::directory::peer::post(
        &acme.base,
        &d.tls_sha256,
        "/d/local/feed/v1/report",
        &a,
        &body,
    )
    .await
    .unwrap();
    assert_eq!(s, 204);
    assert_eq!(callback(&acme).as_deref(), Some(moved.as_str()));
}

/// D70: with its feed, north reports to acme what it holds of acme's
/// tenant; acme keeps it on the cluster's record and the tenant's cluster
/// list shows it; a report naming a tenant not opted in to north keeps
/// only the opted-in one, and an unsigned one is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cluster_reports_its_tenants_back() {
    let Admitted {
        north,
        acme,
        issuer,
        ..
    } = admitted().await;
    let d = north
        .app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .unwrap()
        .unwrap();
    glidex_control_plane::api::trust::pull_feed(&north.app, &d)
        .await
        .unwrap();
    glidex_control_plane::api::trust::push_report(&north.app, &d)
        .await
        .unwrap();
    let c = acme
        .app
        .auth
        .trust
        .clusters("local")
        .unwrap()
        .pop()
        .unwrap();
    let r = c.report.clone().expect("a report");
    assert_eq!(r.tenants.len(), 1);
    let t = &r.tenants[0];
    assert_eq!(c.tenants, vec![t.tenant.clone()]);
    assert_eq!(
        (t.suspended, t.projects, t.vms, t.disks),
        (false, 1, 0, 0),
        "{t:?}"
    );

    // The tenant's page, at acme.
    let root = acme
        .app
        .auth
        .store
        .user_for_identity("pam", "root", "root", None, false)
        .unwrap()
        .unwrap();
    let (cookie, csrf) = acme
        .app
        .auth
        .create_session(&root, auth::Method::Pam)
        .unwrap();
    let (s, v) = acme
        .rest(
            "GET",
            "/tenants/acme/clusters",
            None,
            &format!("{cookie}|{csrf}"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rep = &v["clusters"][0]["report"];
    assert_eq!(rep["projects"], "1", "{v}");
    assert_eq!(rep["status"], "ADMISSION_STATUS_ADMITTED", "{v}");

    // Someone else's tenant in north's report: dropped.
    let north_id = glidex_control_plane::api::op::cluster_id(&north.app);
    let mut forged = r.clone();
    let mut other = t.clone();
    other.tenant = "someone-else".into();
    other.vms = 99;
    forged.tenants.push(other);
    let a = glidex_control_plane::directory::peer::assertion(
        &north.app.auth.connectors,
        glidex_control_plane::directory::issuer::CLUSTER_OWNER,
        &north_id,
        &issuer,
        glidex_control_plane::trust::REPORT,
        glidex_control_plane::auth::store::now(),
    )
    .unwrap();
    let body = json!({ "cluster_id": north_id, "report": forged });
    let (s, _) = glidex_control_plane::directory::peer::post(
        &acme.base,
        &d.tls_sha256,
        "/d/local/feed/v1/report",
        &a,
        &body,
    )
    .await
    .unwrap();
    assert_eq!(s, 204);
    let c2 = acme
        .app
        .auth
        .trust
        .clusters("local")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(c2.report.unwrap().tenants.len(), 1, "only acme's own");
    // Unsigned: refused.
    let (s, _) = glidex_control_plane::directory::peer::post(
        &acme.base,
        &d.tls_sha256,
        "/d/local/feed/v1/report",
        "",
        &body,
    )
    .await
    .unwrap();
    assert_eq!(s, 403);
}

/// D13: past its `max_staleness` without a feed, a trusted directory's
/// credentials are refused on the cluster (but `/auth/…`), the cluster's
/// own people are not affected, responses carry the lag once it is behind,
/// and the next feed restores them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_out_of_reach_too_long_is_refused() {
    let Admitted {
        north,
        acme,
        erin,
        issuer,
        ..
    } = admitted().await;
    let op = north.admin();
    let d = north
        .app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .unwrap()
        .unwrap();
    glidex_control_plane::api::trust::pull_feed(&north.app, &d)
        .await
        .unwrap();
    let north_id = glidex_control_plane::api::op::cluster_id(&north.app);
    let grant = glidex_control_plane::directory::op::Grant {
        user_id: erin.clone(),
        auth_time: glidex_control_plane::auth::store::now(),
        amr: vec!["pwd".into()],
        mfa: false,
        session: None,
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
    let resp = acme
        .router
        .clone()
        .oneshot(
            Request::post("/d/local/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=refresh_token&client_id=gxctl&refresh_token={refresh}&resource=urn:glidex:cluster:{north_id}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let access = v["access_token"].as_str().unwrap().to_string();
    let call = |path: &'static str| {
        let north = &north;
        let access = access.clone();
        async move {
            let resp = north
                .router
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::AUTHORIZATION, format!("Bearer {access}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let lag = resp
                .headers()
                .get("x-glidex-directory-lag")
                .map(|v| v.to_str().unwrap().parse::<u64>().unwrap());
            let st = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (
                st,
                lag,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let (s, lag, _) = call("/projects").await;
    assert_eq!((s, lag), (StatusCode::OK, None), "fresh: no lag header");

    // Behind: told by how much.
    north.app.auth.trust.backdate_feed(&d.id, 100).unwrap();
    let (s, lag, _) = call("/projects").await;
    assert_eq!(s, StatusCode::OK);
    assert!(lag.is_some_and(|l| (100..200).contains(&l)), "{lag:?}");

    // Past a 1-minute limit: refused, but for /auth/…; the cluster's own
    // administrator is not affected.
    let (s, v) = north
        .rest(
            "PATCH",
            &format!("/admissions/directories/{}", d.id),
            Some(json!({"max_staleness_minutes": 1})),
            &op,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["directory"]["stale"], true, "{v}");
    let (s, _, v) = call("/projects").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("directory_stale")),
        "{v}"
    );
    let (s, lag, _) = call("/auth/whoami").await;
    assert_eq!(s, StatusCode::OK);
    assert!(lag.is_some());
    let (s, _) = north.rest("GET", "/projects", None, &op).await;
    assert_eq!(s, StatusCode::OK, "PAM still works");
    let (s, v) = north
        .rest("GET", "/admissions/directories", None, &op)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["directories"][0]["effective_max_staleness_minutes"], "1",
        "{v}"
    );

    // The next feed (nothing new) makes it fresh again.
    assert!(
        !glidex_control_plane::api::trust::pull_feed(&north.app, &d)
            .await
            .unwrap()
    );
    let (s, lag, _) = call("/projects").await;
    assert_eq!((s, lag), (StatusCode::OK, None));

    // Back to the default (60 minutes).
    let (s, v) = north
        .rest(
            "PATCH",
            &format!("/admissions/directories/{}", d.id),
            Some(json!({})),
            &op,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["directory"]["effective_max_staleness_minutes"], "60",
        "{v}"
    );
}

/// §7.4: an API token is good only on the clusters it lists. Erin's token
/// listing north reaches north through the feed (its hash, cut); one with
/// no list stays at acme; `all` works on north too; an unknown cluster is
/// refused at creation; Bob's (not in acme) is never fed; revoking it at
/// acme ends it on north with the next feed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_tokens_work_on_the_clusters_they_list() {
    let Admitted {
        north,
        acme,
        erin,
        bob,
        project,
        issuer,
    } = admitted().await;
    let north_id = glidex_control_plane::api::op::cluster_id(&north.app);
    let session_of = |user: &str| {
        let u = acme.app.auth.store.user(user).unwrap().unwrap();
        let (cookie, csrf) = acme.app.auth.create_session(&u, auth::Method::Pam).unwrap();
        format!("{cookie}|{csrf}")
    };
    let erin_s = session_of(&erin);
    let mint = |s: String, clusters: Value| {
        let acme = &acme;
        async move {
            acme.rest(
                "POST",
                "/tokens",
                Some(json!({"name": "t", "clusters": clusters})),
                &s,
            )
            .await
        }
    };
    let (s, v) = mint(erin_s.clone(), json!([north_id])).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let for_north = v["token"].as_str().unwrap().to_string();
    let for_north_id = v["record"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["record"]["clusters"].as_array().unwrap().len(), 2, "{v}");
    let (_, v) = mint(erin_s.clone(), json!([])).await;
    let local_only = v["token"].as_str().unwrap().to_string();
    let (_, v) = mint(erin_s.clone(), json!(["all"])).await;
    let everywhere = v["token"].as_str().unwrap().to_string();
    let (s, v) = mint(erin_s.clone(), json!(["no-such-cluster"])).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = mint(session_of(&bob), json!([north_id])).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let bobs = v["token"].as_str().unwrap().to_string();

    let get = |cp: &Cp, tok: String| {
        let router = cp.router.clone();
        async move {
            let resp = router
                .oneshot(
                    Request::get("/projects")
                        .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let st = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (st, String::from_utf8_lossy(&bytes).to_string())
        }
    };
    // At acme, where they were made, all of them work.
    for t in [&for_north, &local_only, &everywhere] {
        assert_eq!(get(&acme, t.clone()).await.0, StatusCode::OK);
    }
    assert_eq!(
        get(&north, for_north.clone()).await.0,
        StatusCode::UNAUTHORIZED,
        "before a feed"
    );

    let d = north
        .app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .unwrap()
        .unwrap();
    glidex_control_plane::api::trust::pull_feed(&north.app, &d)
        .await
        .unwrap();
    let (s, body) = get(&north, for_north.clone()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(body.contains(&project), "her project role holds: {body}");
    assert_eq!(get(&north, everywhere.clone()).await.0, StatusCode::OK);
    assert_eq!(
        get(&north, local_only).await.0,
        StatusCode::UNAUTHORIZED,
        "not listed"
    );
    assert_eq!(
        get(&north, bobs).await.0,
        StatusCode::UNAUTHORIZED,
        "not acme's"
    );
    // What north keeps: the hash and lifetime, no use or device.
    let (_, t) = north
        .app
        .auth
        .store
        .token_by_id(&for_north_id)
        .unwrap()
        .expect("fed");
    assert!(t.device.is_none() && t.client.is_none());

    // Revoked at acme: gone from north with the next feed.
    let (s, _) = acme
        .rest("DELETE", &format!("/tokens/{for_north_id}"), None, &erin_s)
        .await;
    assert!(s.is_success());
    glidex_control_plane::api::trust::pull_feed(&north.app, &d)
        .await
        .unwrap();
    assert_eq!(get(&north, for_north).await.0, StatusCode::UNAUTHORIZED);
}

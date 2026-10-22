//! Passkeys (spec/passkeys.md §5) through the API, with a software
//! authenticator: registration, sign-in, and every refusal looking the
//! same; MFA and step-up; P4's removal rules.

mod common;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use glidex_control_plane::api::{self, App, AppState, ClientAddr, Listener, PeerUid};
use glidex_control_plane::auth::AuthService;
use glidex_control_plane::auth::webauthn::{Cbor, b64url, encode};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
use http_body_util::BodyExt;
use p256::ecdsa::signature::Signer as _;
use serde_json::{Value, json};
use sha2::Digest as _;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "https://glidex.example.com:5173";
const RP: &str = "example.com";

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
    cfg.auth.allowed_origins = vec![ORIGIN.into(), "https://evil.example:5173".into()];
    cfg.auth.passkeys.rp_id = Some(RP.into());
    // This test plays glidex-ui on ui.sock, as the UI e2e setup does.
    cfg.ui_user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .unwrap()
        .name;
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    H {
        router: api::router(app.clone()),
        app,
        _dir: dir,
    }
}

/// A browser: its cookies, and the session's CSRF value.
#[derive(Default, Clone)]
struct Browser {
    cookies: Vec<(String, String)>,
    csrf: Option<String>,
    origin: Option<String>,
}

impl Browser {
    fn cookie(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn keep(&mut self, h: &HeaderMap) {
        for v in h.get_all(header::SET_COOKIE) {
            let s = v.to_str().unwrap();
            let (kv, _) = s.split_once(';').unwrap_or((s, ""));
            let (k, val) = kv.split_once('=').unwrap();
            self.cookies.retain(|(x, _)| x != k);
            if !val.is_empty() {
                self.cookies.push((k.into(), val.into()));
            }
        }
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

    /// A browser signed in (by password) as `user`, just now.
    fn signed_in(&self, user: &str) -> Browser {
        let u = self.app.auth.store.user(user).unwrap().unwrap();
        let (cookie, csrf) = self
            .app
            .auth
            .create_session(&u, glidex_control_plane::auth::Method::Pam)
            .unwrap();
        Browser {
            cookies: vec![(glidex_control_plane::auth::SESSION_COOKIE.into(), cookie)],
            csrf: Some(csrf),
            origin: None,
        }
    }

    async fn call(
        &self,
        b: &mut Browser,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut r = Request::builder()
            .method(method)
            .uri(path)
            .header(header::ORIGIN, b.origin.as_deref().unwrap_or(ORIGIN));
        if !b.cookies.is_empty() {
            r = r.header(header::COOKIE, b.cookie());
        }
        if let Some(c) = &b.csrf {
            r = r.header(glidex_control_plane::auth::CSRF_HEADER, c);
        }
        let req = match body {
            Some(v) => r
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => r.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let st = resp.status();
        b.keep(resp.headers());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

/// A software passkey (P-256).
struct Authenticator {
    key: p256::ecdsa::SigningKey,
    id: Vec<u8>,
    counter: u32,
    uv: bool,
    /// The user handle it was registered with.
    handle: String,
}

fn sha256(b: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(b).to_vec()
}

impl Authenticator {
    fn new() -> Self {
        Authenticator {
            key: p256::ecdsa::SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng),
            id: sha256(uuid::Uuid::new_v4().as_bytes())[..16].to_vec(),
            counter: 0,
            uv: true,
            handle: String::new(),
        }
    }

    fn auth_data(&self, rp: &str, attested: bool) -> Vec<u8> {
        let mut a = sha256(rp.as_bytes());
        let flags = 0x01 | if self.uv { 0x04 } else { 0 } | if attested { 0x40 } else { 0 };
        a.push(flags);
        a.extend(self.counter.to_be_bytes());
        if attested {
            let p = self.key.verifying_key().to_encoded_point(false);
            a.extend([0u8; 16]);
            a.extend((self.id.len() as u16).to_be_bytes());
            a.extend(&self.id);
            a.extend(encode(&Cbor::Map(vec![
                (Cbor::Int(1), Cbor::Int(2)),
                (Cbor::Int(3), Cbor::Int(-7)),
                (Cbor::Int(-1), Cbor::Int(1)),
                (Cbor::Int(-2), Cbor::Bytes(p.x().unwrap().to_vec())),
                (Cbor::Int(-3), Cbor::Bytes(p.y().unwrap().to_vec())),
            ])));
        }
        a
    }

    fn client_data(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
        json!({"type": kind, "challenge": challenge, "origin": origin, "crossOrigin": false})
            .to_string()
            .into_bytes()
    }

    /// `navigator.credentials.create`, answering `options`.
    fn create(&mut self, options: &Value, origin: &str) -> Value {
        let pk = &options["publicKey"];
        self.handle = pk["user"]["id"].as_str().unwrap().into();
        let rp = pk["rp"]["id"].as_str().unwrap();
        let cd = Self::client_data("webauthn.create", pk["challenge"].as_str().unwrap(), origin);
        let att = encode(&Cbor::Map(vec![
            (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
            (Cbor::Text("attStmt".into()), Cbor::Map(vec![])),
            (
                Cbor::Text("authData".into()),
                Cbor::Bytes(self.auth_data(rp, true)),
            ),
        ]));
        json!({"id": b64url(&self.id), "rawId": b64url(&self.id), "type": "public-key",
               "response": {"clientDataJSON": b64url(&cd), "attestationObject": b64url(&att)}})
    }

    /// `navigator.credentials.get`, answering `options`.
    fn get(&mut self, options: &Value, origin: &str) -> Value {
        let pk = &options["publicKey"];
        self.counter += 1;
        let cd = Self::client_data("webauthn.get", pk["challenge"].as_str().unwrap(), origin);
        let ad = self.auth_data(pk["rpId"].as_str().unwrap(), false);
        let mut msg = ad.clone();
        msg.extend(sha256(&cd));
        let sig: p256::ecdsa::Signature = self.key.sign(&msg);
        json!({"id": b64url(&self.id), "rawId": b64url(&self.id), "type": "public-key",
               "response": {"clientDataJSON": b64url(&cd), "authenticatorData": b64url(&ad),
                            "signature": b64url(sig.to_der().as_bytes()), "userHandle": self.handle}})
    }
}

async fn register(
    h: &H,
    b: &mut Browser,
    a: &mut Authenticator,
    name: &str,
) -> (StatusCode, Value) {
    let (st, opts) = h
        .call(
            b,
            "POST",
            "/auth/passkeys/register/start",
            Some(json!({"name": name})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{opts}");
    let cred = a.create(&opts, ORIGIN);
    h.call(b, "POST", "/auth/passkeys/register/finish", Some(cred))
        .await
}

async fn sign_in(h: &H, a: &mut Authenticator, origin: &str) -> (StatusCode, Value, Browser) {
    let mut b = Browser::default();
    let (st, opts) = h
        .call(&mut b, "POST", "/auth/passkeys/login/start", None)
        .await;
    assert_eq!(st, StatusCode::OK, "{opts}");
    let cred = a.get(&opts, origin);
    let (st, v) = h
        .call(&mut b, "POST", "/auth/passkeys/login/finish", Some(cred))
        .await;
    // The page keeps the session's CSRF value from the login's answer.
    b.csrf = v["csrf"].as_str().map(String::from);
    (st, v, b)
}

#[tokio::test]
async fn a_passkey_is_added_by_its_user_and_signs_in() {
    let h = harness();
    let ada = h.user("ada");
    let mut b = h.signed_in(&ada);
    let mut a = Authenticator::new();

    let (st, opts) = h
        .call(
            &mut b,
            "POST",
            "/auth/passkeys/register/start",
            Some(json!({"name": "laptop"})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{opts}");
    let pk = &opts["publicKey"];
    assert_eq!(pk["rp"]["id"], RP);
    assert_eq!(pk["authenticatorSelection"]["userVerification"], "required");
    assert_eq!(pk["authenticatorSelection"]["residentKey"], "required");
    assert_ne!(
        pk["user"]["id"],
        ada.as_str(),
        "the handle is never the user id"
    );
    let cred = a.create(&opts, ORIGIN);
    let (st, v) = h
        .call(
            &mut b,
            "POST",
            "/auth/passkeys/register/finish",
            Some(cred.clone()),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    // A sign-in method of hers, listed with its name.
    let ids = h.app.auth.store.identities_of(&ada).unwrap();
    assert!(
        ids.iter()
            .any(|i| i.provider == "passkey" && i.subject == b64url(&a.id))
    );
    let (_, l) = h
        .call(&mut b, "GET", &format!("/users/{ada}/passkeys"), None)
        .await;
    assert_eq!(l["passkeys"][0]["name"], "laptop");
    assert_eq!(l["passkeys"][0]["algorithm"], "ES256");
    assert_eq!(l["rp_id"], RP);
    // The same registration twice, and a stale prompt, are refused.
    let (st, _) = h
        .call(&mut b, "POST", "/auth/passkeys/register/finish", Some(cred))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Sign in: no user name; a multi-factor session.
    let (st, w, mut nb) = sign_in(&h, &mut a, ORIGIN).await;
    assert_eq!(st, StatusCode::OK, "{w}");
    assert_eq!(w["user_id"], ada.as_str());
    assert!(
        nb.cookies
            .iter()
            .any(|(k, _)| k == glidex_control_plane::auth::SESSION_COOKIE)
    );
    let (_, me) = h.call(&mut nb, "GET", "/auth/whoami", None).await;
    assert_eq!(me["method"], "passkey", "{me}");
    let (_, l) = h
        .call(&mut b, "GET", &format!("/users/{ada}/passkeys"), None)
        .await;
    assert!(l["passkeys"][0]["last_used_at"].is_string());
}

#[tokio::test]
async fn every_bad_sign_in_gets_the_same_answer() {
    let h = harness();
    let ada = h.user("ada");
    let mut b = h.signed_in(&ada);
    let mut a = Authenticator::new();
    let (st, v) = register(&h, &mut b, &mut a, "key").await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let (st, _, _) = sign_in(&h, &mut a, ORIGIN).await;
    assert_eq!(st, StatusCode::OK);

    let refused = json!({"error": "passkey_refused"});
    let check = |st: StatusCode, v: &Value, why: &str| {
        assert_eq!(
            (st, &v["error"]),
            (StatusCode::UNAUTHORIZED, &refused["error"]),
            "{why}: {v}"
        );
    };
    // Signed for another origin (a phishing page the browser vouches for).
    let (st, v, _) = sign_in(&h, &mut a, "https://glidex-login.example.net").await;
    check(st, &v, "another origin");
    // Without user verification.
    a.uv = false;
    let (st, v, _) = sign_in(&h, &mut a, ORIGIN).await;
    check(st, &v, "no user verification");
    a.uv = true;
    // A counter that goes backwards: a cloned passkey.
    let saved = a.counter;
    a.counter = 0;
    let (st, v, _) = sign_in(&h, &mut a, ORIGIN).await;
    check(st, &v, "a counter going backwards");
    a.counter = saved + 5;
    // A replayed answer: the challenge is single use.
    let mut br = Browser::default();
    let (_, opts) = h
        .call(&mut br, "POST", "/auth/passkeys/login/start", None)
        .await;
    let cred = a.get(&opts, ORIGIN);
    let (st, _) = h
        .call(
            &mut br.clone(),
            "POST",
            "/auth/passkeys/login/finish",
            Some(cred.clone()),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = h
        .call(&mut br, "POST", "/auth/passkeys/login/finish", Some(cred))
        .await;
    check(st, &v, "a replay");
    // Another authenticator nobody registered.
    let mut stranger = Authenticator::new();
    stranger.handle = a.handle.clone();
    let (st, v, _) = sign_in(&h, &mut stranger, ORIGIN).await;
    check(st, &v, "an unknown passkey");
    // A disabled user.
    let mut u = h.app.auth.store.user(&ada).unwrap().unwrap();
    u.disabled = true;
    h.app.auth.store.put_user(&u).unwrap();
    let (st, v, _) = sign_in(&h, &mut a, ORIGIN).await;
    check(st, &v, "a disabled user");
    // All of them audited, with their reasons.
    let entries = h.app.auth.store.audit(0, 200, |_| true).unwrap();
    let text = serde_json::to_string(&entries).unwrap();
    assert!(
        text.contains("went backwards") && text.contains("disabled"),
        "{text}"
    );
}

#[tokio::test]
async fn adding_needs_a_fresh_sign_in_and_two_passkeys_are_needed_to_lose_the_rest() {
    let h = harness();
    let ada = h.user("ada");
    // An old session can't add one (step-up).
    let u = h.app.auth.store.user(&ada).unwrap().unwrap();
    let (cookie, csrf) = h
        .app
        .auth
        .create_session(&u, glidex_control_plane::auth::Method::Pam)
        .unwrap();
    let hash = glidex_control_plane::auth::sha256_hex(cookie.as_bytes());
    let mut s = h.app.auth.store.session(&hash).unwrap().unwrap();
    s.authenticated_at -= 3600;
    h.app.auth.store.put_session(&hash, &s).unwrap();
    let mut old = Browser {
        cookies: vec![(glidex_control_plane::auth::SESSION_COOKIE.into(), cookie)],
        csrf: Some(csrf),
        origin: None,
    };
    let (st, v) = h
        .call(
            &mut old,
            "POST",
            "/auth/passkeys/register/start",
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("reauth_required")),
        "{v}"
    );
    // A page outside the passkeys' domain can't start a ceremony.
    let mut b = h.signed_in(&ada);
    b.origin = Some("https://evil.example:5173".into());
    let (st, _) = h
        .call(
            &mut b,
            "POST",
            "/auth/passkeys/register/start",
            Some(json!({})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    b.origin = None;

    // One passkey beside her password: the password can't go (P4).
    let mut k1 = Authenticator::new();
    assert_eq!(
        register(&h, &mut b, &mut k1, "one").await.0,
        StatusCode::CREATED
    );
    let pam = format!(
        "pam:{}",
        h.app
            .auth
            .store
            .identities_of(&ada)
            .unwrap()
            .iter()
            .find(|i| i.provider == "pam")
            .unwrap()
            .subject
    );
    let (st, v) = h
        .call(
            &mut b,
            "DELETE",
            &format!("/users/{ada}/identities/{pam}"),
            None,
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("last_identity")),
        "{v}"
    );
    // With a second passkey it can.
    let mut k2 = Authenticator::new();
    assert_eq!(
        register(&h, &mut b, &mut k2, "two").await.0,
        StatusCode::CREATED
    );
    let (st, v) = h
        .call(
            &mut b,
            "DELETE",
            &format!("/users/{ada}/identities/{pam}"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");
    // And then one of the two can't go.
    let (st, v) = h
        .call(
            &mut b,
            "DELETE",
            &format!("/users/{ada}/passkeys/{}", b64url(&k1.id)),
            None,
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("last_identity")),
        "{v}"
    );
    // Rename; another user can't rename hers.
    let (st, v) = h
        .call(
            &mut b,
            "PATCH",
            &format!("/users/{ada}/passkeys/{}", b64url(&k1.id)),
            Some(json!({"name": "phone"})),
        )
        .await;
    assert_eq!(
        (st, v["passkey"]["name"].as_str()),
        (StatusCode::OK, Some("phone")),
        "{v}"
    );
    let bob = h.user("bob");
    let mut bb = h.signed_in(&bob);
    let (st, _) = h
        .call(
            &mut bb,
            "PATCH",
            &format!("/users/{ada}/passkeys/{}", b64url(&k1.id)),
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = h
        .call(
            &mut bb,
            "DELETE",
            &format!("/users/{ada}/passkeys/{}", b64url(&k1.id)),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // An administrator may remove hers (never add one).
    let admin = h.user("root-admin");
    h.app
        .auth
        .add_link(
            "role.system-admin",
            Ent::User(admin.clone()),
            Ent::Cluster,
            "test",
        )
        .unwrap();
    h.app
        .auth
        .make_administrator(&Ent::User(admin.clone()), "test")
        .unwrap();
    let mut ab = h.signed_in(&admin);
    let mut k3 = Authenticator::new();
    assert_eq!(
        register(&h, &mut b, &mut k3, "three").await.0,
        StatusCode::CREATED
    );
    let (st, v) = h
        .call(
            &mut ab,
            "DELETE",
            &format!("/users/{ada}/passkeys/{}", b64url(&k3.id)),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");
    let (st, v, _) = sign_in(&h, &mut k3, ORIGIN).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");

    // A passkey sign-in counts as multi-factor and passes step-up: it can
    // add a passkey at once.
    let (st, _, mut fresh) = sign_in(&h, &mut k2, ORIGIN).await;
    assert_eq!(st, StatusCode::OK);
    let mut k4 = Authenticator::new();
    let (st, v) = register(&h, &mut fresh, &mut k4, "four").await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
}

/// spec/tenancy.md D59, D60: a directory's first administrator comes in
/// with a passkey through the operator's invitation, administers the new
/// directory and nothing of the host's own, and signs in again with it.
#[tokio::test]
async fn a_new_directorys_first_admin_signs_in_with_a_passkey() {
    let h = harness();
    let root = h.user("root");
    let ent = glidex_control_plane::authz::Ent::User(root.clone());
    h.app
        .auth
        .add_link(
            "role.system-admin",
            ent.clone(),
            glidex_control_plane::authz::Ent::Cluster,
            "test",
        )
        .unwrap();
    h.app.auth.make_administrator(&ent, "test").unwrap();
    let mut op = h.signed_in(&root);
    let (st, v) = h
        .call(
            &mut op,
            "POST",
            "/directories",
            Some(json!({"id": "acme", "name": "Acme", "owner": "acme-admin", "admin_email": "erin@acme.example"})),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let link = v["invitation_link"].as_str().unwrap();
    let (id, secret) = link
        .rsplit_once("/invite/")
        .unwrap()
        .1
        .split_once('#')
        .unwrap();
    let invitation = format!("{id}.{secret}");

    // The invite page: what it is, then the passkey.
    let mut b = Browser::default();
    let (st, v) = h
        .call(
            &mut b,
            "POST",
            "/auth/invitation",
            Some(json!({"invitation": invitation})),
        )
        .await;
    assert_eq!(
        (st, v["connector_kind"].as_str()),
        (StatusCode::OK, Some("passkey")),
        "{v}"
    );
    let (st, opts) = h
        .call(
            &mut b,
            "POST",
            "/auth/invitation/passkey/start",
            Some(json!({"invitation": invitation})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{opts}");
    let mut a = Authenticator::new();
    let cred = a.create(&opts, ORIGIN);
    // The credential with another invitation's id is refused.
    let (st, _) = h
        .call(
            &mut b.clone(),
            "POST",
            "/auth/invitation/passkey/finish",
            Some(json!({"invitation": format!("{id}.wrong"), "credential": cred})),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, me) = h
        .call(
            &mut b,
            "POST",
            "/auth/invitation/passkey/start",
            Some(json!({"invitation": invitation})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let cred = a.create(&me, ORIGIN);
    let (st, me) = h
        .call(
            &mut b,
            "POST",
            "/auth/invitation/passkey/finish",
            Some(json!({"invitation": invitation, "credential": cred})),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{me}");
    assert_eq!(me["method"], "passkey", "{me}");
    b.csrf = me["csrf"].as_str().map(String::from);
    let erin = me["user"]["id"].as_str().unwrap().to_string();
    let u = h.app.auth.store.user(&erin).unwrap().unwrap();
    assert_eq!(u.directory_id, "acme");

    // She administers acme: its connectors and users, and nothing of local.
    let (st, v) = h.call(&mut b, "GET", "/users", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let users: Vec<_> = v["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(users, vec![erin.clone()], "{v}");
    let (st, v) = h
        .call(
            &mut b,
            "POST",
            "/connectors",
            Some(json!({"connector": {"id": "acme-idp", "display_name": "Acme IdP", "oidc": {"issuer": "https://idp.acme.example", "client_id": "glidex"}}})),
        )
        .await;
    assert!(st.is_success(), "{v}");
    let (st, _) = h.call(&mut b, "GET", "/tenants/admin", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // The invitation is spent.
    let (st, _) = h
        .call(
            &mut Browser::default(),
            "POST",
            "/auth/invitation/passkey/start",
            Some(json!({"invitation": invitation})),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // And she signs in again with the passkey.
    let (st, v, _) = sign_in(&h, &mut a, ORIGIN).await;
    assert_eq!(
        (st, v["user"]["id"].as_str()),
        (StatusCode::OK, Some(erin.as_str())),
        "{v}"
    );
}

/// Sign-in attempts are limited per client. Over TCP the client is the
/// peer, whatever X-Forwarded-For says; only glidex-ui's socket passes a
/// browser's address on that way.
#[tokio::test]
async fn sign_in_attempts_are_limited_by_the_real_client() {
    let h = harness();
    let finish = |listener: Listener, forwarded: String| {
        let mut r = Request::builder()
            .method("POST")
            .uri("/auth/passkeys/login/finish")
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-forwarded-for", forwarded)
            .body(Body::from(
                json!({"id": "AA", "rawId": "AA", "type": "public-key",
                       "response": {"clientDataJSON": "", "authenticatorData": "", "signature": ""}})
                .to_string(),
            ))
            .unwrap();
        r.extensions_mut().insert(listener);
        match listener {
            Listener::Ui => {
                r.extensions_mut()
                    .insert(PeerUid(nix::unistd::getuid().as_raw()));
            }
            _ => {
                r.extensions_mut()
                    .insert(ClientAddr("192.0.2.7:40000".parse().unwrap()));
            }
        }
        let router = h.router.clone();
        async move { router.oneshot(r).await.unwrap().status() }
    };
    // Over TCP, a new X-Forwarded-For each time is still one client.
    let mut limited = false;
    for i in 0..15 {
        if finish(Listener::Tcp, format!("198.51.100.{i}")).await == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(
        limited,
        "made-up X-Forwarded-For values got around the limit"
    );
    // From glidex-ui, the browser's address it forwards is the client:
    // another browser isn't held back by the first one's attempts.
    for _ in 0..15 {
        finish(Listener::Ui, "203.0.113.1".into()).await;
    }
    assert_eq!(
        finish(Listener::Ui, "203.0.113.1".into()).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_ne!(
        finish(Listener::Ui, "203.0.113.2".into()).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

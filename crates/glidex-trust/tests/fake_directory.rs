//! A cluster built from this crate's pieces, trusting the fake directory:
//! the whole handshake, the feed, reports, logins and tokens, logouts,
//! ended chains, a partition and ending trust, both ways.

use glidex_trust::client::{self, Feed};
use glidex_trust::cluster::{self, Offer};
use glidex_trust::fake::{FakeDirectory, Handler, Reply, serve_https};
use glidex_trust::feed::{self, Entity};
use glidex_trust::key::{self, ClusterKey};
use glidex_trust::{
    ClusterReport, EndAtCluster, FeedState, OfferToken, RedeemAnswer, RedeemRequest, Replays,
    StorageReport, StorageTenantReport, TrustedDirectory, peer, tokens,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

const CLUSTER: &str = "c1";
const CALLBACK: &str = "https://north.example/api/auth/t/x/callback";

/// The cluster's side: its key, one open offer, its trust records.
struct Cluster {
    key: ClusterKey,
    secret_hash: String,
    offer_open: Mutex<bool>,
    replays: Replays,
    trusted: Mutex<Option<TrustedDirectory>>,
}

impl Cluster {
    fn handle(&self, req: glidex_trust::fake::Request, issuer: &str) -> Reply {
        let refused = || Reply::json(403, json!({ "code": "trust_refused" }));
        let Some(a) = peer::from_headers(&req.headers) else {
            return refused();
        };
        let now = glidex_trust::now();
        match req.path.as_str() {
            "/trust/v1/redeem" => {
                let Ok(b) = serde_json::from_slice::<RedeemRequest>(&req.body) else {
                    return refused();
                };
                let open = *self.offer_open.lock().unwrap();
                let offer = Offer {
                    open,
                    secret_hash: &self.secret_hash,
                    directory: issuer,
                    tenant: "acme",
                };
                let Ok(r) = cluster::check_redeem(&a, &b, Some(offer), CLUSTER, &self.replays, now)
                else {
                    return refused();
                };
                // Once: the offer is spent first.
                *self.offer_open.lock().unwrap() = false;
                let mut t = self.trusted.lock().unwrap();
                *t = Some(cluster::redeemed_directory(t.take(), &b, &r, now));
                Reply::json(
                    200,
                    serde_json::to_value(RedeemAnswer {
                        cluster_id: CLUSTER.into(),
                        cluster_key: key::jwks(&[&self.key]),
                        ui_callback: Some(CALLBACK.into()),
                    })
                    .unwrap(),
                )
            }
            "/trust/v1/end" => {
                let Ok(b) = serde_json::from_slice::<EndAtCluster>(&req.body) else {
                    return refused();
                };
                let cur = self.trusted.lock().unwrap().clone();
                let found = cluster::check_end(
                    &a,
                    &b,
                    |iss| cur.filter(|d| d.issuer == iss),
                    CLUSTER,
                    &self.replays,
                    now,
                );
                match found {
                    Ok(mut d) => {
                        d.tenants.retain(|t| t.id != b.tenant);
                        *self.trusted.lock().unwrap() = (!d.tenants.is_empty()).then_some(d);
                        Reply::status(200)
                    }
                    Err(_) => refused(),
                }
            }
            _ => Reply::status(404),
        }
    }
}

fn trusted(c: &Cluster) -> TrustedDirectory {
    c.trusted.lock().unwrap().clone().expect("trusted")
}

#[tokio::test]
async fn a_cluster_trusts_the_fake_directory() {
    let dir = FakeDirectory::start("acme").await.unwrap();
    let tenant = dir.add_tenant("t1", "acme");
    dir.add_project("p1", "t1", "web");
    dir.add_user("u1", "Ada", false);
    dir.add_member("t1", "u1");
    dir.link(
        Entity::User("u1".into()),
        Entity::Tenant("t1".into()),
        "role.tenant-admin",
    );

    // The cluster, with an offer for tenant `acme` of this directory.
    let c = Arc::new(Cluster {
        key: ClusterKey::generate("c1:1").unwrap(),
        secret_hash: glidex_trust::sha256_hex(b"s3cret"),
        offer_open: Mutex::new(true),
        replays: Replays::default(),
        trusted: Mutex::new(None),
    });
    let (c2, iss) = (c.clone(), dir.issuer().to_string());
    let handler: Handler = Arc::new(move |req| {
        let (c, iss) = (c2.clone(), iss.clone());
        Box::pin(async move { c.handle(req, &iss) })
    });
    let https = serve_https(handler).await.unwrap();
    let offer = OfferToken {
        cluster_id: CLUSTER.into(),
        cluster_name: "north".into(),
        api: https.base.clone(),
        tls_sha256: https.pin.clone(),
        tenant: "acme".into(),
        directory: dir.issuer().into(),
        offer: "o1".into(),
        secret: "s3cret".into(),
        expires_at: u64::MAX,
    }
    .encode();

    // The handshake: once.
    let a = dir.redeem(&offer, &tenant).await.unwrap();
    assert_eq!(a.cluster_id, CLUSTER);
    assert!(dir.redeem(&offer, &tenant).await.is_err());
    let d = trusted(&c);
    assert_eq!(d.issuer, dir.issuer());
    assert_eq!(d.tls_sha256, dir.tls_sha256());
    assert_eq!(
        dir.cluster(CLUSTER).unwrap().tenants,
        vec!["t1".to_string()]
    );

    // The feed, then nothing new.
    let now = glidex_trust::now();
    let Feed::Changed(snap) = client::pull_feed(&c.key, CLUSTER, &d, "", now)
        .await
        .unwrap()
    else {
        panic!("a first feed");
    };
    let users: Vec<(&str, feed::User)> = feed::rows(&snap, "users").collect();
    assert_eq!(users[0].1.display_name, "Ada");
    let links: Vec<(&str, feed::Link)> = feed::rows(&snap, "policy_links").collect();
    assert_eq!(links[0].1.template, "role.tenant-admin");
    assert_eq!(
        client::pull_feed(&c.key, CLUSTER, &d, &snap.revision, now)
            .await
            .unwrap(),
        Feed::Unchanged
    );
    // A disabled user comes with the next feed.
    dir.add_user("u1", "Ada", true);
    let Feed::Changed(snap) = client::pull_feed(&c.key, CLUSTER, &d, &snap.revision, now)
        .await
        .unwrap()
    else {
        panic!("a changed feed");
    };
    assert!(feed::rows::<feed::User>(&snap, "users").all(|(_, u)| u.disabled));
    dir.add_user("u1", "Ada", false);
    // Signed with another key, it is refused.
    let stranger = ClusterKey::generate("c1:1").unwrap();
    assert!(
        client::pull_feed(&stranger, CLUSTER, &d, "", now)
            .await
            .is_err()
    );

    // A storage cluster's report: only storage, as tenant totals (D85).
    let storage = StorageReport {
        volumes: 2,
        volume_gib: 100,
        volume_used_bytes: 7 << 30,
        buckets: 1,
        bucket_bytes: 12_345,
        s3_keys: 3,
    };
    let report = ClusterReport {
        at: now,
        tenants: vec![StorageTenantReport {
            tenant: "t1".into(),
            suspended: false,
            storage: storage.clone(),
        }],
    };
    client::push_report(&c.key, CLUSTER, &d, &report, Some(CALLBACK), now)
        .await
        .unwrap();
    let seen = dir.cluster(CLUSTER).unwrap();
    assert_eq!(seen.report, Some(serde_json::to_value(&report).unwrap()));
    assert_eq!(seen.storage.get("t1"), Some(&storage));
    // A report without storage (a VM cluster's) is taken too, and one whose
    // storage doesn't read is refused.
    let vms = json!({ "at": now, "tenants": [{ "tenant": "t1", "vms": 2 }] });
    client::push_report(&c.key, CLUSTER, &d, &vms, None, now)
        .await
        .unwrap();
    assert!(dir.cluster(CLUSTER).unwrap().storage.is_empty());
    let bad =
        json!({ "at": now, "tenants": [{ "tenant": "t1", "storage": { "volumes": "two" } }] });
    assert!(
        client::push_report(&c.key, CLUSTER, &d, &bad, None, now)
            .await
            .is_err()
    );

    // Access tokens and API tokens for the cluster.
    let at = dir.access_token("u1", CLUSTER, "s0").unwrap();
    assert_eq!(
        tokens::unverified_issuer(&at).as_deref(),
        Some(dir.issuer())
    );
    assert_eq!(
        tokens::verify_access_token(&at, &d, CLUSTER).unwrap()["sub"],
        "u1"
    );
    assert!(tokens::verify_access_token(&at, &d, "c2").is_none());
    let secret = dir.add_api_token("u1", CLUSTER, u64::MAX);
    let Feed::Changed(snap) = client::pull_feed(&c.key, CLUSTER, &d, "", now)
        .await
        .unwrap()
    else {
        panic!("a feed");
    };
    let hash = tokens::api_token_hash(&secret);
    let (_, tok) = feed::rows::<feed::ApiToken>(&snap, "api_tokens")
        .find(|(k, _)| *k == hash)
        .unwrap();
    assert!(tok.good_on(CLUSTER));

    // A browser login: authorize, the code exchanged with a client
    // assertion, the ID token checked.
    dir.login_as(Some("u1"), true);
    let (state, nonce, verifier) = (
        glidex_trust::random_token(16),
        glidex_trust::random_token(16),
        glidex_trust::random_token(32),
    );
    let url = client::authorize_url(&d, CLUSTER, CALLBACK, &state, &nonce, &verifier).unwrap();
    let back = url::Url::parse(&dir.browse(&url).unwrap()).unwrap();
    assert!(back.as_str().starts_with(CALLBACK));
    let q: std::collections::HashMap<String, String> = back.query_pairs().into_owned().collect();
    assert_eq!(q["state"], state);
    // A wrong verifier is refused, and spends the code.
    let (s, _) = client::exchange_code(&c.key, CLUSTER, &d, &q["code"], CALLBACK, "wrong", now)
        .await
        .unwrap();
    assert_eq!(s, 400);
    let back = url::Url::parse(&dir.browse(&url).unwrap()).unwrap();
    let q: std::collections::HashMap<String, String> = back.query_pairs().into_owned().collect();
    let (s, answer) =
        client::exchange_code(&c.key, CLUSTER, &d, &q["code"], CALLBACK, &verifier, now)
            .await
            .unwrap();
    assert_eq!(s, 200, "{answer}");
    let id = tokens::verify_id_token(
        answer["id_token"].as_str().unwrap(),
        &d,
        &client::client_id(CLUSTER),
        &nonce,
    )
    .unwrap();
    assert_eq!(id["sub"], "u1");
    assert!(tokens::mfa_of(&id));
    let sid = id["sid"].as_str().unwrap().to_string();
    // Nobody logged in: the directory sends the browser back with an error.
    dir.login_as(None, false);
    assert!(dir.browse(&url).unwrap().contains("error=access_denied"));

    // Logging out at the cluster ends the chain there; the directory's
    // feed tells of chains it ended.
    client::logout(&c.key, CLUSTER, &d, &sid, now)
        .await
        .unwrap();
    assert_eq!(dir.logouts(), vec![(CLUSTER.to_string(), sid.clone())]);
    dir.end_chain("s0");
    let Feed::Changed(snap) = client::pull_feed(&c.key, CLUSTER, &d, "", now)
        .await
        .unwrap()
    else {
        panic!("a feed");
    };
    assert_eq!(snap.ended_sids, vec!["s0".to_string()]);

    // A partition: feeds fail, and credentials go stale past max_staleness.
    dir.set_down(true);
    assert!(
        client::pull_feed(&c.key, CLUSTER, &d, "", now)
            .await
            .is_err()
    );
    let st = FeedState {
        checked_at: now,
        ..Default::default()
    };
    assert!(cluster::freshness(&d, &st, 60, now + 3_600).is_ok());
    assert!(cluster::freshness(&d, &st, 60, now + 3_601).is_err());
    dir.set_down(false);

    // The directory ends trust: the cluster forgets it.
    assert_eq!(dir.end_trust(CLUSTER, "t1").await.unwrap(), 200);
    assert!(c.trusted.lock().unwrap().is_none());
    assert!(dir.cluster(CLUSTER).is_none());
}

#[tokio::test]
async fn the_cluster_ends_trust_at_the_directory() {
    let dir = FakeDirectory::start("acme").await.unwrap();
    let tenant = dir.add_tenant("t1", "acme");
    let c = Arc::new(Cluster {
        key: ClusterKey::generate("c1:1").unwrap(),
        secret_hash: glidex_trust::sha256_hex(b"s"),
        offer_open: Mutex::new(true),
        replays: Replays::default(),
        trusted: Mutex::new(None),
    });
    let (c2, iss) = (c.clone(), dir.issuer().to_string());
    let handler: Handler = Arc::new(move |req| {
        let (c, iss) = (c2.clone(), iss.clone());
        Box::pin(async move { c.handle(req, &iss) })
    });
    let https = serve_https(handler).await.unwrap();
    let offer = OfferToken {
        cluster_id: CLUSTER.into(),
        cluster_name: "north".into(),
        api: https.base.clone(),
        tls_sha256: https.pin.clone(),
        tenant: "acme".into(),
        directory: dir.issuer().into(),
        offer: "o1".into(),
        secret: "s".into(),
        expires_at: u64::MAX,
    };
    // An offer for another directory or tenant isn't sent.
    let mut other = offer.clone();
    other.directory = "https://elsewhere.example/d/x".into();
    assert!(dir.redeem(&other.encode(), &tenant).await.is_err());
    dir.redeem(&offer.encode(), &tenant).await.unwrap();
    let d = trusted(&c);
    let s = client::end_trust(&c.key, CLUSTER, &d, "t1", glidex_trust::now())
        .await
        .unwrap();
    assert_eq!(s, 200);
    assert!(dir.cluster(CLUSTER).is_none());
    // Its feed is closed to the cluster now.
    assert!(
        client::pull_feed(&c.key, CLUSTER, &d, "", glidex_trust::now())
            .await
            .is_err()
    );
}

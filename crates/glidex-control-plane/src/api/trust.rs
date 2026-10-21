//! The handshake's HTTP side (spec/tenancy.md D63–D66), public and
//! authenticated by peer assertions instead of a user:
//!
//! - at a cluster: `POST /trust/v1/redeem` (a directory redeems an offer)
//!   and `POST /trust/v1/end` (a trusted directory ends a tenant's trust);
//! - at a directory: `POST /d/{dir}/trust/v1/end` (a trusted cluster ends
//!   it).
//!
//! Every refusal answers the same way; the reason goes to the audit log.

use super::{AppState, err};
use crate::directory::issuer::CLUSTER_OWNER;
use crate::directory::peer;
use crate::trust::{self, RemoteTenant, TrustedDirectory};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

fn now() -> u64 {
    crate::auth::store::now()
}

/// The one answer to a refused peer request; `why` goes to the audit log.
fn refused(app: &AppState, action: &str, why: &str) -> Response {
    app.auth.audit_system(
        "peer",
        "",
        action,
        None,
        "denied",
        Some("trust_refused"),
        json!({ "why": why }),
    );
    err(
        StatusCode::FORBIDDEN,
        "trust_refused",
        "the request was refused",
    )
    .into_response()
}

#[derive(Deserialize)]
pub struct Redeem {
    offer: String,
    secret: String,
    issuer: String,
    directory: String,
    tenant: RemoteTenant,
    jwks: Value,
    api: String,
    tls_sha256: String,
}

/// `POST /trust/v1/redeem`, at the cluster (D65).
pub async fn redeem(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<Redeem>,
) -> Response {
    const ACTION: &str = "redeemOffer";
    let t = now();
    let Some(assertion) = peer::from_headers(&headers) else {
        return refused(&app, ACTION, "no assertion");
    };
    let offers = app.auth.tenants.offers().unwrap_or_default();
    let Some(mut offer) = offers.into_iter().find(|o| o.id == b.offer) else {
        return refused(&app, ACTION, "no such offer");
    };
    if !offer.is_open(t) {
        return refused(&app, ACTION, "the offer was redeemed, or expired");
    }
    if !crate::auth::constant_eq(
        &crate::auth::sha256_hex(b.secret.as_bytes()),
        &offer.secret_hash,
    ) {
        return refused(&app, ACTION, "the wrong secret");
    }
    let Some(issuer) = trust::normalize_issuer(&b.issuer) else {
        return refused(&app, ACTION, "not an issuer URL");
    };
    if issuer != offer.directory {
        return refused(&app, ACTION, "another directory than the offer's");
    }
    if b.tenant.slug != offer.tenant || b.tenant.id.is_empty() {
        return refused(&app, ACTION, "another tenant than the offer's");
    }
    let Some(pin) = glidex_tls::Trust::normalize_pin(&b.tls_sha256) else {
        return refused(&app, ACTION, "no certificate fingerprint");
    };
    let cluster = super::op::cluster_id(&app);
    if let Err(e) = peer::verify(
        &assertion,
        &b.jwks,
        &issuer,
        &cluster,
        trust::REDEEM,
        &app.auth.peer_replays,
        t,
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    // Once: the offer is spent before anything else is written.
    offer.redeemed_at = Some(t);
    if let Err(e) = app.auth.tenants.put_offer(&offer) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()).into_response();
    }
    let mut d = app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .ok()
        .flatten()
        .unwrap_or_else(|| TrustedDirectory {
            id: trust::trusted_id(&issuer),
            issuer: issuer.clone(),
            directory: b.directory.clone(),
            api: String::new(),
            tls_sha256: String::new(),
            jwks: json!({ "keys": [] }),
            tenants: Vec::new(),
            created_at: t,
        });
    d.directory = b.directory.clone();
    d.api = b.api.clone();
    d.tls_sha256 = pin;
    d.jwks = b.jwks.clone();
    d.tenants.retain(|x| x.id != b.tenant.id);
    d.tenants.push(b.tenant.clone());
    let by = format!("offer:{}", offer.id);
    let done = app
        .auth
        .trust
        .put_directory(&d)
        .map_err(|e| e.to_string())
        .and_then(|_| {
            app.auth
                .tenants
                .admit_remote(&b.tenant.id, &d.id, offer.cap.clone(), &by)
                .map_err(|e| e.to_string())
        })
        .and_then(|_| {
            app.auth
                .connectors
                .maintain_signing_keys(CLUSTER_OWNER, t)
                .map_err(|e| e.to_string())
        })
        .and_then(|_| {
            app.auth
                .connectors
                .jwks(CLUSTER_OWNER, t)
                .map_err(|e| e.to_string())
        });
    let cluster_key = match done {
        Ok(k) => k,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e).into_response(),
    };
    app.auth.audit_system(
        &format!("directory:{issuer}"),
        "",
        ACTION,
        Some(&format!("tenant:{}", b.tenant.id)),
        "allowed",
        None,
        json!({ "offer": offer.id, "tenant": b.tenant.slug }),
    );
    Json(json!({ "cluster_id": cluster, "cluster_key": cluster_key })).into_response()
}

#[derive(Deserialize)]
pub struct EndAtCluster {
    issuer: String,
    tenant: String,
}

/// `POST /trust/v1/end`, at the cluster: a trusted directory ends a
/// tenant's trust (D66).
pub async fn end_at_cluster(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<EndAtCluster>,
) -> Response {
    const ACTION: &str = "endTrust";
    let Some(assertion) = peer::from_headers(&headers) else {
        return refused(&app, ACTION, "no assertion");
    };
    let Some(issuer) = trust::normalize_issuer(&b.issuer) else {
        return refused(&app, ACTION, "not an issuer URL");
    };
    let Ok(Some(mut d)) = app.auth.trust.directory_by_issuer(&issuer) else {
        return refused(&app, ACTION, "not a trusted directory");
    };
    let cluster = super::op::cluster_id(&app);
    if let Err(e) = peer::verify(
        &assertion,
        &d.jwks,
        &issuer,
        &cluster,
        trust::END,
        &app.auth.peer_replays,
        now(),
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    forget_tenant_here(&app, &mut d, &b.tenant);
    StatusCode::OK.into_response()
}

/// This cluster stops admitting `tenant` of trusted directory `d`; a
/// directory left with none is forgotten (D66).
pub fn forget_tenant_here(app: &AppState, d: &mut TrustedDirectory, tenant: &str) {
    let _ = app.auth.tenants.remove_admission(tenant);
    d.tenants.retain(|x| x.id != tenant);
    let _ = if d.tenants.is_empty() {
        let _ = app.auth.trust.drop_replica(&d.id);
        let _ = app.auth.reload_policies();
        app.auth.trust.remove_directory(&d.id)
    } else {
        app.auth.trust.put_directory(d)
    };
}

#[derive(Deserialize)]
pub struct EndAtDirectory {
    cluster_id: String,
    tenant: String,
}

/// `POST /d/{dir}/trust/v1/end`, at the directory: a trusted cluster
/// stops admitting one of its tenants (D66).
pub async fn end_at_directory(
    State(app): State<AppState>,
    Path(dir): Path<String>,
    headers: HeaderMap,
    Json(b): Json<EndAtDirectory>,
) -> Response {
    const ACTION: &str = "endTrust";
    let Some(assertion) = peer::from_headers(&headers) else {
        return refused(&app, ACTION, "no assertion");
    };
    let Ok(Some(mut c)) = app.auth.trust.cluster(&dir, &b.cluster_id) else {
        return refused(&app, ACTION, "not a trusted cluster");
    };
    let Some(iss) = app
        .auth
        .tenants
        .get_directory(&dir)
        .ok()
        .flatten()
        .and_then(|d| d.issuer)
    else {
        return refused(&app, ACTION, "no issuer");
    };
    if let Err(e) = peer::verify(
        &assertion,
        &c.jwks,
        &c.cluster_id,
        &iss,
        trust::END,
        &app.auth.peer_replays,
        now(),
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    c.tenants.retain(|t| *t != b.tenant);
    let _ = if c.tenants.is_empty() {
        app.auth.trust.remove_cluster(&dir, &c.cluster_id)
    } else {
        app.auth.trust.put_cluster(&c)
    };
    StatusCode::OK.into_response()
}

/// Tell trusted directory `d` that this cluster no longer admits `tenant`
/// (D66); best effort: it also finds out when it is next refused.
pub async fn tell_directory_ended(app: &AppState, d: &TrustedDirectory, tenant: &str) {
    let cluster = super::op::cluster_id(app);
    let Ok(a) = peer::assertion(
        &app.auth.connectors,
        CLUSTER_OWNER,
        &cluster,
        &d.issuer,
        trust::END,
        now(),
    ) else {
        return;
    };
    let path = format!("/d/{}/trust/v1/end", d.directory);
    if let Err(e) = peer::post(
        &d.api,
        &d.tls_sha256,
        &path,
        &a,
        &json!({ "cluster_id": cluster, "tenant": tenant }),
    )
    .await
    {
        tracing::warn!(directory = %d.issuer, "telling the directory trust ended: {}", e);
    }
}

// ---- feeds (D67, D68) -----------------------------------------------------------

/// Directory `dir`'s filtered snapshot for trusted cluster `c` (D67): the
/// tenants opted in to it, their projects, members and role links, and the
/// users and teams those name, users and teams cut to what a cluster
/// needs.
pub fn snapshot_for(
    app: &AppState,
    dir: &str,
    c: &trust::TrustedCluster,
) -> Result<trust::Snapshot, String> {
    use crate::auth::store::{LinkRecord, Team, User};
    use crate::authz::Ent;
    let e = |x: crate::trust::StoreError| x.to_string();
    let ts = &app.auth.trust;
    let parse = |name: &str| ts.raw_rows(name).map_err(e);
    let tenants: Vec<(String, Value)> = parse("tenants")?
        .into_iter()
        .filter(|(k, v)| c.tenants.contains(k) && v["directory_id"] == dir)
        .collect();
    let tids: std::collections::HashSet<String> = tenants.iter().map(|(k, _)| k.clone()).collect();
    let projects: Vec<(String, Value)> = parse("projects")?
        .into_iter()
        .filter(|(_, v)| v["tenant_id"].as_str().is_some_and(|t| tids.contains(t)))
        .collect();
    let pids: std::collections::HashSet<String> = projects.iter().map(|(k, _)| k.clone()).collect();
    let members: Vec<(String, Value)> = parse("tenant_members")?
        .into_iter()
        .filter(|(_, v)| v["tenant_id"].as_str().is_some_and(|t| tids.contains(t)))
        .collect();
    let in_scope = |r: &Ent| match r {
        Ent::Tenant(t) => tids.contains(t),
        Ent::Project(p) => pids.contains(p),
        _ => false,
    };
    let links: Vec<(String, Value)> = parse("policy_links")?
        .into_iter()
        .filter(|(_, v)| {
            serde_json::from_value::<LinkRecord>(v.clone())
                .is_ok_and(|l| in_scope(&l.link.resource))
        })
        .collect();
    // The principals they name.
    let mut users: std::collections::BTreeSet<String> = Default::default();
    let mut teams: std::collections::BTreeSet<String> = Default::default();
    let mut name = |p: &Value| {
        if let Ok(p) = serde_json::from_value::<Ent>(p.clone()) {
            match p {
                Ent::User(u) => {
                    users.insert(u);
                }
                Ent::Team(t) => {
                    teams.insert(t);
                }
                _ => {}
            }
        }
    };
    for (_, m) in &members {
        name(&m["principal"]);
    }
    for (_, l) in &links {
        name(&l["principal"]);
    }
    let mut rows: Vec<trust::Row> = Vec::new();
    for (k, v) in parse("teams")? {
        let Ok(mut t) = serde_json::from_value::<Team>(v) else {
            continue;
        };
        if !teams.contains(&k) || t.directory_id != dir {
            continue;
        }
        t.managed = None;
        for m in &t.members {
            users.insert(m.user_id.clone());
        }
        rows.push(trust::Row {
            table: "teams".into(),
            key: k,
            value: serde_json::to_value(&t).unwrap_or_default(),
        });
    }
    for (k, v) in parse("users")? {
        let Ok(u) = serde_json::from_value::<User>(v) else {
            continue;
        };
        if !users.contains(&k) || u.directory_id != dir {
            continue;
        }
        let cut = User {
            id: u.id,
            directory_id: u.directory_id,
            display_name: u.display_name,
            disabled: u.disabled,
            default_project: None,
            created_at: u.created_at,
            managed: None,
            email: None,
            disabled_reason: None,
        };
        rows.push(trust::Row {
            table: "users".into(),
            key: k,
            value: serde_json::to_value(&cut).unwrap_or_default(),
        });
    }
    for (table, set) in [
        ("tenants", tenants),
        ("projects", projects),
        ("tenant_members", members),
        ("policy_links", links),
    ] {
        for (k, v) in set {
            rows.push(trust::Row {
                table: table.into(),
                key: k,
                value: v,
            });
        }
    }
    rows.sort_by(|a, b| (&a.table, &a.key).cmp(&(&b.table, &b.key)));
    let jwks = app
        .auth
        .connectors
        .jwks(dir, now())
        .map_err(|e| e.to_string())?;
    Ok(trust::Snapshot {
        revision: trust::revision_of(&jwks, &rows),
        jwks,
        rows,
    })
}

/// The rows of `snap` as this cluster keeps them for trusted directory
/// `d` (D68): only its admitted tenants' records, its users, teams and
/// tenants named as its; `Err` for anything outside that.
pub fn localize(d: &TrustedDirectory, snap: &trust::Snapshot) -> Result<trust::Snapshot, String> {
    let tids: std::collections::HashSet<&str> = d.tenants.iter().map(|t| t.id.as_str()).collect();
    let pids: std::collections::HashSet<String> = snap
        .rows
        .iter()
        .filter(|r| r.table == "projects")
        .map(|r| r.key.clone())
        .collect();
    let mut rows = Vec::new();
    for r in &snap.rows {
        let mut v = r.value.clone();
        let ok = match r.table.as_str() {
            "users" | "teams" => {
                v["directory_id"] = d.id.clone().into();
                v["id"] == r.key
            }
            "tenants" => {
                v["directory_id"] = d.id.clone().into();
                tids.contains(r.key.as_str()) && v["id"] == r.key
            }
            "projects" => v["tenant_id"].as_str().is_some_and(|t| tids.contains(t)),
            "tenant_members" => v["tenant_id"].as_str().is_some_and(|t| tids.contains(t)),
            // Only a tenant role on one of its tenants, or a project role
            // on one of their projects: nothing of this cluster's own.
            "policy_links" => {
                let res = &v["resource"];
                let role = v["template"].as_str().unwrap_or("");
                (res["type"] == "Tenant"
                    && crate::authz::TENANT_ROLES.contains(&role)
                    && res["id"].as_str().is_some_and(|t| tids.contains(t)))
                    || (res["type"] == "Project"
                        && crate::authz::PROJECT_ROLES.contains(&role)
                        && res["id"].as_str().is_some_and(|p| pids.contains(p)))
            }
            _ => false,
        };
        if !ok {
            return Err(format!(
                "a feed row outside its admitted tenants: {} {}",
                r.table, r.key
            ));
        }
        rows.push(trust::Row {
            table: r.table.clone(),
            key: r.key.clone(),
            value: v,
        });
    }
    Ok(trust::Snapshot {
        revision: snap.revision.clone(),
        jwks: snap.jwks.clone(),
        rows,
    })
}

#[derive(Deserialize)]
pub struct FeedAsk {
    cluster_id: String,
    #[serde(default)]
    revision: String,
}

/// `POST /d/{dir}/feed/v1 {cluster_id, revision}`, at the directory (D67):
/// the snapshot for that cluster, or `304` when its revision is current.
pub async fn feed(
    State(app): State<AppState>,
    Path(dir): Path<String>,
    headers: HeaderMap,
    Json(b): Json<FeedAsk>,
) -> Response {
    const ACTION: &str = "readFeed";
    let Some(assertion) = peer::from_headers(&headers) else {
        return refused(&app, ACTION, "no assertion");
    };
    let Ok(Some(c)) = app.auth.trust.cluster(&dir, &b.cluster_id) else {
        return refused(&app, ACTION, "not a trusted cluster");
    };
    let Some(iss) = app
        .auth
        .tenants
        .get_directory(&dir)
        .ok()
        .flatten()
        .and_then(|d| d.issuer)
    else {
        return refused(&app, ACTION, "no issuer");
    };
    if let Err(e) = peer::verify(
        &assertion,
        &c.jwks,
        &c.cluster_id,
        &iss,
        trust::FEED,
        &app.auth.peer_replays,
        now(),
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    match snapshot_for(&app, &dir, &c) {
        Ok(s) if s.revision == b.revision => StatusCode::NOT_MODIFIED.into_response(),
        Ok(s) => Json(s).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e).into_response(),
    }
}

/// Pull trusted directory `d`'s feed now and apply it (D67, D68).
/// `Ok(true)` when the replica changed.
pub async fn pull_feed(app: &AppState, d: &TrustedDirectory) -> Result<bool, String> {
    let cluster = super::op::cluster_id(app);
    let t = now();
    let state = app
        .auth
        .trust
        .feed_state(&d.id)
        .map_err(|e| e.to_string())?;
    let a = peer::assertion(
        &app.auth.connectors,
        CLUSTER_OWNER,
        &cluster,
        &d.issuer,
        trust::FEED,
        t,
    )
    .map_err(|e| e.to_string())?;
    let path = format!("/d/{}/feed/v1", d.directory);
    let (status, v) = peer::post(
        &d.api,
        &d.tls_sha256,
        &path,
        &a,
        &json!({ "cluster_id": cluster, "revision": state.revision }),
    )
    .await?;
    match status {
        304 => Ok(false),
        200 => {
            let snap: trust::Snapshot = serde_json::from_value(v).map_err(|e| e.to_string())?;
            if trust::revision_of(&snap.jwks, &snap.rows) != snap.revision {
                return Err("a feed whose revision doesn't match its content".into());
            }
            let local = localize(d, &snap)?;
            app.auth
                .trust
                .apply_feed(d, &local, t)
                .map_err(|e| e.to_string())?;
            // Its role links are policies now.
            app.auth.reload_policies().map_err(|e| e.to_string())?;
            // Its keys as the feed brings them (rotations, D53).
            if let Ok(Some(mut cur)) = app.auth.trust.directory_by_issuer(&d.issuer) {
                cur.jwks = snap.jwks.clone();
                let _ = app.auth.trust.put_directory(&cur);
            }
            Ok(true)
        }
        s => Err(format!("the directory answered {s}")),
    }
}

/// How often a cluster pulls its trusted directories' feeds.
const FEED_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// The task that pulls every trusted directory's feed: on a standalone
/// host, or the cluster's leader (where writes go).
pub fn spawn_feeds(app: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(FEED_EVERY);
        loop {
            tick.tick().await;
            if !app.auth.tenants.can_write() || app.auth.is_disabled() {
                continue;
            }
            for d in app.auth.trust.directories().unwrap_or_default() {
                if let Err(e) = pull_feed(&app, &d).await {
                    tracing::warn!(directory = %d.issuer, "feed: {}", e);
                }
            }
        }
    });
}

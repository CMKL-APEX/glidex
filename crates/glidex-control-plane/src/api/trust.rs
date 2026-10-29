//! The handshake's HTTP side (spec/19-tenancy.md D63–D66), public and
//! authenticated by peer assertions instead of a user:
//!
//! - at a cluster: `POST /trust/v1/redeem` (a directory redeems an offer)
//!   and `POST /trust/v1/end` (a trusted directory ends a tenant's trust);
//! - at a directory: `POST /d/{dir}/trust/v1/end` (a trusted cluster ends
//!   it) and `POST /d/{dir}/trust/v1/logout` (a session there ended, D72).
//!
//! Every refusal answers the same way; the reason goes to the audit log.

use super::{AppState, err};
use crate::directory::issuer::CLUSTER_OWNER;
use crate::directory::{op, peer};
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
            max_staleness_minutes: None,
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
    // Where the directory sends this cluster's browser logins back (D69).
    let ui_callback = super::trusted_login::callback_url(&app, &d.id);
    Json(json!({ "cluster_id": cluster, "cluster_key": cluster_key, "ui_callback": ui_callback }))
        .into_response()
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
    // API tokens listing this cluster (§7.4), of the people (as sent) and
    // projects it gets: their hash, owner, kind, lifetime and list only.
    let sent: std::collections::HashSet<String> = rows
        .iter()
        .filter(|r| r.table == "users")
        .map(|r| r.key.clone())
        .collect();
    for (k, v) in parse("api_tokens")? {
        let Ok(t) = serde_json::from_value::<crate::auth::store::Token>(v) else {
            continue;
        };
        let ours = match &t.kind {
            crate::auth::store::TokenKind::Personal { owner } => sent.contains(owner),
            crate::auth::store::TokenKind::ServiceAccount { project } => pids.contains(project),
        };
        if !ours || t.clusters.is_empty() || !t.good_on(&c.cluster_id, false) {
            continue;
        }
        let cut = crate::auth::store::Token {
            last_used_at: None,
            last_used_from: None,
            device: None,
            client: None,
            ..t
        };
        rows.push(trust::Row {
            table: "api_tokens".into(),
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
    let client = format!("{}{}", op::CLUSTER_CLIENT, c.cluster_id);
    let ended_sids = app
        .auth
        .refresh_tokens
        .ended_since(dir, &client, now().saturating_sub(op::ENDED_KEEP_SECS))
        .map_err(|e| e.to_string())?;
    Ok(trust::Snapshot {
        revision: trust::revision_of(&jwks, &rows, &ended_sids),
        jwks,
        rows,
        ended_sids,
    })
}

/// The rows of `snap` as this cluster keeps them for trusted directory
/// `d` (D68): only its admitted tenants' records, its users, teams and
/// tenants named as its; `Err` for anything outside that.
pub fn localize(
    d: &TrustedDirectory,
    snap: &trust::Snapshot,
    cluster: &str,
) -> Result<trust::Snapshot, String> {
    let tids: std::collections::HashSet<&str> = d.tenants.iter().map(|t| t.id.as_str()).collect();
    let pids: std::collections::HashSet<String> = snap
        .rows
        .iter()
        .filter(|r| r.table == "projects")
        .map(|r| r.key.clone())
        .collect();
    let users: std::collections::HashSet<String> = snap
        .rows
        .iter()
        .filter(|r| r.table == "users")
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
            // Its people's and projects' tokens, listing this cluster.
            "api_tokens" => serde_json::from_value::<crate::auth::store::Token>(v.clone())
                .is_ok_and(|t| {
                    let whose = match &t.kind {
                        crate::auth::store::TokenKind::Personal { owner } => users.contains(owner),
                        crate::auth::store::TokenKind::ServiceAccount { project } => {
                            pids.contains(project)
                        }
                    };
                    whose && !t.clusters.is_empty() && t.good_on(cluster, false)
                }),
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
        ended_sids: snap.ended_sids.clone(),
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
        304 => {
            app.auth
                .trust
                .mark_checked(&d.id, t)
                .map_err(|e| e.to_string())?;
            Ok(false)
        }
        200 => {
            let snap: trust::Snapshot = serde_json::from_value(v).map_err(|e| e.to_string())?;
            if trust::revision_of(&snap.jwks, &snap.rows, &snap.ended_sids) != snap.revision {
                return Err("a feed whose revision doesn't match its content".into());
            }
            let local = localize(d, &snap, &cluster)?;
            app.auth
                .trust
                .apply_feed(d, &local, t)
                .map_err(|e| e.to_string())?;
            // Its role links are policies now.
            app.auth.reload_policies().map_err(|e| e.to_string())?;
            // Sessions made from chains it ended end (D69), only its own
            // users'; its users gone or disabled have none already.
            if !snap.ended_sids.is_empty() {
                let ended: std::collections::HashSet<&String> = snap.ended_sids.iter().collect();
                let users: std::collections::HashSet<&String> = local
                    .rows
                    .iter()
                    .filter(|r| r.table == "users")
                    .map(|r| &r.key)
                    .collect();
                let n = app
                    .auth
                    .store
                    .prune_sessions(|s| {
                        users.contains(&s.user_id)
                            && s.sid.as_ref().is_some_and(|x| ended.contains(x))
                    })
                    .map_err(|e| e.to_string())?;
                if n > 0 {
                    tracing::info!(directory = %d.issuer, sessions = n, "ended with their directory chains");
                }
            }
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

// ---- reports (D70) -----------------------------------------------------------------

/// What this cluster holds of trusted directory `d`'s admitted tenants
/// (D70): each one's admission and the use of its projects here.
pub async fn report_for(
    app: &AppState,
    d: &TrustedDirectory,
) -> Result<trust::ClusterReport, String> {
    let projects = app.manager.projects().list().map_err(|e| e.to_string())?;
    let disks = app.manager.list_disks();
    let mut tenants = Vec::new();
    for rt in &d.tenants {
        let Some(a) = app
            .auth
            .tenants
            .admission(&rt.id)
            .map_err(|e| e.to_string())?
            .filter(|a| a.directory_id == d.id)
        else {
            continue;
        };
        let mut r = trust::TenantReport {
            tenant: rt.id.clone(),
            suspended: a.status == crate::tenants::AdmissionStatus::Suspended,
            cap: a.cap.clone(),
            projects: 0,
            vms: 0,
            running_vms: 0,
            disks: 0,
            disk_gib: 0,
            networks: 0,
        };
        for p in projects.iter().filter(|p| p.tenant_id == rt.id) {
            let u = app.manager.usage(&p.id).await.map_err(|e| e.to_string())?;
            r.projects += 1;
            r.vms += u.vms;
            r.running_vms += u.running_vms;
            r.disk_gib += u.disk_gib;
            r.networks += u.networks;
            r.disks += disks.iter().filter(|x| x.project == p.id).count() as u64;
        }
        tenants.push(r);
    }
    Ok(trust::ClusterReport { at: now(), tenants })
}

/// Send trusted directory `d` this cluster's report (D70).
pub async fn push_report(app: &AppState, d: &TrustedDirectory) -> Result<(), String> {
    let cluster = super::op::cluster_id(app);
    let report = report_for(app, d).await?;
    let a = peer::assertion(
        &app.auth.connectors,
        CLUSTER_OWNER,
        &cluster,
        &d.issuer,
        trust::REPORT,
        now(),
    )
    .map_err(|e| e.to_string())?;
    let path = format!("/d/{}/feed/v1/report", d.directory);
    // The callback as it is now, should the web UI's address have changed (D73).
    let ui_callback = super::trusted_login::callback_url(app, &d.id);
    let body = json!({ "cluster_id": cluster, "report": report, "ui_callback": ui_callback });
    match peer::post(&d.api, &d.tls_sha256, &path, &a, &body).await? {
        (204, _) | (200, _) => Ok(()),
        (s, _) => Err(format!("the directory answered {s}")),
    }
}

#[derive(Deserialize)]
pub struct ReportBody {
    cluster_id: String,
    report: trust::ClusterReport,
    #[serde(default)]
    ui_callback: Option<String>,
}

/// `POST /d/{dir}/feed/v1/report {cluster_id, report, ui_callback}`, at
/// the directory (D70): kept on the cluster's record, only for the tenants
/// opted in to it; a changed callback registered in its place (D73).
pub async fn report(
    State(app): State<AppState>,
    Path(dir): Path<String>,
    headers: HeaderMap,
    Json(b): Json<ReportBody>,
) -> Response {
    const ACTION: &str = "reportFeed";
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
        trust::REPORT,
        &app.auth.peer_replays,
        now(),
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    let mut r = b.report;
    // Its own clock, not the cluster's.
    r.at = now();
    r.tenants.retain(|t| c.tenants.contains(&t.tenant));
    r.tenants.truncate(c.tenants.len());
    c.report = Some(r);
    if let Some(u) = b.ui_callback.filter(|u| valid_callback(u))
        && c.ui_callback.as_deref() != Some(u.as_str())
    {
        app.auth.audit_system(
            &format!("cluster:{}", c.cluster_id),
            "",
            "registerCallback",
            Some(&format!("cluster:{}", c.cluster_id)),
            "allowed",
            None,
            json!({ "from": c.ui_callback, "to": u }),
        );
        c.ui_callback = Some(u);
    }
    match app.auth.trust.put_cluster(&c) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()).into_response(),
    }
}

/// A web UI callback a cluster may register: an `http(s)` URL.
pub fn valid_callback(u: &str) -> bool {
    url::Url::parse(u).is_ok_and(|u| matches!(u.scheme(), "https" | "http"))
}

// ---- logouts (D72) -----------------------------------------------------------------

/// Tell the trusted directories whose cluster sessions ended here to end
/// those chains (D72), best effort: a chain not ended lapses there unused.
pub async fn end_chains(app: &AppState) {
    let ended = app.auth.ended_chains.take();
    if ended.is_empty() {
        return;
    }
    let cluster = super::op::cluster_id(app);
    let dirs = app.auth.trust.directories().unwrap_or_default();
    for (id, sid) in ended {
        let Some(d) = dirs.iter().find(|d| d.id == id) else {
            continue;
        };
        let sent = async {
            let a = peer::assertion(
                &app.auth.connectors,
                CLUSTER_OWNER,
                &cluster,
                &d.issuer,
                trust::LOGOUT,
                now(),
            )
            .map_err(|e| e.to_string())?;
            let path = format!("/d/{}/trust/v1/logout", d.directory);
            let body = json!({ "cluster_id": cluster, "sid": sid });
            match peer::post(&d.api, &d.tls_sha256, &path, &a, &body).await? {
                (204, _) | (200, _) => Ok(()),
                (s, _) => Err(format!("the directory answered {s}")),
            }
        };
        if let Err(e) = sent.await {
            tracing::warn!(directory = %d.issuer, "ending a logged-out session's chain: {}", e);
        }
    }
}

#[derive(Deserialize)]
pub struct LogoutBody {
    cluster_id: String,
    sid: String,
}

/// `POST /d/{dir}/trust/v1/logout {cluster_id, sid}`, at the directory
/// (D72): a trusted cluster's session from chain `sid` ended, so the chain
/// ends; only that cluster's own chains. `204` whether or not it was alive.
pub async fn logout_at_directory(
    State(app): State<AppState>,
    Path(dir): Path<String>,
    headers: HeaderMap,
    Json(b): Json<LogoutBody>,
) -> Response {
    const ACTION: &str = "endClusterSession";
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
        trust::LOGOUT,
        &app.auth.peer_replays,
        now(),
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    let client = format!("{}{}", op::CLUSTER_CLIENT, c.cluster_id);
    match app
        .auth
        .refresh_tokens
        .end_client_family(&dir, &client, &b.sid)
    {
        Ok(n) => {
            if n > 0 {
                app.auth.audit_system(
                    &format!("cluster:{}", c.cluster_id),
                    "",
                    ACTION,
                    None,
                    "allowed",
                    None,
                    json!({ "sid": b.sid }),
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()).into_response(),
    }
}

// ---- staleness (D13) ---------------------------------------------------------------

/// A trusted directory's replica counts as behind past this: three
/// missed pulls.
pub const BEHIND_SECS: u64 = 90;

/// Whether trusted directory `id`'s credentials are still good here
/// (D13): `Ok(lag)` in seconds, or `Err(lag)` past its `max_staleness`
/// (`u64::MAX` when it can't be told: refused). `None` for an id that
/// isn't a trusted directory's.
pub fn freshness(app: &AppState, id: &str) -> Option<Result<u64, u64>> {
    if !id.starts_with("trusted-") {
        return None;
    }
    let d = app
        .auth
        .trust
        .directories()
        .ok()
        .and_then(|ds| ds.into_iter().find(|d| d.id == id));
    let Some(d) = d else {
        return Some(Err(u64::MAX));
    };
    let Ok(lag) = app.auth.trust.lag(&d, now()) else {
        return Some(Err(u64::MAX));
    };
    let max = d
        .max_staleness_minutes
        .unwrap_or(app.auth.config.trust.default_max_staleness_minutes)
        * 60;
    Some(if lag > max { Err(lag) } else { Ok(lag) })
}

/// How often a cluster pulls its trusted directories' feeds.
const FEED_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// The task that pulls every trusted directory's feed: on a standalone
/// host, or the cluster's leader (where writes go). And, on every server,
/// the one that tells directories of the sessions that ended (D72).
pub fn spawn_feeds(app: AppState) {
    let logouts = app.clone();
    tokio::spawn(async move {
        loop {
            logouts.auth.ended_chains.wait().await;
            end_chains(&logouts).await;
        }
    });
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
                    continue;
                }
                // And what it holds of the directory's tenants (D70).
                if let Err(e) = push_report(&app, &d).await {
                    tracing::warn!(directory = %d.issuer, "report: {}", e);
                }
            }
        }
    });
}

//! SCIM 2.0 endpoints (spec/tenancy.md §8.1, D41–D43): `/scim/v2/c/{id}/…`,
//! called by a connector's IdP with one of its `gxs_` tokens. REST only
//! (D29). Every request that writes does it in one store write, and is
//! audited as `system:scim:<connector>`. The protocol is in
//! `directory::scim`.

use super::{AppState, RequestId};
use crate::auth::store::{DisabledReason, ScimGroup, ScimTx, User};
use crate::directory::scim::{self as p, GroupAttrs, ScimError, UserAttrs};
use crate::directory::{Connector, GroupMode, Provisioning};
use axum::{
    Extension,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Requests per second per connector, and the burst (§8.1).
const RATE: f64 = 20.0;
const BURST: f64 = 40.0;

fn buckets() -> &'static Mutex<HashMap<String, (Instant, f64)>> {
    static B: std::sync::OnceLock<Mutex<HashMap<String, (Instant, f64)>>> =
        std::sync::OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `connector` may make one more request now.
fn take(connector: &str) -> bool {
    let mut b = buckets().lock().unwrap();
    let now = Instant::now();
    let (at, level) = b.entry(connector.to_string()).or_insert((now, BURST));
    *level = (*level + now.duration_since(*at).as_secs_f64() * RATE).min(BURST);
    *at = now;
    if *level >= 1.0 {
        *level -= 1.0;
        true
    } else {
        false
    }
}

fn reply(status: u16, v: Value) -> Response {
    let mut r = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        serde_json::to_vec(&v).unwrap_or_default(),
    )
        .into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/scim+json"),
    );
    r
}

fn error(e: &ScimError) -> Response {
    let mut r = reply(e.status, e.body());
    if e.status == 429 {
        r.headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
    }
    if e.status == 401 {
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
    }
    r
}

/// The endpoint's URL, as the IdP reached it (directly, or through the
/// web UI's `/api`), for `meta.location`.
fn base(headers: &HeaderMap, connector: &str) -> String {
    let h = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    let path = crate::service::directory::scim_path(connector);
    match (h("x-forwarded-host"), h("host")) {
        (Some(fwd), _) => format!("https://{fwd}/api{path}"),
        (None, Some(host)) => format!("https://{host}{path}"),
        (None, None) => path,
    }
}

/// The connector the token is for, or the SCIM error to answer.
fn authorize(app: &AppState, headers: &HeaderMap, id: &str) -> Result<Connector, ScimError> {
    let unauthorized = || {
        ScimError::new(
            401,
            None,
            "a valid SCIM token for this connector is required",
        )
    };
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(s, _)| s.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim().to_string())
        .ok_or_else(unauthorized)?;
    let c = app
        .auth
        .connectors
        .get(id)
        .ok()
        .flatten()
        .ok_or_else(unauthorized)?;
    app.auth
        .scim_tokens
        .verify(&c.id, &token)
        .map_err(|e| {
            tracing::error!("SCIM token check: {e}");
            ScimError::new(500, None, "storage error")
        })?
        .ok_or_else(unauthorized)?;
    if !c.enabled || c.provisioning != Provisioning::Scim {
        return Err(ScimError::new(
            403,
            None,
            "this connector isn't provisioned by SCIM",
        ));
    }
    if !take(&c.id) {
        return Err(ScimError::new(429, Some("tooMany"), "too many requests"));
    }
    Ok(c)
}

fn json_body(body: &Bytes) -> Result<Value, ScimError> {
    serde_json::from_slice(body).map_err(|_| ScimError::syntax("the body isn't JSON"))
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    filter: Option<String>,
    start_index: Option<usize>,
    count: Option<usize>,
}

/// What one request did, for its audit entry.
struct Done {
    status: u16,
    body: Value,
    action: &'static str,
    target: Option<String>,
}

fn audit(
    app: &AppState,
    rid: &str,
    c: &str,
    action: &str,
    target: Option<&str>,
    e: Option<&ScimError>,
) {
    app.auth.audit_system(
        &format!("system:scim:{c}"),
        rid,
        action,
        target,
        if e.is_some() { "error" } else { "ok" },
        e.map(|e| e.scim_type.unwrap_or("error")),
        serde_json::json!({ "status": e.map(|e| e.status).unwrap_or(200) }),
    );
}

/// Run `f` for an authorized request; audit it when `write`.
fn run(
    app: &AppState,
    rid: &str,
    headers: &HeaderMap,
    id: &str,
    write: Option<&'static str>,
    f: impl FnOnce(&Connector, &str) -> Result<Done, ScimError>,
) -> Response {
    let c = match authorize(app, headers, id) {
        Ok(c) => c,
        Err(e) => return error(&e),
    };
    let base = base(headers, &c.id);
    match f(&c, &base) {
        Ok(d) => {
            if write.is_some() {
                audit(app, rid, &c.id, d.action, d.target.as_deref(), None);
            }
            if d.status == 204 {
                StatusCode::NO_CONTENT.into_response()
            } else {
                reply(d.status, d.body)
            }
        }
        Err(e) => {
            if let Some(a) = write {
                audit(app, rid, &c.id, a, None, Some(&e));
            }
            error(&e)
        }
    }
}

// ---- discovery ---------------------------------------------------------

pub async fn service_provider_config(
    State(app): State<AppState>,
    Extension(RequestId(rid)): Extension<RequestId>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    run(&app, &rid, &headers, &id, None, |_, base| {
        Ok(read(p::service_provider_config(base)))
    })
}

pub async fn schemas(
    State(app): State<AppState>,
    Extension(RequestId(rid)): Extension<RequestId>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    run(&app, &rid, &headers, &id, None, |_, base| {
        Ok(read(p::schemas(base)))
    })
}

pub async fn resource_types(
    State(app): State<AppState>,
    Extension(RequestId(rid)): Extension<RequestId>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    run(&app, &rid, &headers, &id, None, |_, base| {
        Ok(read(p::resource_types(base)))
    })
}

fn read(body: Value) -> Done {
    Done {
        status: 200,
        body,
        action: "scimRead",
        target: None,
    }
}

// ---- users ---------------------------------------------------------------

/// The connector's users and their teams follow (D43), in this write.
fn sync(tx: &ScimTx<'_>, c: &Connector) -> Result<(), ScimError> {
    Ok(tx.sync_teams(c.group_mode == GroupMode::Auto, &c.group_teams)?)
}

/// `active` as SCIM set it (D42): off disables (D20); on enables only what
/// SCIM disabled.
fn set_active(tx: &ScimTx<'_>, u: &mut User, active: bool) -> Result<(), ScimError> {
    if !active {
        if !u.disabled || u.disabled_reason != Some(DisabledReason::Scim) {
            tx.put_user(u)?;
            if let Some((after, _)) = tx.deprovision(&u.id, DisabledReason::Scim)? {
                *u = after;
            }
            return Ok(());
        }
    } else if p::scim_disabled(u) {
        u.disabled = false;
        u.disabled_reason = None;
    }
    tx.put_user(u)?;
    Ok(())
}

fn unique_user_name(tx: &ScimTx<'_>, name: &str, except: Option<&str>) -> Result<(), ScimError> {
    if tx.users()?.iter().any(|u| {
        Some(u.id.as_str()) != except
            && u.managed
                .as_ref()
                .and_then(|m| m.user_name.as_deref())
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
    }) {
        return Err(ScimError::uniqueness(format!("userName {name} is taken")));
    }
    Ok(())
}

fn managed_user(tx: &ScimTx<'_>, id: &str) -> Result<User, ScimError> {
    tx.user(id)?
        .filter(|u| {
            u.managed
                .as_ref()
                .is_some_and(|m| m.connector == tx.connector)
        })
        .ok_or_else(|| ScimError::not_found("user"))
}

pub async fn users(
    State(app): State<AppState>,
    Extension(RequestId(rid)): Extension<RequestId>,
    method: Method,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method == Method::POST {
        return run(
            &app,
            &rid,
            &headers,
            &id,
            Some("scimCreateUser"),
            |c, base| {
                let a = UserAttrs::from_json(&json_body(&body)?)?;
                let provider = c.identity_provider();
                app.auth.store.scim(&c.directory_id, &c.id, |tx| {
                    unique_user_name(tx, &a.user_name, None)?;
                    // A connector that had JIT before SCIM: its user is adopted,
                    // not made twice.
                    let mut found = Vec::new();
                    for e in [Some(a.user_name.as_str()), a.email.as_deref()]
                        .into_iter()
                        .flatten()
                    {
                        for u in tx.unmanaged_with_email(&provider, e)? {
                            if !found.iter().any(|x: &User| x.id == u.id) {
                                found.push(u);
                            }
                        }
                    }
                    let mut u = match found.len() {
                        1 => found.remove(0),
                        _ => User {
                            id: uuid::Uuid::new_v4().to_string(),
                            directory_id: c.directory_id.clone(),
                            display_name: String::new(),
                            disabled: false,
                            default_project: None,
                            created_at: crate::auth::store::now(),
                            managed: None,
                            email: None,
                            disabled_reason: None,
                        },
                    };
                    a.apply(&mut u, &c.id);
                    set_active(tx, &mut u, a.active)?;
                    if let Some(t) = &c.default_tenant {
                        tx.join_tenant(t, &u.id)?;
                    }
                    sync(tx, c)?;
                    Ok::<_, ScimError>(Done {
                        status: 201,
                        body: p::user_json(&u, base),
                        action: "scimCreateUser",
                        target: Some(format!("user:{}", u.id)),
                    })
                })
            },
        );
    }
    run(&app, &rid, &headers, &id, None, |c, base| {
        let filter = q
            .filter
            .as_deref()
            .map(|f| p::parse_filter(f, &p::USER_FILTERS))
            .transpose()?;
        let (users, _) = app.auth.store.scim_read(&c.id)?;
        let mut users = users;
        users.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        let items = users
            .iter()
            .filter(|u| filter.as_ref().is_none_or(|f| p::user_matches(u, f)))
            .map(|u| p::user_json(u, base))
            .collect();
        Ok(read(p::page(items, q.start_index, q.count)))
    })
}

pub async fn user(
    State(app): State<AppState>,
    Extension(RequestId(rid)): Extension<RequestId>,
    method: Method,
    Path((id, uid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let action = match method {
        Method::PUT => Some("scimReplaceUser"),
        Method::PATCH => Some("scimUpdateUser"),
        Method::DELETE => Some("scimDeleteUser"),
        _ => None,
    };
    run(&app, &rid, &headers, &id, action, |c, base| {
        let target = Some(format!("user:{uid}"));
        if method == Method::GET {
            let (users, _) = app.auth.store.scim_read(&c.id)?;
            let u = users
                .iter()
                .find(|u| u.id == uid)
                .ok_or_else(|| ScimError::not_found("user"))?;
            return Ok(read(p::user_json(u, base)));
        }
        let parsed = if method == Method::DELETE {
            Value::Null
        } else {
            json_body(&body)?
        };
        app.auth.store.scim(&c.directory_id, &c.id, |tx| {
            let mut u = managed_user(tx, &uid)?;
            if method == Method::DELETE {
                // D42: disabled, and no longer the IdP's.
                tx.deprovision(&u.id, DisabledReason::Scim)?;
                let mut u = tx.user(&uid)?.ok_or_else(|| ScimError::not_found("user"))?;
                u.managed = None;
                tx.put_user(&u)?;
                tx.leave_groups(&uid)?;
                sync(tx, c)?;
                return Ok(Done {
                    status: 204,
                    body: Value::Null,
                    action: "scimDeleteUser",
                    target,
                });
            }
            let a = if method == Method::PUT {
                UserAttrs::from_json(&parsed)?
            } else {
                let mut a = UserAttrs::of(&u);
                p::patch_user(&mut a, &parsed)?;
                a
            };
            unique_user_name(tx, &a.user_name, Some(&u.id))?;
            a.apply(&mut u, &c.id);
            set_active(tx, &mut u, a.active)?;
            sync(tx, c)?;
            Ok::<_, ScimError>(Done {
                status: 200,
                body: p::user_json(&u, base),
                action: action.unwrap_or("scimUpdateUser"),
                target,
            })
        })
    })
}

// ---- groups --------------------------------------------------------------

/// A group's members are users this connector manages: others are dropped.
fn known_members(tx: &ScimTx<'_>, ids: Vec<String>) -> Result<Vec<String>, ScimError> {
    let mine: Vec<String> = tx.users()?.into_iter().map(|u| u.id).collect();
    Ok(ids.into_iter().filter(|m| mine.contains(m)).collect())
}

fn unique_group(tx: &ScimTx<'_>, name: &str, except: Option<&str>) -> Result<(), ScimError> {
    if tx
        .groups()?
        .iter()
        .any(|g| Some(g.id.as_str()) != except && g.display_name.eq_ignore_ascii_case(name))
    {
        return Err(ScimError::uniqueness(format!("group {name} exists")));
    }
    Ok(())
}

fn names(users: &[User]) -> impl Fn(&str) -> Option<String> + '_ {
    move |id| {
        users
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.display_name.clone())
    }
}

pub async fn groups(
    State(app): State<AppState>,
    Extension(RequestId(rid)): Extension<RequestId>,
    method: Method,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method == Method::POST {
        return run(
            &app,
            &rid,
            &headers,
            &id,
            Some("scimCreateGroup"),
            |c, base| {
                let a = GroupAttrs::from_json(&json_body(&body)?)?;
                app.auth.store.scim(&c.directory_id, &c.id, |tx| {
                    unique_group(tx, &a.display_name, None)?;
                    let now = crate::auth::store::now();
                    let g = ScimGroup {
                        id: uuid::Uuid::new_v4().to_string(),
                        connector: c.id.clone(),
                        display_name: a.display_name.clone(),
                        external_id: a.external_id.clone(),
                        members: known_members(tx, a.members.clone())?,
                        created_at: now,
                        updated_at: now,
                    };
                    tx.put_group(&g)?;
                    sync(tx, c)?;
                    let users = tx.users()?;
                    Ok::<_, ScimError>(Done {
                        status: 201,
                        body: p::group_json(&g, base, &names(&users)),
                        action: "scimCreateGroup",
                        target: Some(format!("scim-group:{}", g.id)),
                    })
                })
            },
        );
    }
    run(&app, &rid, &headers, &id, None, |c, base| {
        let filter = q
            .filter
            .as_deref()
            .map(|f| p::parse_filter(f, &p::GROUP_FILTERS))
            .transpose()?;
        let (users, mut groups) = app.auth.store.scim_read(&c.id)?;
        groups.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        let n = names(&users);
        let items = groups
            .iter()
            .filter(|g| filter.as_ref().is_none_or(|f| p::group_matches(g, f)))
            .map(|g| p::group_json(g, base, &n))
            .collect();
        Ok(read(p::page(items, q.start_index, q.count)))
    })
}

pub async fn group(
    State(app): State<AppState>,
    Extension(RequestId(rid)): Extension<RequestId>,
    method: Method,
    Path((id, gid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let action = match method {
        Method::PUT => Some("scimReplaceGroup"),
        Method::PATCH => Some("scimUpdateGroup"),
        Method::DELETE => Some("scimDeleteGroup"),
        _ => None,
    };
    run(&app, &rid, &headers, &id, action, |c, base| {
        let target = Some(format!("scim-group:{gid}"));
        if method == Method::GET {
            let (users, groups) = app.auth.store.scim_read(&c.id)?;
            let g = groups
                .iter()
                .find(|g| g.id == gid)
                .ok_or_else(|| ScimError::not_found("group"))?;
            return Ok(read(p::group_json(g, base, &names(&users))));
        }
        let parsed = if method == Method::DELETE {
            Value::Null
        } else {
            json_body(&body)?
        };
        app.auth.store.scim(&c.directory_id, &c.id, |tx| {
            let mut g = tx
                .groups()?
                .into_iter()
                .find(|g| g.id == gid)
                .ok_or_else(|| ScimError::not_found("group"))?;
            if method == Method::DELETE {
                tx.remove_group(&gid)?;
                sync(tx, c)?;
                return Ok(Done {
                    status: 204,
                    body: Value::Null,
                    action: "scimDeleteGroup",
                    target,
                });
            }
            let a = if method == Method::PUT {
                GroupAttrs::from_json(&parsed)?
            } else {
                let mut a = GroupAttrs::of(&g);
                p::patch_group(&mut a, &parsed)?;
                a
            };
            unique_group(tx, &a.display_name, Some(&g.id))?;
            g.display_name = a.display_name;
            g.external_id = a.external_id;
            g.members = known_members(tx, a.members)?;
            g.updated_at = crate::auth::store::now();
            tx.put_group(&g)?;
            sync(tx, c)?;
            let users = tx.users()?;
            Ok::<_, ScimError>(Done {
                status: 200,
                body: p::group_json(&g, base, &names(&users)),
                action: action.unwrap_or("scimUpdateGroup"),
                target,
            })
        })
    })
}

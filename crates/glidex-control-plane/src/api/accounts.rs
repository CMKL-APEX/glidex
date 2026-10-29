//! Accounts in one browser (spec/19-tenancy.md D71): one session per
//! directory, one active (`gx_session`); the others, and the directories
//! whose sessions ended, in `gx_accounts`, which only these endpoints see
//! (`Path=/api/auth/accounts`). Nothing here is stored or logged on the
//! server: each answer comes from the cookies its request carries.
//!
//! `gx_accounts` is a `.`-separated list of `s:<directory>:<session>`
//! (another account, listed signed out once its session ends) and
//! `d:<directory>` (a directory to sign in to again; also the active
//! account's, so it stays listed when its session ends).

use super::access::{secure_transport, session_cookie};
use super::{ApiErr, AppState, Listener, RequestId, err};
use crate::auth::store::User;
use crate::auth::{self, Principal, Transport};
use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

const JAR: &str = "gx_accounts";
const JAR_PATH: &str = "/api/auth/accounts";
/// The most directories a browser lists.
pub const MAX_ACCOUNTS: usize = 5;
/// How long the list outlives its last change.
const JAR_SECS: u64 = 30 * 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Session { directory: String, secret: String },
    Directory(String),
}

fn parse(v: &str) -> Vec<Entry> {
    v.split('.')
        .filter_map(|e| match e.split_once(':')? {
            ("s", rest) => {
                let (d, t) = rest.split_once(':')?;
                (valid_directory(d)
                    && !t.is_empty()
                    && t.bytes().all(|b| b.is_ascii_alphanumeric()))
                .then(|| Entry::Session {
                    directory: d.into(),
                    secret: t.into(),
                })
            }
            ("d", d) if valid_directory(d) => Some(Entry::Directory(d.into())),
            _ => None,
        })
        .collect()
}

fn valid_directory(d: &str) -> bool {
    (1..=64).contains(&d.len())
        && d.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What a browser holds: its active session and the other accounts, one
/// per directory.
struct Held {
    active: Option<(String, Principal)>,
    /// The other directories, in the list's order: a live session (its
    /// secret and user) or none.
    others: Vec<(String, Option<(String, User)>)>,
}

impl Held {
    fn active_dir(&self) -> Option<&str> {
        self.active.as_ref().map(|(_, p)| p.directory.as_str())
    }

    fn encode(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        if let Some(d) = self.active_dir() {
            out.push(format!("d:{d}"));
        }
        for (d, live) in &self.others {
            out.push(match live {
                Some((s, _)) => format!("s:{d}:{s}"),
                None => format!("d:{d}"),
            });
        }
        out.join(".")
    }

    fn count(&self) -> usize {
        self.others.len() + usize::from(self.active.is_some())
    }
}

/// Read the browser's accounts; sessions that ended become directories
/// to sign in to again, and a second session of the active account's
/// directory (an older login there) is ended.
fn held(app: &AppState, headers: &HeaderMap) -> Held {
    let active = super::cookie(headers, auth::SESSION_COOKIE).and_then(|c| {
        app.auth
            .session_principal(&c, Transport::Tcp, None)
            .ok()
            .flatten()
            .filter(|p| p.user.is_some())
            .map(|p| (c, p))
    });
    let mut h = Held {
        active,
        others: Vec::new(),
    };
    let entries = super::cookie(headers, JAR)
        .map(|v| parse(&v))
        .unwrap_or_default();
    for e in entries {
        let (dir, live) = match e {
            Entry::Directory(d) => (d, None),
            Entry::Session { directory, secret } => {
                match app.auth.peek_session(&secret).ok().flatten() {
                    // Its user's directory, not the cookie's word for it.
                    Some(u) => (u.directory_id.clone(), Some((secret, u))),
                    None => (directory, None),
                }
            }
        };
        if h.active_dir() == Some(dir.as_str()) {
            // One account per directory: the active one wins.
            if let Some((s, _)) = live {
                let _ = app
                    .auth
                    .end_session_hash(&crate::auth::sha256_hex(s.as_bytes()));
            }
            continue;
        }
        match h.others.iter_mut().find(|(d, _)| *d == dir) {
            // A live session wins over the directory alone.
            Some(slot) if slot.1.is_none() => slot.1 = live,
            Some(_) => {}
            None => h.others.push((dir, live)),
        }
    }
    h.others
        .truncate(MAX_ACCOUNTS - usize::from(h.active.is_some()));
    h
}

fn jar_cookie(value: &str, secure: bool) -> HeaderValue {
    let age = if value.is_empty() { 0 } else { JAR_SECS };
    let mut c = format!("{JAR}={value}; HttpOnly; SameSite=Strict; Path={JAR_PATH}; Max-Age={age}");
    if secure {
        c.push_str("; Secure");
    }
    HeaderValue::from_str(&c).expect("ascii")
}

fn secure(app: &AppState, headers: &HeaderMap, listener: Option<Extension<Listener>>) -> bool {
    secure_transport(app, headers, listener.map(|l| l.0).unwrap_or(Listener::Tcp))
}

/// A directory's name for its own people: a hosted directory's, or a
/// trusted directory's tenants' at its host.
fn directory_name(app: &AppState, dir: &str) -> (String, &'static str) {
    if dir.starts_with("trusted-")
        && let Ok(ds) = app.auth.trust.directories()
        && let Some(d) = ds.into_iter().find(|d| d.id == dir)
    {
        let host = url::Url::parse(&d.issuer)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_default();
        let names: Vec<String> = d.tenants.iter().map(|t| t.display_name.clone()).collect();
        let name = if names.is_empty() {
            host
        } else {
            format!("{} at {host}", names.join(", "))
        };
        return (name, "trusted");
    }
    // The host's own accounts (D23).
    if dir == crate::tenants::LOCAL_DIRECTORY {
        return ("This host".into(), "hosted");
    }
    match app.auth.tenants.get_directory(dir).ok().flatten() {
        Some(d) if !d.name.is_empty() => (d.name, "hosted"),
        _ => (dir.into(), "hosted"),
    }
}

fn listing(app: &AppState, h: &Held) -> Value {
    let one = |dir: &str, user: Option<&User>, active: bool| {
        let (name, kind) = directory_name(app, dir);
        json!({
            "directory": dir, "directory_name": name, "kind": kind,
            "user_name": user.map(|u| u.display_name.clone()),
            "signed_in": user.is_some(), "active": active,
        })
    };
    let mut accounts = Vec::new();
    if let Some((_, p)) = &h.active {
        accounts.push(one(&p.directory, p.user.as_ref(), true));
    }
    for (d, live) in &h.others {
        accounts.push(one(d, live.as_ref().map(|(_, u)| u), false));
    }
    json!({ "accounts": accounts, "max": MAX_ACCOUNTS })
}

/// The answer with the browser's cookies rewritten to `h`.
fn answer(
    app: &AppState,
    headers: &HeaderMap,
    listener: Option<Extension<Listener>>,
    h: &Held,
    session: Option<Option<&str>>,
    body: Value,
) -> Response {
    let secure = secure(app, headers, listener);
    let mut r = Json(body).into_response();
    r.headers_mut()
        .append(header::SET_COOKIE, jar_cookie(&h.encode(), secure));
    match session {
        Some(Some(s)) => {
            r.headers_mut()
                .append(header::SET_COOKIE, session_cookie(s, secure, None));
        }
        Some(None) => {
            r.headers_mut()
                .append(header::SET_COOKIE, session_cookie("", secure, Some(0)));
        }
        None => {}
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Writes come from the web UI's own pages: an allowed `Origin` (the API
/// middleware refuses any other).
fn same_origin(headers: &HeaderMap) -> Result<(), ApiErr> {
    if headers.get(header::ORIGIN).is_none() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "origin_required",
            "account changes come from the web UI",
        ));
    }
    Ok(())
}

/// `GET /auth/accounts`.
pub async fn list(
    State(app): State<AppState>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
) -> Response {
    let h = held(&app, &headers);
    answer(&app, &headers, listener, &h, None, listing(&app, &h))
}

#[derive(Deserialize)]
pub struct Pick {
    directory: String,
}

/// `POST /auth/accounts/switch {directory}`.
pub async fn switch(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Json(b): Json<Pick>,
) -> Result<Response, ApiErr> {
    same_origin(&headers)?;
    let mut h = held(&app, &headers);
    if h.active_dir() == Some(b.directory.as_str()) {
        let csrf = h.active.as_ref().and_then(|(_, p)| p.csrf.clone());
        let body = json!({ "csrf": csrf, "accounts": listing(&app, &h)["accounts"] });
        return Ok(answer(&app, &headers, listener, &h, None, body));
    }
    let Some(i) = h
        .others
        .iter()
        .position(|(d, live)| *d == b.directory && live.is_some())
    else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "not_signed_in",
            "this browser isn't signed in to that directory",
        ));
    };
    let (_, live) = h.others.remove(i);
    let (secret, _) = live.expect("live");
    let p = app
        .auth
        .session_principal(&secret, Transport::Tcp, None)
        .map_err(super::auth_error)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not_signed_in", "that session ended"))?;
    // The account switched from stays listed, signed in.
    if let Some((s, old)) = h.active.take() {
        h.others
            .insert(0, (old.directory.clone(), old.user.clone().map(|u| (s, u))));
    }
    app.auth.audit(
        Some(&p),
        &rid.0,
        "switchAccount",
        None,
        p.user.as_ref().map(|u| format!("user:{}", u.id)).as_deref(),
        "allowed",
        None,
        &[],
        json!({}),
    );
    let csrf = p.csrf.clone();
    h.active = Some((secret.clone(), p));
    let body = json!({ "csrf": csrf, "accounts": listing(&app, &h)["accounts"] });
    Ok(answer(
        &app,
        &headers,
        listener,
        &h,
        Some(Some(&secret)),
        body,
    ))
}

/// `POST /auth/accounts/add`: the active account into the list, for the
/// login page to sign in to another directory.
pub async fn add(
    State(app): State<AppState>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
) -> Result<Response, ApiErr> {
    same_origin(&headers)?;
    let mut h = held(&app, &headers);
    if h.count() >= MAX_ACCOUNTS {
        return Err(err(
            StatusCode::CONFLICT,
            "too_many_accounts",
            format!("a browser holds at most {MAX_ACCOUNTS} accounts: remove one first"),
        ));
    }
    if let Some((s, p)) = h.active.take() {
        h.others
            .insert(0, (p.directory.clone(), p.user.clone().map(|u| (s, u))));
    }
    let body = listing(&app, &h);
    Ok(answer(&app, &headers, listener, &h, Some(None), body))
}

/// `POST /auth/accounts/remove {directory}`.
pub async fn remove(
    State(app): State<AppState>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Json(b): Json<Pick>,
) -> Result<Response, ApiErr> {
    same_origin(&headers)?;
    let mut h = held(&app, &headers);
    let mut session = None;
    if h.active_dir() == Some(b.directory.as_str()) {
        let (s, _) = h.active.take().expect("active");
        app.auth
            .end_session_hash(&crate::auth::sha256_hex(s.as_bytes()))
            .map_err(super::auth_error)?;
        session = Some(None);
    }
    h.others.retain(|(d, live)| {
        if *d != b.directory {
            return true;
        }
        if let Some((s, _)) = live {
            let _ = app
                .auth
                .end_session_hash(&crate::auth::sha256_hex(s.as_bytes()));
        }
        false
    });
    let body = listing(&app, &h);
    Ok(answer(&app, &headers, listener, &h, session, body))
}

/// `POST /auth/accounts/logout-all`: every session ended, the list gone.
pub async fn logout_all(
    State(app): State<AppState>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
) -> Result<Response, ApiErr> {
    same_origin(&headers)?;
    let h = held(&app, &headers);
    let secrets = h.active.iter().map(|(s, _)| s.clone()).chain(
        h.others
            .iter()
            .filter_map(|(_, l)| l.as_ref().map(|(s, _)| s.clone())),
    );
    for s in secrets {
        app.auth
            .end_session_hash(&crate::auth::sha256_hex(s.as_bytes()))
            .map_err(super::auth_error)?;
    }
    let empty = Held {
        active: None,
        others: Vec::new(),
    };
    Ok(answer(
        &app,
        &headers,
        listener,
        &empty,
        Some(None),
        json!({ "accounts": [] }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_cookie_takes_only_what_it_writes() {
        assert_eq!(
            parse("d:local.s:beta:abc123.s:x:bad!.d:Upper.junk.d:"),
            vec![
                Entry::Directory("local".into()),
                Entry::Session {
                    directory: "beta".into(),
                    secret: "abc123".into()
                },
            ]
        );
    }
}

//! `auth login --directory` (spec/tenancy.md D57): a login at the
//! directory the profile's servers host, with its device grant. The
//! profile keeps the refresh token; before a command, an access token for
//! this cluster is asked for when the one held is about to expire.

use crate::client::{self, ApiClient};
use crate::config::{self, DirectoryLogin};
use crate::tenancy::err;
use colored::Colorize;
use hyper::Method;
use serde_json::{Value, json};
use zeroize::Zeroizing;

/// Refresh this long before the access token expires.
const EARLY_SECS: u64 = 120;

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// POST `path` with no credential (the token endpoint is public, and a
/// stale access token would be refused before it is read).
async fn post_bare(cl: &ApiClient, path: &str, body: &Value) -> Result<(u16, Value), String> {
    let held = cl.take_token();
    let r = cl.send(Method::POST, path, Some(body)).await;
    cl.set_token(held);
    let r = r.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    Ok((r.status.as_u16(), v))
}

/// The login's result: the access token, and what the profile keeps.
pub struct Login {
    pub access: Zeroizing<String>,
    pub keep: DirectoryLogin,
}

fn login_of(dir: &str, resource: &str, v: &Value) -> Option<Login> {
    let access = v["access_token"].as_str()?;
    let refresh = v["refresh_token"].as_str()?;
    Some(Login {
        access: Zeroizing::new(access.to_string()),
        keep: DirectoryLogin {
            id: dir.to_string(),
            refresh_token: refresh.to_string(),
            resource: resource.to_string(),
            access_expires_at: client::now_secs() + v["expires_in"].as_u64().unwrap_or(3600),
        },
    })
}

/// The device grant at directory `dir` of `cl`'s servers: print where to
/// approve it, wait, and return the tokens for `resource`.
pub async fn device_login(cl: &ApiClient, dir: &str, resource: &str) -> Option<Login> {
    let base = format!("/d/{}", client::enc(dir));
    let start = match post_bare(
        cl,
        &format!("{base}/device_authorization"),
        &json!({ "client_id": "gxctl", "scope": "openid offline_access" }),
    )
    .await
    {
        Ok((200, v)) => v,
        Ok((_, v)) => {
            err(format!(
                "the directory refused: {}",
                s(&v["error_description"])
            ));
            return None;
        }
        Err(e) => {
            err(e);
            return None;
        }
    };
    println!(
        "To log in, open:\n\n    {}\n",
        s(&start["verification_uri_complete"]).cyan().bold()
    );
    println!(
        "and check that it shows the code:  {}",
        s(&start["user_code"]).yellow().bold()
    );
    println!("Waiting for you to approve it (Ctrl-C to cancel)...");
    let mut interval = start["interval"].as_u64().unwrap_or(5).max(1);
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(start["expires_in"].as_u64().unwrap_or(600));
    let body = json!({
        "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        "client_id": "gxctl",
        "device_code": s(&start["device_code"]),
        "resource": resource,
    });
    while std::time::Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
            _ = tokio::signal::ctrl_c() => {
                println!("Cancelled");
                return None;
            }
        }
        match post_bare(cl, &format!("{base}/token"), &body).await {
            Ok((200, v)) => match login_of(dir, resource, &v) {
                Some(l) => return Some(l),
                None => {
                    err("the directory gave no access token");
                    return None;
                }
            },
            Ok((_, v)) => match s(&v["error"]) {
                "authorization_pending" => continue,
                "slow_down" => interval += 5,
                "access_denied" => {
                    err("the login was denied");
                    return None;
                }
                e => {
                    err(format!("{e}: {}", s(&v["error_description"])));
                    return None;
                }
            },
            Err(e) => {
                err(e);
                return None;
            }
        }
    }
    err("the login code expired; run 'auth login --directory' again");
    None
}

/// A new access token from the refresh token (which is replaced).
pub async fn refresh(cl: &ApiClient, d: &DirectoryLogin) -> Result<Login, String> {
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": "gxctl",
        "refresh_token": d.refresh_token,
        "resource": d.resource,
    });
    match post_bare(cl, &format!("/d/{}/token", client::enc(&d.id)), &body).await? {
        (200, v) => login_of(&d.id, &d.resource, &v)
            .ok_or_else(|| "the directory gave no access token".to_string()),
        (_, v) => Err(format!(
            "the directory login ended ({}); run 'gxctl auth login --directory' again",
            s(&v["error_description"])
        )),
    }
}

/// End the login at the directory (`revoke`).
pub async fn revoke(cl: &ApiClient, d: &DirectoryLogin) -> Result<(), String> {
    let body = json!({ "token": d.refresh_token, "client_id": "gxctl" });
    match post_bare(cl, &format!("/d/{}/revoke", client::enc(&d.id)), &body).await? {
        (200, _) => Ok(()),
        (st, _) => Err(format!("HTTP {st}")),
    }
}

/// Before a command: refresh the profile's access token when it is a
/// directory login's and about to expire, and keep the new tokens.
pub async fn refresh_if_due(cl: &ApiClient, want: Option<&str>) {
    if client::token_from_env().is_some() {
        return;
    }
    let Ok(Some(cfg)) = config::load() else {
        return;
    };
    let Ok(Some(sel)) = config::select(&cfg, want) else {
        return;
    };
    let Some(d) = sel.profile.directory.clone() else {
        return;
    };
    if client::now_secs() + EARLY_SECS < d.access_expires_at {
        return;
    }
    let name = sel.name.clone();
    match refresh(cl, &d).await {
        Ok(l) => {
            cl.set_token(Some(l.access.clone()));
            let access = l.access.to_string();
            if let Err(e) = config::edit(Some(cfg), |c| {
                if let Some(p) = c.profiles.get_mut(&name) {
                    p.token = Some(access);
                    p.directory = Some(l.keep);
                }
            }) {
                err(format!("keeping the refreshed login: {e}"));
            }
        }
        Err(e) => eprintln!("{} {}", "Warning:".yellow(), e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_answer_becomes_what_the_profile_keeps_without_printing_it() {
        let v = json!({
            "access_token": "eyJ.a.b", "refresh_token": "gxr_secret", "expires_in": 3600,
            "token_type": "Bearer",
        });
        let l = login_of("local", "urn:glidex:cluster:c1", &v).unwrap();
        assert_eq!(l.access.as_str(), "eyJ.a.b");
        assert_eq!(
            (l.keep.id.as_str(), l.keep.refresh_token.as_str()),
            ("local", "gxr_secret")
        );
        assert!(l.keep.access_expires_at >= client::now_secs() + 3500);
        let shown = format!("{:?}", l.keep);
        assert!(!shown.contains("gxr_secret"), "{shown}");
        // No access token (no resource asked): not a usable login.
        assert!(login_of("local", "r", &json!({"refresh_token": "gxr_x"})).is_none());
    }
}

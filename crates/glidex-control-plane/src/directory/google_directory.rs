//! Google Workspace's Directory API as a pull source (spec/tenancy.md
//! §8.2, D44, D47): a service account with domain-wide delegation, acting
//! as `sync.admin_email`, reads the members of the connector's groups
//! (nested ones too) and those users. Its key is the connector's sealed
//! secret `service_account_key`; access tokens are kept in memory only.

use crate::auth::AuthService;
use crate::directory::sync::{Snapshot, Source, SourceGroup, SourceUser, SyncError};
use crate::directory::{Connector, SyncSettings};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

/// The service account key's secret field (D47).
pub const KEY_FIELD: &str = "service_account_key";
pub const SCOPES: [&str; 2] = [
    "https://www.googleapis.com/auth/admin.directory.user.readonly",
    "https://www.googleapis.com/auth/admin.directory.group.readonly",
];
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://admin.googleapis.com/admin/directory/v1";
const PAGE: &str = "200";
/// Tries per request on `429` and `5xx`.
const TRIES: u32 = 4;

/// The parts of a service account's JSON key glidex uses.
#[derive(Deserialize)]
struct Key {
    client_email: String,
    private_key: String,
}

fn parse_key(json: &str) -> Result<Key, SyncError> {
    let k: Key = serde_json::from_str(json).map_err(|_| {
        SyncError::Source("service_account_key isn't a service account's JSON key".into())
    })?;
    if !k.private_key.contains("PRIVATE KEY") || !k.client_email.contains('@') {
        return Err(SyncError::Source(
            "service_account_key has no client_email or private_key".into(),
        ));
    }
    Ok(k)
}

/// Where the token and the API are: Google's, or a test mock's (the
/// connector's loopback `issuer`, which only a mock may have).
fn endpoints(c: &Connector) -> (String, String) {
    match c.google().ok().and_then(|g| g.issuer) {
        Some(mock) => (
            format!("{mock}/token"),
            format!("{mock}/admin/directory/v1"),
        ),
        None => (TOKEN_URL.into(), API.into()),
    }
}

/// Reads a Workspace directory for its `google` connectors.
pub struct GoogleDirectory {
    http: reqwest::Client,
    /// connector → (access token, expiry), never stored.
    tokens: Mutex<HashMap<String, (String, u64)>>,
}

impl Default for GoogleDirectory {
    fn default() -> Self {
        GoogleDirectory {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            tokens: Mutex::new(HashMap::new()),
        }
    }
}

fn settings(c: &Connector) -> Result<&SyncSettings, SyncError> {
    c.sync
        .as_ref()
        .ok_or_else(|| SyncError::Source("the connector has no sync settings".into()))
}

/// Google's error message, without anything secret in it.
fn google_error(status: reqwest::StatusCode, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| v.get("error_description").and_then(Value::as_str))
        .or_else(|| v.get("error").and_then(Value::as_str))
        .unwrap_or("");
    let msg: String = msg.chars().take(300).collect();
    format!(
        "Google answered {}{}{}",
        status.as_u16(),
        if msg.is_empty() { "" } else { ": " },
        msg
    )
}

impl GoogleDirectory {
    /// An access token for `c`, from the cache or a new exchange (RFC 7523).
    pub async fn token(&self, auth: &AuthService, c: &Connector) -> Result<String, SyncError> {
        let now = crate::auth::store::now();
        if let Some((t, exp)) = self.tokens.lock().unwrap().get(&c.id)
            && *exp > now + 60
        {
            return Ok(t.clone());
        }
        let key = auth
            .connectors
            .secret(c, KEY_FIELD)
            .map_err(|e| SyncError::Source(format!("reading the service account key: {e}")))?
            .ok_or_else(|| SyncError::Source(format!("set the connector's {KEY_FIELD} first")))?;
        let key = parse_key(&key)?;
        let s = settings(c)?;
        let (token_url, _) = endpoints(c);
        let claims = serde_json::json!({
            "iss": key.client_email,
            "sub": s.admin_email,
            "scope": SCOPES.join(" "),
            "aud": token_url,
            "iat": now,
            "exp": now + 3600,
        });
        let signer =
            jsonwebtoken::EncodingKey::from_rsa_pem(key.private_key.as_bytes()).map_err(|_| {
                SyncError::Source("the service account's private_key can't be read".into())
            })?;
        let assertion = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &signer,
        )
        .map_err(|e| SyncError::Source(format!("signing the token request: {e}")))?;
        let r = self
            .http
            .post(&token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|e| {
                SyncError::Source(format!(
                    "reaching Google's token endpoint: {}",
                    e.without_url()
                ))
            })?;
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SyncError::Source(format!(
                "the service account's token: {}",
                google_error(status, &body)
            )));
        }
        #[derive(Deserialize)]
        struct T {
            access_token: String,
            #[serde(default)]
            expires_in: Option<u64>,
        }
        let t: T = serde_json::from_str(&body)
            .map_err(|_| SyncError::Source("Google's token answer can't be read".into()))?;
        let exp = now + t.expires_in.unwrap_or(3600);
        self.tokens
            .lock()
            .unwrap()
            .insert(c.id.clone(), (t.access_token.clone(), exp));
        Ok(t.access_token)
    }

    /// GET `path` of the Directory API, retried on `429` and `5xx`.
    async fn get(
        &self,
        token: &str,
        base: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, SyncError> {
        let url = format!("{base}{path}");
        let mut wait = Duration::from_millis(500);
        for attempt in 1..=TRIES {
            let r = self
                .http
                .get(&url)
                .bearer_auth(token)
                .query(query)
                .send()
                .await
                .map_err(|e| {
                    SyncError::Source(format!("reaching the Directory API: {}", e.without_url()))
                })?;
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            if status.is_success() {
                return serde_json::from_str(&body).map_err(|_| {
                    SyncError::Source(format!(
                        "the Directory API's answer to {path} can't be read"
                    ))
                });
            }
            let again = status.as_u16() == 429 || status.is_server_error();
            if !again || attempt == TRIES {
                return Err(SyncError::Source(format!(
                    "{path}: {}",
                    google_error(status, &body)
                )));
            }
            tokio::time::sleep(wait).await;
            wait *= 2;
        }
        unreachable!("the last try returns")
    }

    /// Every page of a list.
    async fn list(
        &self,
        token: &str,
        base: &str,
        path: &str,
        query: &[(&str, &str)],
        field: &str,
    ) -> Result<Vec<Value>, SyncError> {
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        for _ in 0..10_000 {
            let mut q: Vec<(&str, &str)> = query.to_vec();
            q.push(("maxResults", PAGE));
            if let Some(t) = &next {
                q.push(("pageToken", t));
            }
            let v = self.get(token, base, path, &q).await?;
            if let Some(items) = v.get(field) {
                let items = items
                    .as_array()
                    .ok_or_else(|| SyncError::Source(format!("{path}: {field} isn't a list")))?;
                out.extend(items.iter().cloned());
            }
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => next = Some(t.to_string()),
                _ => return Ok(out),
            }
        }
        Err(SyncError::Source(format!("{path}: too many pages")))
    }

    /// Group `email`: its id and name (the connector test's step, and a run's).
    pub async fn group(
        &self,
        token: &str,
        base: &str,
        email: &str,
    ) -> Result<(String, String), SyncError> {
        let v = self
            .get(token, base, &format!("/groups/{}", enc(email)), &[])
            .await?;
        let id = v
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| SyncError::Source(format!("group {email} has no id")))?;
        let name = v.get("name").and_then(Value::as_str).unwrap_or(email);
        Ok((id.to_string(), name.to_string()))
    }
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn opt(v: &Value, p: &str) -> Option<String> {
    v.pointer(p)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// A Directory API user, as a pull's [`SourceUser`].
pub fn source_user(v: &Value, ssh_field: Option<&str>) -> Result<SourceUser, SyncError> {
    let id = opt(v, "/id").ok_or_else(|| SyncError::Source("a user without an id".into()))?;
    let email = opt(v, "/primaryEmail")
        .ok_or_else(|| SyncError::Source(format!("user {id} has no primaryEmail")))?
        .to_ascii_lowercase();
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let ssh_keys = match ssh_field.and_then(|f| f.split_once('.')) {
        Some((schema, field)) => match v.pointer(&format!("/customSchemas/{schema}/{field}")) {
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(|x| {
                    x.get("value")
                        .and_then(Value::as_str)
                        .or_else(|| x.as_str())
                })
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect(),
            Some(Value::String(k)) => k
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    Ok(SourceUser {
        id,
        email,
        display_name: opt(v, "/name/fullName"),
        given_name: opt(v, "/name/givenName"),
        family_name: opt(v, "/name/familyName"),
        active: !flag("suspended") && !flag("archived"),
        ssh_keys,
    })
}

#[async_trait::async_trait]
impl Source for GoogleDirectory {
    async fn read(&self, auth: &AuthService, c: &Connector) -> Result<Snapshot, SyncError> {
        let s = settings(c)?;
        let (_, base) = endpoints(c);
        let token = self.token(auth, c).await?;
        let mut groups = Vec::new();
        let mut wanted: Vec<String> = Vec::new();
        for email in &s.groups {
            let (id, name) = self.group(&token, &base, email).await?;
            let members = self
                .list(
                    &token,
                    &base,
                    &format!("/groups/{}/members", enc(email)),
                    &[("includeDerivedMembership", "true")],
                    "members",
                )
                .await?;
            let users: Vec<String> = members
                .iter()
                .filter(|m| m.get("type").and_then(Value::as_str) == Some("USER"))
                .filter_map(|m| opt(m, "/id"))
                .collect();
            for u in &users {
                if !wanted.contains(u) {
                    wanted.push(u.clone());
                }
            }
            groups.push(SourceGroup {
                id,
                email: email.clone(),
                name,
                members: users,
            });
        }
        let mut users: BTreeMap<String, SourceUser> = BTreeMap::new();
        let ssh = s.ssh_keys_field.as_deref();
        let schema = ssh
            .and_then(|f| f.split_once('.'))
            .map(|(s, _)| s.to_string());
        for id in wanted {
            let mut q: Vec<(&str, &str)> = vec![("projection", "basic")];
            if let Some(sc) = &schema {
                q = vec![("projection", "custom"), ("customFieldMask", sc.as_str())];
            }
            let v = self
                .get(&token, &base, &format!("/users/{}", enc(&id)), &q)
                .await?;
            let u = source_user(&v, ssh)?;
            users.insert(u.id.clone(), u);
        }
        Ok(Snapshot {
            users: users.into_values().collect(),
            groups,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn users_are_read_with_their_state_and_keys() {
        let v = json!({"id": "1077", "primaryEmail": "Ada@Corp.example",
            "name": {"fullName": "Ada Lovelace", "givenName": "Ada", "familyName": "Lovelace"},
            "suspended": false, "archived": true,
            "customSchemas": {"glidex": {"ssh_keys": [{"value": "ssh-ed25519 AAAA ada"}, {"value": " "}]}}});
        let u = source_user(&v, Some("glidex.ssh_keys")).unwrap();
        assert_eq!(
            (u.id.as_str(), u.email.as_str()),
            ("1077", "ada@corp.example")
        );
        assert!(!u.active, "archived");
        assert_eq!(u.ssh_keys, ["ssh-ed25519 AAAA ada"]);
        assert!(source_user(&json!({"primaryEmail": "x@y.z"}), None).is_err());
        let u = source_user(&json!({"id": "2", "primaryEmail": "b@c.d"}), None).unwrap();
        assert!(u.active && u.display_name.is_none());
    }

    #[test]
    fn errors_say_what_google_said_and_nothing_else() {
        let e = google_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error": {"code": 403, "message": "Not Authorized to access this resource/api"}}"#,
        );
        assert_eq!(
            e,
            "Google answered 403: Not Authorized to access this resource/api"
        );
        assert_eq!(
            google_error(
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"error": "unauthorized_client", "error_description": "Client is unauthorized to retrieve access tokens using this method"}"#
            ),
            "Google answered 400: Client is unauthorized to retrieve access tokens using this method"
        );
        assert!(parse_key("{}").is_err());
        assert!(
            parse_key(r#"{"client_email": "a@b.iam.gserviceaccount.com", "private_key": "nope"}"#)
                .is_err()
        );
    }
}

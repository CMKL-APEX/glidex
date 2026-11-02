//! SCIM 2.0, the protocol (spec/19-tenancy.md §8.1; RFC 7643, RFC 7644):
//! errors, filters, the user and group attributes glidex keeps, PATCH as
//! Entra, Okta and JumpCloud send it, and the resources' JSON. Pure: the
//! HTTP side and the store's one write per request are in `api/scim.rs`.

use crate::auth::store::{Managed, ScimGroup, User};
use serde_json::{Value, json};

pub const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
pub const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
pub const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
pub const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
pub const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const SPC_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";
const RESOURCE_TYPE_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ResourceType";
const SCHEMA_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Schema";
/// Longest page (§8.1).
pub const MAX_PAGE: usize = 200;
/// A page when the request names none.
pub const DEFAULT_PAGE: usize = 100;
/// Longest string attribute glidex keeps.
const MAX_STR: usize = 256;
/// Most members a group may name in one request.
pub const MAX_MEMBERS: usize = 10_000;

/// A SCIM error (RFC 7644 §3.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScimError {
    pub status: u16,
    pub scim_type: Option<&'static str>,
    pub detail: String,
}

impl ScimError {
    pub fn new(status: u16, scim_type: Option<&'static str>, detail: impl Into<String>) -> Self {
        ScimError {
            status,
            scim_type,
            detail: detail.into(),
        }
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(400, Some("invalidValue"), detail)
    }

    pub fn syntax(detail: impl Into<String>) -> Self {
        Self::new(400, Some("invalidSyntax"), detail)
    }

    pub fn filter(detail: impl Into<String>) -> Self {
        Self::new(400, Some("invalidFilter"), detail)
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(404, None, format!("no such {what}"))
    }

    pub fn uniqueness(detail: impl Into<String>) -> Self {
        Self::new(409, Some("uniqueness"), detail)
    }

    pub fn body(&self) -> Value {
        let mut v = json!({
            "schemas": [ERROR_SCHEMA],
            "status": self.status.to_string(),
            "detail": self.detail,
        });
        if let Some(t) = self.scim_type {
            v["scimType"] = json!(t);
        }
        v
    }
}

impl From<crate::auth::store::StoreError> for ScimError {
    fn from(e: crate::auth::store::StoreError) -> Self {
        tracing::error!("SCIM store error: {e}");
        ScimError::new(500, None, "storage error")
    }
}

impl From<crate::store::StoreError> for ScimError {
    fn from(e: crate::store::StoreError) -> Self {
        crate::auth::store::StoreError::from(e).into()
    }
}

// ---- filters ------------------------------------------------------------

/// `<attribute> eq <value>`, the one filter form glidex answers (§8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// Lower case, without the core schema's prefix.
    pub attr: String,
    pub value: String,
}

/// The attributes `GET /Users?filter=` and `GET /Groups?filter=` take.
pub const USER_FILTERS: [&str; 5] = [
    "username",
    "externalid",
    "emails",
    "emails.value",
    "displayname",
];
pub const GROUP_FILTERS: [&str; 2] = ["displayname", "externalid"];

fn strip_schema(path: &str) -> &str {
    for s in [USER_SCHEMA, GROUP_SCHEMA] {
        if let Some(rest) = path.strip_prefix(s)
            && let Some(rest) = rest.strip_prefix(':')
        {
            return rest;
        }
    }
    path
}

pub fn parse_filter(s: &str, allowed: &[&str]) -> Result<Filter, ScimError> {
    let s = s.trim();
    let (attr, rest) = s
        .split_once(char::is_whitespace)
        .ok_or_else(|| ScimError::filter("a filter is `<attribute> eq \"<value>\"`"))?;
    let rest = rest.trim_start();
    let (op, value) = rest
        .split_once(char::is_whitespace)
        .ok_or_else(|| ScimError::filter("a filter is `<attribute> eq \"<value>\"`"))?;
    if !op.eq_ignore_ascii_case("eq") {
        return Err(ScimError::filter(format!("only eq is supported, not {op}")));
    }
    let attr = strip_schema(attr).to_ascii_lowercase();
    if !allowed.contains(&attr.as_str()) {
        return Err(ScimError::filter(format!(
            "filtering on {attr} isn't supported: {}",
            allowed.join(", ")
        )));
    }
    let value = value.trim();
    let value = if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        // JSON string escapes, as RFC 7644 §3.4.2.2 says.
        serde_json::from_str::<String>(&format!("\"{inner}\""))
            .map_err(|_| ScimError::filter("a bad string in the filter"))?
    } else if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
        value.to_ascii_lowercase()
    } else {
        return Err(ScimError::filter("compare with a quoted string"));
    };
    Ok(Filter { attr, value })
}

/// Whether `u` matches `f` (`userName` and emails ignore case, RFC 7643
/// §4.1.1, §2.4; `externalId` doesn't).
pub fn user_matches(u: &User, f: &Filter) -> bool {
    let m = u.managed.as_ref();
    match f.attr.as_str() {
        "username" => m
            .and_then(|m| m.user_name.as_deref())
            .is_some_and(|n| n.eq_ignore_ascii_case(&f.value)),
        "externalid" => m.and_then(|m| m.external_id.as_deref()) == Some(f.value.as_str()),
        "emails" | "emails.value" => u
            .email
            .as_deref()
            .is_some_and(|e| e.eq_ignore_ascii_case(&f.value)),
        "displayname" => u.display_name == f.value,
        _ => false,
    }
}

pub fn group_matches(g: &ScimGroup, f: &Filter) -> bool {
    match f.attr.as_str() {
        "displayname" => g.display_name == f.value,
        "externalid" => g.external_id.as_deref() == Some(f.value.as_str()),
        _ => false,
    }
}

// ---- users --------------------------------------------------------------

/// What glidex keeps of a SCIM user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserAttrs {
    pub user_name: String,
    pub external_id: Option<String>,
    pub display_name: Option<String>,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub formatted: Option<String>,
    pub email: Option<String>,
    pub active: bool,
}

fn short(v: &Value, what: &str) -> Result<Option<String>, ScimError> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => {
            let s = s.trim();
            if s.chars().count() > MAX_STR || s.chars().any(char::is_control) {
                return Err(ScimError::invalid(format!(
                    "{what} is too long or has control characters"
                )));
            }
            Ok((!s.is_empty()).then(|| s.to_string()))
        }
        _ => Err(ScimError::invalid(format!("{what} is a string"))),
    }
}

/// `true`, `false`, or Entra's `"True"` / `"False"`.
fn boolean(v: &Value, what: &str) -> Result<bool, ScimError> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(ScimError::invalid(format!("{what} is true or false"))),
    }
}

/// The user's email: the primary one, else the first `work`, else the first.
fn pick_email(v: &Value) -> Result<Option<String>, ScimError> {
    let Some(list) = v.as_array() else {
        return match v {
            Value::Null => Ok(None),
            _ => Err(ScimError::invalid("emails is a list")),
        };
    };
    let value = |e: &Value| e.get("value").cloned().unwrap_or(Value::Null);
    let chosen = list
        .iter()
        .find(|e| {
            e.get("primary")
                .is_some_and(|p| boolean(p, "primary").unwrap_or(false))
        })
        .or_else(|| {
            list.iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("work"))
        })
        .or_else(|| list.first());
    match chosen {
        Some(e) => short(&value(e), "emails.value"),
        None => Ok(None),
    }
}

impl UserAttrs {
    /// From a `POST` or `PUT` body.
    pub fn from_json(v: &Value) -> Result<UserAttrs, ScimError> {
        if !v.is_object() {
            return Err(ScimError::syntax("a user is a JSON object"));
        }
        let mut a = UserAttrs {
            active: true,
            ..Default::default()
        };
        for (k, val) in v.as_object().into_iter().flatten() {
            a.set(k, val)?;
        }
        if a.user_name.is_empty() {
            return Err(ScimError::invalid("userName is required"));
        }
        Ok(a)
    }

    /// What `u` holds now.
    pub fn of(u: &User) -> UserAttrs {
        let m = u.managed.clone().unwrap_or_else(|| Managed {
            connector: String::new(),
            external_id: None,
            user_name: None,
            given_name: None,
            family_name: None,
            ssh_keys: vec![],
        });
        UserAttrs {
            user_name: m.user_name.unwrap_or_default(),
            external_id: m.external_id,
            display_name: Some(u.display_name.clone()),
            given_name: m.given_name,
            family_name: m.family_name,
            formatted: None,
            email: u.email.clone(),
            active: !scim_disabled(u),
        }
    }

    /// Set one attribute, named as in the core schema (any case).
    fn set(&mut self, key: &str, v: &Value) -> Result<(), ScimError> {
        let key = strip_schema(key);
        match key.to_ascii_lowercase().as_str() {
            "username" => self.user_name = short(v, "userName")?.unwrap_or_default(),
            "externalid" => self.external_id = short(v, "externalId")?,
            "displayname" => self.display_name = short(v, "displayName")?,
            "active" => self.active = boolean(v, "active")?,
            "emails" => self.email = pick_email(v)?,
            "name" => {
                if let Some(o) = v.as_object() {
                    for (k, x) in o {
                        self.set(&format!("name.{k}"), x)?;
                    }
                } else if !v.is_null() {
                    return Err(ScimError::invalid("name is an object"));
                }
            }
            "name.givenname" => self.given_name = short(v, "name.givenName")?,
            "name.familyname" => self.family_name = short(v, "name.familyName")?,
            "name.formatted" => self.formatted = short(v, "name.formatted")?,
            // `emails[type eq "work"].value`, `emails[primary eq true].value`.
            k if k.starts_with("emails[") && k.ends_with("].value") => {
                self.email = short(v, "emails.value")?
            }
            // Read-only, or not kept (RFC 7644 §3.5.2: ignored).
            _ => {}
        }
        Ok(())
    }

    fn unset(&mut self, key: &str) -> Result<(), ScimError> {
        match strip_schema(key).to_ascii_lowercase().as_str() {
            "username" => {
                return Err(ScimError::new(
                    400,
                    Some("mutability"),
                    "userName can't be removed",
                ));
            }
            "externalid" => self.external_id = None,
            "displayname" => self.display_name = None,
            "active" => self.active = false,
            "name" => {
                self.given_name = None;
                self.family_name = None;
                self.formatted = None;
            }
            "name.givenname" => self.given_name = None,
            "name.familyname" => self.family_name = None,
            "name.formatted" => self.formatted = None,
            k if k == "emails" || k.starts_with("emails[") => self.email = None,
            _ => {}
        }
        Ok(())
    }

    /// The name glidex shows: displayName, else name.formatted, else the
    /// given and family names, else userName.
    pub fn display(&self) -> String {
        let names = [self.given_name.as_deref(), self.family_name.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        self.display_name
            .clone()
            .or_else(|| self.formatted.clone())
            .or_else(|| (!names.is_empty()).then_some(names))
            .unwrap_or_else(|| self.user_name.clone())
    }

    /// Put them on `u`, managed by `connector` (activity aside).
    pub fn apply(&self, u: &mut User, connector: &str) {
        u.display_name = self.display();
        u.email = self.email.clone();
        // SSH keys come from a pull, not from these attributes: kept.
        let ssh_keys = u
            .managed
            .as_ref()
            .filter(|m| m.connector == connector)
            .map(|m| m.ssh_keys.clone())
            .unwrap_or_default();
        u.managed = Some(Managed {
            connector: connector.to_string(),
            external_id: self.external_id.clone(),
            user_name: Some(self.user_name.clone()),
            given_name: self.given_name.clone(),
            family_name: self.family_name.clone(),
            ssh_keys,
        });
    }
}

/// The IdP turned it off (rather than an administrator, D42).
pub fn scim_disabled(u: &User) -> bool {
    u.disabled && u.disabled_reason == Some(crate::auth::store::DisabledReason::Scim)
}

/// A PATCH body's operations (RFC 7644 §3.5.2).
fn ops(body: &Value) -> Result<Vec<(String, Option<String>, Value)>, ScimError> {
    let list = body
        .get("Operations")
        .or_else(|| body.get("operations"))
        .and_then(Value::as_array)
        .ok_or_else(|| ScimError::syntax("a PATCH has Operations"))?;
    if list.len() > 1000 {
        return Err(ScimError::invalid("too many operations"));
    }
    list.iter()
        .map(|o| {
            let op = o
                .get("op")
                .and_then(Value::as_str)
                .map(|s| s.to_ascii_lowercase())
                .ok_or_else(|| ScimError::syntax("an operation has op"))?;
            if !matches!(op.as_str(), "add" | "replace" | "remove") {
                return Err(ScimError::invalid(format!(
                    "op {op} isn't add, replace or remove"
                )));
            }
            let path = o.get("path").and_then(Value::as_str).map(String::from);
            Ok((op, path, o.get("value").cloned().unwrap_or(Value::Null)))
        })
        .collect()
}

/// Apply a PATCH to what a user holds.
pub fn patch_user(a: &mut UserAttrs, body: &Value) -> Result<(), ScimError> {
    for (op, path, value) in ops(body)? {
        match (op.as_str(), path) {
            ("remove", Some(p)) => a.unset(&p)?,
            ("remove", None) => {
                return Err(ScimError::new(400, Some("noTarget"), "remove needs a path"));
            }
            (_, Some(p)) => a.set(&p, &value)?,
            (_, None) => {
                let o = value.as_object().ok_or_else(|| {
                    ScimError::invalid("an operation without a path sets an object")
                })?;
                for (k, v) in o {
                    a.set(k, v)?;
                }
            }
        }
    }
    if a.user_name.is_empty() {
        return Err(ScimError::invalid("userName is required"));
    }
    Ok(())
}

/// A user as SCIM sees it. `base` is the endpoint's URL.
pub fn user_json(u: &User, base: &str) -> Value {
    let a = UserAttrs::of(u);
    let mut v = json!({
        "schemas": [USER_SCHEMA],
        "id": u.id,
        "userName": a.user_name,
        "displayName": u.display_name,
        "active": a.active,
        "meta": {
            "resourceType": "User",
            "created": rfc3339(u.created_at),
            "location": format!("{base}/Users/{}", u.id),
        },
    });
    if let Some(x) = &a.external_id {
        v["externalId"] = json!(x);
    }
    if a.given_name.is_some() || a.family_name.is_some() {
        let mut n = json!({});
        if let Some(g) = &a.given_name {
            n["givenName"] = json!(g);
        }
        if let Some(f) = &a.family_name {
            n["familyName"] = json!(f);
        }
        v["name"] = n;
    }
    if let Some(e) = &a.email {
        v["emails"] = json!([{ "value": e, "type": "work", "primary": true }]);
    }
    v
}

// ---- groups -------------------------------------------------------------

/// What glidex keeps of a SCIM group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupAttrs {
    pub display_name: String,
    pub external_id: Option<String>,
    pub members: Vec<String>,
}

fn member_ids(v: &Value) -> Result<Vec<String>, ScimError> {
    let list = match v {
        Value::Null => return Ok(Vec::new()),
        Value::Array(l) => l.clone(),
        Value::Object(_) => vec![v.clone()],
        _ => return Err(ScimError::invalid("members is a list of {value}")),
    };
    if list.len() > MAX_MEMBERS {
        return Err(ScimError::invalid(format!(
            "at most {MAX_MEMBERS} members at once"
        )));
    }
    list.iter()
        .map(|m| {
            m.get("value")
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| ScimError::invalid("a member is {value: <user id>}"))
        })
        .collect()
}

fn add_members(to: &mut Vec<String>, ids: Vec<String>) {
    for id in ids {
        if !to.contains(&id) {
            to.push(id);
        }
    }
}

impl GroupAttrs {
    pub fn from_json(v: &Value) -> Result<GroupAttrs, ScimError> {
        if !v.is_object() {
            return Err(ScimError::syntax("a group is a JSON object"));
        }
        let mut g = GroupAttrs {
            display_name: short(v.get("displayName").unwrap_or(&Value::Null), "displayName")?
                .unwrap_or_default(),
            external_id: short(v.get("externalId").unwrap_or(&Value::Null), "externalId")?,
            members: Vec::new(),
        };
        add_members(
            &mut g.members,
            member_ids(v.get("members").unwrap_or(&Value::Null))?,
        );
        if g.display_name.is_empty() {
            return Err(ScimError::invalid("displayName is required"));
        }
        Ok(g)
    }

    pub fn of(g: &ScimGroup) -> GroupAttrs {
        GroupAttrs {
            display_name: g.display_name.clone(),
            external_id: g.external_id.clone(),
            members: g.members.clone(),
        }
    }
}

/// `members[value eq "<id>"]`: the id.
fn member_path(p: &str) -> Option<String> {
    let inner = p.strip_prefix("members[")?.strip_suffix(']')?;
    let f = parse_filter(inner, &["value"]).ok()?;
    Some(f.value)
}

pub fn patch_group(g: &mut GroupAttrs, body: &Value) -> Result<(), ScimError> {
    for (op, path, value) in ops(body)? {
        let path = path.map(|p| strip_schema(&p).to_string());
        match (
            op.as_str(),
            path.as_deref().map(str::to_ascii_lowercase).as_deref(),
        ) {
            ("remove", Some("members")) => {
                if value.is_null() {
                    g.members.clear();
                } else {
                    let gone = member_ids(&value)?;
                    g.members.retain(|m| !gone.contains(m));
                }
            }
            ("remove", Some(p)) if p.starts_with("members[") => {
                let id = member_path(path.as_deref().unwrap_or_default()).ok_or_else(|| {
                    ScimError::new(400, Some("invalidPath"), "members[value eq \"<id>\"]")
                })?;
                g.members.retain(|m| m != &id);
            }
            ("remove", Some("externalid")) => g.external_id = None,
            ("remove", Some("displayname")) => {
                return Err(ScimError::new(
                    400,
                    Some("mutability"),
                    "displayName can't be removed",
                ));
            }
            ("remove", _) => {
                return Err(ScimError::new(400, Some("noTarget"), "remove needs a path"));
            }
            ("add", Some("members")) => add_members(&mut g.members, member_ids(&value)?),
            ("replace", Some("members")) => {
                g.members.clear();
                add_members(&mut g.members, member_ids(&value)?);
            }
            (_, Some("displayname")) => {
                g.display_name = short(&value, "displayName")?
                    .ok_or_else(|| ScimError::invalid("displayName is required"))?
            }
            (_, Some("externalid")) => g.external_id = short(&value, "externalId")?,
            (_, Some(_)) => {}
            (op, None) => {
                let o = value.as_object().ok_or_else(|| {
                    ScimError::invalid("an operation without a path sets an object")
                })?;
                for (k, v) in o {
                    match k.to_ascii_lowercase().as_str() {
                        "displayname" => {
                            g.display_name = short(v, "displayName")?
                                .ok_or_else(|| ScimError::invalid("displayName is required"))?
                        }
                        "externalid" => g.external_id = short(v, "externalId")?,
                        "members" if op == "replace" => {
                            g.members.clear();
                            add_members(&mut g.members, member_ids(v)?);
                        }
                        "members" => add_members(&mut g.members, member_ids(v)?),
                        _ => {}
                    }
                }
            }
        }
    }
    if g.members.len() > MAX_MEMBERS {
        return Err(ScimError::invalid(format!("at most {MAX_MEMBERS} members")));
    }
    Ok(())
}

/// A group as SCIM sees it; `names` gives members' display names.
pub fn group_json(g: &ScimGroup, base: &str, names: &dyn Fn(&str) -> Option<String>) -> Value {
    let mut v = json!({
        "schemas": [GROUP_SCHEMA],
        "id": g.id,
        "displayName": g.display_name,
        "members": g.members.iter().map(|m| {
            let mut x = json!({ "value": m, "$ref": format!("{base}/Users/{m}"), "type": "User" });
            if let Some(n) = names(m) {
                x["display"] = json!(n);
            }
            x
        }).collect::<Vec<_>>(),
        "meta": {
            "resourceType": "Group",
            "created": rfc3339(g.created_at),
            "lastModified": rfc3339(g.updated_at.max(g.created_at)),
            "location": format!("{base}/Groups/{}", g.id),
        },
    });
    if let Some(x) = &g.external_id {
        v["externalId"] = json!(x);
    }
    v
}

// ---- lists and discovery -------------------------------------------------

/// One page of `items` (`startIndex` is 1-based).
pub fn page(items: Vec<Value>, start: Option<usize>, count: Option<usize>) -> Value {
    let total = items.len();
    let start = start.unwrap_or(1).max(1);
    let count = count.unwrap_or(DEFAULT_PAGE).min(MAX_PAGE);
    let page: Vec<Value> = items.into_iter().skip(start - 1).take(count).collect();
    json!({
        "schemas": [LIST_SCHEMA],
        "totalResults": total,
        "startIndex": start,
        "itemsPerPage": page.len(),
        "Resources": page,
    })
}

pub fn service_provider_config(base: &str) -> Value {
    json!({
        "schemas": [SPC_SCHEMA],
        "documentationUri": "https://github.com/CMKL-APEX/glidex/blob/main/spec/19-tenancy.md",
        "patch": { "supported": true },
        "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
        "filter": { "supported": true, "maxResults": MAX_PAGE },
        "changePassword": { "supported": false },
        "sort": { "supported": false },
        "etag": { "supported": false },
        "authenticationSchemes": [{
            "type": "oauthbearertoken",
            "name": "Bearer token",
            "description": "A gxs_ token made for this connector",
            "primary": true,
        }],
        "meta": { "resourceType": "ServiceProviderConfig", "location": format!("{base}/ServiceProviderConfig") },
    })
}

pub fn resource_types(base: &str) -> Value {
    let t = |id: &str, ep: &str, schema: &str| {
        json!({
            "schemas": [RESOURCE_TYPE_SCHEMA],
            "id": id, "name": id, "endpoint": ep, "schema": schema,
            "meta": { "resourceType": "ResourceType", "location": format!("{base}/ResourceTypes/{id}") },
        })
    };
    page(
        vec![
            t("User", "/Users", USER_SCHEMA),
            t("Group", "/Groups", GROUP_SCHEMA),
        ],
        None,
        None,
    )
}

pub fn schemas(base: &str) -> Value {
    let attr = |name: &str, ty: &str, multi: bool, req: bool| {
        json!({ "name": name, "type": ty, "multiValued": multi, "required": req,
                "mutability": "readWrite", "returned": "default", "uniqueness": "none" })
    };
    let user = json!({
        "schemas": [SCHEMA_SCHEMA],
        "id": USER_SCHEMA, "name": "User",
        "attributes": [
            attr("userName", "string", false, true),
            attr("externalId", "string", false, false),
            attr("displayName", "string", false, false),
            attr("name", "complex", false, false),
            attr("emails", "complex", true, false),
            attr("active", "boolean", false, false),
        ],
        "meta": { "resourceType": "Schema", "location": format!("{base}/Schemas/{USER_SCHEMA}") },
    });
    let group = json!({
        "schemas": [SCHEMA_SCHEMA],
        "id": GROUP_SCHEMA, "name": "Group",
        "attributes": [
            attr("displayName", "string", false, true),
            attr("externalId", "string", false, false),
            attr("members", "complex", true, false),
        ],
        "meta": { "resourceType": "Schema", "location": format!("{base}/Schemas/{GROUP_SCHEMA}") },
    });
    page(vec![user, group], None, None)
}

fn rfc3339(secs: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(a: &UserAttrs) -> User {
        let mut u = User {
            id: "u1".into(),
            directory_id: crate::auth::store::local_directory(),
            display_name: String::new(),
            disabled: !a.active,
            default_project: None,
            created_at: 0,
            managed: None,
            email: None,
            disabled_reason: (!a.active).then_some(crate::auth::store::DisabledReason::Scim),
            pending_login: None,
        };
        a.apply(&mut u, "jc");
        u
    }

    #[test]
    fn filters_are_eq_on_the_named_attributes() {
        let f = parse_filter(r#"userName eq "Ada@Corp.example""#, &USER_FILTERS).unwrap();
        assert_eq!(
            f,
            Filter {
                attr: "username".into(),
                value: "Ada@Corp.example".into()
            }
        );
        let f = parse_filter(
            &format!(r#"{USER_SCHEMA}:externalId Eq "x\"y""#),
            &USER_FILTERS,
        )
        .unwrap();
        assert_eq!(f.value, "x\"y");
        for bad in [
            r#"userName co "a""#,
            r#"title eq "a""#,
            "userName eq a",
            "userName",
            r#"userName eq "a" and active eq true"#,
        ] {
            let e = parse_filter(bad, &USER_FILTERS).unwrap_err();
            assert_eq!(
                (e.status, e.scim_type),
                (400, Some("invalidFilter")),
                "{bad}"
            );
        }
        let u = user(&UserAttrs {
            user_name: "ada@corp.example".into(),
            external_id: Some("E1".into()),
            email: Some("Ada@corp.example".into()),
            active: true,
            ..Default::default()
        });
        let m = |s: &str| user_matches(&u, &parse_filter(s, &USER_FILTERS).unwrap());
        assert!(m(r#"userName eq "ADA@corp.example""#));
        assert!(m(r#"externalId eq "E1""#));
        assert!(!m(r#"externalId eq "e1""#), "externalId is case exact");
        assert!(m(r#"emails.value eq "ada@CORP.example""#));
        assert!(m(r#"emails eq "ada@corp.example""#));
    }

    #[test]
    fn a_user_from_jumpcloud_and_okta_bodies() {
        // JumpCloud: no externalId; Okta: name parts and a primary email.
        let jc = UserAttrs::from_json(&json!({
            "schemas": [USER_SCHEMA], "userName": "ada@corp.example",
            "name": {"givenName": "Ada", "familyName": "Lovelace"},
            "emails": [{"value": "ada@corp.example", "type": "work", "primary": true}],
            "active": true,
        }))
        .unwrap();
        assert_eq!(jc.display(), "Ada Lovelace");
        assert_eq!(jc.email.as_deref(), Some("ada@corp.example"));
        assert_eq!(jc.external_id, None);
        let okta = UserAttrs::from_json(&json!({
            "userName": "bob", "externalId": "00u1", "displayName": "Bob B",
            "emails": [{"value": "b2@x.example", "type": "home"}, {"value": "bob@x.example", "primary": "True"}],
        }))
        .unwrap();
        assert_eq!(
            (okta.display().as_str(), okta.email.as_deref(), okta.active),
            ("Bob B", Some("bob@x.example"), true)
        );
        assert!(
            UserAttrs::from_json(&json!({"displayName": "x"})).is_err(),
            "userName is required"
        );
        assert!(UserAttrs::from_json(&json!({"userName": "a\u{0}b"})).is_err());
        // Round trip.
        let back = user_json(&user(&jc), "https://h/scim/v2/c/jc");
        assert_eq!(back["userName"], "ada@corp.example");
        assert_eq!(back["name"]["givenName"], "Ada");
        assert_eq!(back["emails"][0]["value"], "ada@corp.example");
        assert_eq!(back["active"], true);
        assert_eq!(back["meta"]["location"], "https://h/scim/v2/c/jc/Users/u1");
        assert!(back.get("externalId").is_none());
    }

    #[test]
    fn user_patches_as_entra_okta_and_jumpcloud_send_them() {
        let mut a =
            UserAttrs::from_json(&json!({"userName": "ada", "displayName": "Ada"})).unwrap();
        // Entra: capitalized ops, "False" as a string.
        patch_user(&mut a, &json!({"schemas": [PATCH_SCHEMA], "Operations": [
            {"op": "Replace", "path": "active", "value": "False"},
            {"op": "Add", "path": "emails[type eq \"work\"].value", "value": "ada@corp.example"},
            {"op": "Replace", "path": "name.familyName", "value": "Lovelace"},
        ]}))
        .unwrap();
        assert!(!a.active);
        assert_eq!(a.email.as_deref(), Some("ada@corp.example"));
        assert_eq!(a.family_name.as_deref(), Some("Lovelace"));
        // Okta: replace without a path.
        patch_user(&mut a, &json!({"Operations": [{"op": "replace", "value": {"active": true, "displayName": "A. L."}}]})).unwrap();
        assert!(a.active);
        assert_eq!(a.display(), "A. L.");
        patch_user(
            &mut a,
            &json!({"Operations": [{"op": "remove", "path": "displayName"}]}),
        )
        .unwrap();
        assert_eq!(a.display(), "Lovelace");
        assert!(
            patch_user(
                &mut a,
                &json!({"Operations": [{"op": "remove", "path": "userName"}]})
            )
            .is_err()
        );
        assert!(
            patch_user(
                &mut a,
                &json!({"Operations": [{"op": "move", "path": "x"}]})
            )
            .is_err()
        );
        assert!(patch_user(&mut a, &json!({"ops": []})).is_err());
        // Unknown attributes are ignored.
        patch_user(
            &mut a,
            &json!({"Operations": [{"op": "add", "path": "title", "value": "Dr"}]}),
        )
        .unwrap();
    }

    #[test]
    fn group_patches_add_remove_and_replace_members() {
        let mut g =
            GroupAttrs::from_json(&json!({"displayName": "eng", "members": [{"value": "a"}]}))
                .unwrap();
        patch_group(&mut g, &json!({"Operations": [{"op": "add", "path": "members", "value": [{"value": "b"}, {"value": "a"}]}]})).unwrap();
        assert_eq!(g.members, ["a", "b"]);
        // Okta and JumpCloud: remove by filter path.
        patch_group(
            &mut g,
            &json!({"Operations": [{"op": "remove", "path": "members[value eq \"a\"]"}]}),
        )
        .unwrap();
        assert_eq!(g.members, ["b"]);
        // Entra: remove with a value list.
        patch_group(&mut g, &json!({"Operations": [{"op": "Remove", "path": "members", "value": [{"value": "b"}]}]})).unwrap();
        assert!(g.members.is_empty());
        patch_group(&mut g, &json!({"Operations": [
            {"op": "replace", "value": {"displayName": "engineering", "members": [{"value": "c"}]}},
        ]}))
        .unwrap();
        assert_eq!(
            (g.display_name.as_str(), g.members.as_slice()),
            ("engineering", ["c".to_string()].as_slice())
        );
        patch_group(
            &mut g,
            &json!({"Operations": [{"op": "remove", "path": "members"}]}),
        )
        .unwrap();
        assert!(g.members.is_empty());
        assert!(
            patch_group(
                &mut g,
                &json!({"Operations": [{"op": "remove", "path": "displayName"}]})
            )
            .is_err()
        );
        assert!(GroupAttrs::from_json(&json!({"members": []})).is_err());
    }

    #[test]
    fn pages_start_at_one_and_are_bounded() {
        let items: Vec<Value> = (0..250).map(|i| json!(i)).collect();
        let p = page(items.clone(), None, None);
        assert_eq!(
            (p["totalResults"].as_u64(), p["itemsPerPage"].as_u64()),
            (Some(250), Some(100))
        );
        let p = page(items.clone(), Some(201), Some(500));
        assert_eq!(
            (p["startIndex"].as_u64(), p["itemsPerPage"].as_u64()),
            (Some(201), Some(50))
        );
        assert_eq!(p["Resources"][0], 200);
        let p = page(items, Some(0), Some(0));
        assert_eq!(p["itemsPerPage"], 0);
        assert_eq!(ScimError::uniqueness("x").body()["scimType"], "uniqueness");
    }
}

//! The feed's rows as a cluster reads them (D67): the fields a cluster
//! that isn't glidex needs of each table, read leniently so the directory
//! may add fields. The glidex control plane stores the rows whole, as its
//! own records.

use crate::wire::Snapshot;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A principal or resource a row names: `{"type": "User", "id": "u1"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id")]
pub enum Entity {
    User(String),
    Team(String),
    Token(String),
    Tenant(String),
    Project(String),
    /// Anything else (a feed names none: the cluster refuses the row).
    #[serde(other)]
    Other,
}

/// A row of `users`: a person of the directory the feed names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub directory_id: String,
    #[serde(default)]
    pub display_name: String,
    /// Disabled or removed at the directory (D20): no logins, no tokens.
    #[serde(default)]
    pub disabled: bool,
}

/// A row of `teams`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    #[serde(default)]
    pub directory_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub members: Vec<TeamMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub user_id: String,
}

/// A row of `tenants`: one opted in to the cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    pub id: String,
    #[serde(default)]
    pub directory_id: String,
    pub slug: String,
    #[serde(default)]
    pub display_name: String,
    /// Operates clusters (D4).
    #[serde(default)]
    pub operator: bool,
    #[serde(default)]
    pub suspended: bool,
}

/// A row of `projects`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub tenant_id: String,
    #[serde(default)]
    pub description: String,
}

/// A row of `tenant_members`: a user or team in a tenant (D3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub tenant_id: String,
    pub principal: Entity,
}

/// A row of `policy_links`: a role (`template`) a principal holds on a
/// tenant or project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub id: String,
    pub template: String,
    pub principal: Entity,
    pub resource: Entity,
}

/// Whose an API token is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TokenKind {
    /// Acts as `owner`.
    Personal { owner: String },
    /// A project's.
    ServiceAccount { project: String },
}

/// A row of `api_tokens`, keyed by [`crate::tokens::api_token_hash`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub kind: TokenKind,
    pub expires_at: u64,
    /// Cluster ids, or `all`.
    #[serde(default)]
    pub clusters: Vec<String>,
}

impl ApiToken {
    /// Whether it is good on cluster `id` (§7.4).
    pub fn good_on(&self, id: &str) -> bool {
        self.clusters.iter().any(|c| c == id || c == "all")
    }
}

/// The rows of table `table` in `snap` that read as `T`, with their keys;
/// a row that doesn't read is left out.
pub fn rows<'a, T: DeserializeOwned + 'a>(
    snap: &'a Snapshot,
    table: &'a str,
) -> impl Iterator<Item = (&'a str, T)> + 'a {
    snap.rows
        .iter()
        .filter(move |r| r.table == table)
        .filter_map(|r| {
            Some((
                r.key.as_str(),
                serde_json::from_value(r.value.clone()).ok()?,
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Row;
    use serde_json::json;

    #[test]
    fn rows_read_as_glidex_writes_them() {
        let row = |table: &str, key: &str, value: serde_json::Value| Row {
            table: table.into(),
            key: key.into(),
            value,
        };
        let snap = Snapshot {
            revision: String::new(),
            jwks: json!({ "keys": [] }),
            rows: vec![
                row(
                    "users",
                    "u1",
                    json!({ "id": "u1", "directory_id": "acme", "display_name": "A", "disabled": false, "created_at": 1 }),
                ),
                row(
                    "policy_links",
                    "l1",
                    json!({ "id": "l1", "template": "role.tenant-admin", "principal": { "type": "User", "id": "u1" },
                            "resource": { "type": "Tenant", "id": "t1" }, "created_by": "x", "created_at": 1 }),
                ),
                row(
                    "tenant_members",
                    "t1/u1",
                    json!({ "tenant_id": "t1", "principal": { "type": "Team", "id": "g1" }, "by": "x", "at": 1 }),
                ),
                row(
                    "api_tokens",
                    "h1",
                    json!({ "id": "k1", "name": "ci", "kind": "service_account", "project": "p1",
                            "created_by": "u1", "created_at": 1, "expires_at": 9, "clusters": ["c1"] }),
                ),
                row("projects", "p1", json!({ "nope": 1 })),
            ],
            ended_sids: vec![],
        };
        let users: Vec<(&str, User)> = rows(&snap, "users").collect();
        assert_eq!(users[0].1.display_name, "A");
        let links: Vec<(&str, Link)> = rows(&snap, "policy_links").collect();
        assert_eq!(links[0].1.resource, Entity::Tenant("t1".into()));
        let m: Vec<(&str, Member)> = rows(&snap, "tenant_members").collect();
        assert_eq!(m[0].1.principal, Entity::Team("g1".into()));
        let t: Vec<(&str, ApiToken)> = rows(&snap, "api_tokens").collect();
        assert_eq!(
            t[0].1.kind,
            TokenKind::ServiceAccount {
                project: "p1".into()
            }
        );
        assert!(t[0].1.good_on("c1") && !t[0].1.good_on("c2"));
        // A row that doesn't read is left out.
        assert_eq!(rows::<Project>(&snap, "projects").count(), 0);
    }
}

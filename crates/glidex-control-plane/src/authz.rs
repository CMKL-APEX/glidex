//! Cedar authorization (spec/security.md §7).
//!
//! The policy set is: base policies and role templates shipped in
//! `policies/`, role links (template-linked policies, one per role
//! assignment), and enabled site policies. It is validated in strict mode
//! against the schema before it is published, and swapped atomically.
//!
//! Callers build a [`Query`] (principal, action, resource, context and the
//! entities those refer to) and get a [`Decision`]. A decision is an allow
//! only if Cedar allows **and** no policy errored: Cedar skips a policy
//! whose evaluation errors, which for a `forbid` would mean allow.

use arc_swap::ArcSwap;
use cedar_policy::{
    Authorizer, Context, Decision as CedarDecision, Entities, EntityUid, Policy, PolicyId,
    PolicySet, Request, Schema, SlotId, Template, ValidationMode, Validator,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;

pub const SCHEMA_SRC: &str = include_str!("../policies/glidex.cedarschema");
pub const BASE_SRC: &str = include_str!("../policies/base.cedar");
pub const ROLES_SRC: &str = include_str!("../policies/roles.cedar");

/// Id prefixes reserved for shipped policies and links (spec §7.6).
const RESERVED_PREFIXES: [&str; 4] = ["base.", "role.", "grant.", "link."];
const SITE_PREFIX: &str = "site.";

/// The one host entity.
pub const HOST_ID: &str = "local";

/// Team whose members (root, `glidex-admin` on api.sock) are break-glass
/// principals: allowed everything by `base.break-glass` and evaluated
/// without site policies or links.
pub const BREAK_GLASS_TEAM: &str = "unix:glidex-admin";

#[derive(Debug, Error)]
pub enum AuthzError {
    #[error("invalid policy: {0}")]
    Invalid(String),
    #[error("policy validation failed: {}", .0.join("; "))]
    Validation(Vec<String>),
    #[error("authorization request error: {0}")]
    Request(String),
}

/// A Cedar entity, by type and id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", content = "id")]
pub enum Ent {
    Host,
    User(String),
    Team(String),
    Token(String),
    Project(String),
    Vm(String),
    Disk(String),
    Credential(String),
    Image(String),
    Network(String),
    PciDevice(String),
}

impl Ent {
    pub fn type_name(&self) -> &'static str {
        match self {
            Ent::Host => "Host",
            Ent::User(_) => "User",
            Ent::Team(_) => "Team",
            Ent::Token(_) => "Token",
            Ent::Project(_) => "Project",
            Ent::Vm(_) => "Vm",
            Ent::Disk(_) => "Disk",
            Ent::Credential(_) => "Credential",
            Ent::Image(_) => "Image",
            Ent::Network(_) => "Network",
            Ent::PciDevice(_) => "PciDevice",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Ent::Host => HOST_ID,
            Ent::User(s)
            | Ent::Team(s)
            | Ent::Token(s)
            | Ent::Project(s)
            | Ent::Vm(s)
            | Ent::Disk(s)
            | Ent::Credential(s)
            | Ent::Image(s)
            | Ent::Network(s)
            | Ent::PciDevice(s) => s,
        }
    }

    pub fn from_parts(ty: &str, id: &str) -> Option<Ent> {
        let id = id.to_string();
        Some(match ty {
            "Host" => Ent::Host,
            "User" => Ent::User(id),
            "Team" => Ent::Team(id),
            "Token" => Ent::Token(id),
            "Project" => Ent::Project(id),
            "Vm" => Ent::Vm(id),
            "Disk" => Ent::Disk(id),
            "Credential" => Ent::Credential(id),
            "Image" => Ent::Image(id),
            "Network" => Ent::Network(id),
            "PciDevice" => Ent::PciDevice(id),
            _ => return None,
        })
    }

    pub fn uid(&self) -> EntityUid {
        EntityUid::from_type_name_and_id(
            format!("Glidex::{}", self.type_name()).parse().expect("static type name"),
            cedar_policy::EntityId::new(self.id()),
        )
    }

    /// `{"__entity": …}` for use inside entity attributes and contexts.
    pub fn json_ref(&self) -> Value {
        json!({ "__entity": { "type": format!("Glidex::{}", self.type_name()), "id": self.id() } })
    }

    fn json_uid(&self) -> Value {
        json!({ "type": format!("Glidex::{}", self.type_name()), "id": self.id() })
    }
}

impl std::fmt::Display for Ent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}::{:?}", self.type_name(), self.id())
    }
}

/// The entities a query refers to: the principal, its teams, the resource
/// and its ancestors, and any entity named in the context. Built per
/// request, never cached (spec §7.7).
#[derive(Default, Clone)]
pub struct EntitySet {
    entities: BTreeMap<Ent, (Value, Vec<Ent>)>,
}

impl EntitySet {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.add(Ent::Host, json!({}), vec![]);
        s
    }

    /// Add (or replace) an entity with its attributes and parents.
    pub fn add(&mut self, e: Ent, attrs: Value, parents: Vec<Ent>) -> &mut Self {
        self.entities.insert(e, (attrs, parents));
        self
    }

    pub fn contains(&self, e: &Ent) -> bool {
        self.entities.contains_key(e)
    }

    pub fn project(&mut self, id: &str) -> &mut Self {
        self.add(Ent::Project(id.into()), json!({}), vec![Ent::Host])
    }

    pub fn team(&mut self, id: &str) -> &mut Self {
        self.add(Ent::Team(id.into()), json!({}), vec![])
    }

    pub fn user(&mut self, id: &str, disabled: bool, teams: &[String]) -> &mut Self {
        for t in teams {
            self.team(t);
        }
        self.add(
            Ent::User(id.into()),
            json!({ "disabled": disabled }),
            teams.iter().map(|t| Ent::Team(t.clone())).collect(),
        )
    }

    pub fn token(&mut self, id: &str, owner: Option<&str>, expired: bool) -> &mut Self {
        let mut attrs = json!({ "expired": expired });
        if let Some(o) = owner {
            attrs["owner"] = Ent::User(o.into()).json_ref();
        }
        self.add(Ent::Token(id.into()), attrs, vec![])
    }

    /// A project-scoped entity (`Vm`, `Disk`, `Credential`) and its project.
    pub fn in_project(&mut self, e: Ent, project: &str) -> &mut Self {
        self.project(project);
        let p = Ent::Project(project.into());
        self.add(e, json!({ "project": p.json_ref() }), vec![p])
    }

    pub fn image(&mut self, id: &str) -> &mut Self {
        self.add(Ent::Image(id.into()), json!({}), vec![Ent::Host])
    }

    pub fn network(
        &mut self,
        name: &str,
        project: Option<&str>,
        all_projects: bool,
        grants: &[String],
        shares: &[String],
    ) -> &mut Self {
        for p in grants.iter().chain(shares).map(String::as_str).chain(project) {
            self.project(p);
        }
        let set = |v: &[String]| Value::Array(v.iter().map(|p| Ent::Project(p.clone()).json_ref()).collect());
        let mut attrs = json!({ "all_projects": all_projects, "grants": set(grants), "shares": set(shares) });
        let parent = match project {
            Some(p) => {
                attrs["project"] = Ent::Project(p.into()).json_ref();
                Ent::Project(p.into())
            }
            None => Ent::Host,
        };
        self.add(Ent::Network(name.into()), attrs, vec![parent])
    }

    pub fn pci_device(&mut self, bdf: &str, grants: &[String]) -> &mut Self {
        for p in grants {
            self.project(p);
        }
        let set = Value::Array(grants.iter().map(|p| Ent::Project(p.clone()).json_ref()).collect());
        self.add(Ent::PciDevice(bdf.into()), json!({ "grants": set }), vec![Ent::Host])
    }

    fn to_json(&self) -> Value {
        Value::Array(
            self.entities
                .iter()
                .map(|(e, (attrs, parents))| {
                    json!({
                        "uid": e.json_uid(),
                        "attrs": attrs,
                        "parents": parents.iter().map(Ent::json_uid).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
    }
}

/// How the principal authenticated, for `context.auth`.
#[derive(Debug, Clone, Serialize)]
pub struct AuthContext {
    pub method: String,
    pub transport: String,
    pub age_secs: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<std::net::IpAddr>,
}

impl AuthContext {
    fn to_json(&self) -> Value {
        let mut v = json!({ "method": self.method, "transport": self.transport, "age_secs": self.age_secs });
        if let Some(ip) = self.source_ip {
            v["source_ip"] = json!({ "__extn": { "fn": "ip", "arg": ip.to_string() } });
        }
        v
    }
}

/// One authorization question.
#[derive(Clone)]
pub struct Query {
    pub principal: Ent,
    pub action: String,
    pub resource: Ent,
    pub auth: AuthContext,
    /// Extra context fields (`project`, `network`) besides `auth`.
    pub extra: Vec<(&'static str, Ent)>,
    pub entities: EntitySet,
}

impl Query {
    pub fn new(principal: Ent, action: &str, resource: Ent, auth: AuthContext, entities: EntitySet) -> Self {
        Self { principal, action: action.to_string(), resource, auth, extra: Vec::new(), entities }
    }

    pub fn with(mut self, key: &'static str, e: Ent) -> Self {
        self.extra.push((key, e));
        self
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    /// Ids of the policies that determined the decision.
    pub policies: Vec<String>,
    /// Evaluation errors; any error makes the decision a deny.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl Decision {
    pub fn denied_by(&self, id: &str) -> bool {
        !self.allowed && self.policies.iter().any(|p| p == id)
    }
}

/// A role assignment: one link of a role template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub id: String,
    pub template: String,
    pub principal: Ent,
    pub resource: Ent,
}

/// Where a loaded policy came from (`GET /authz/policies`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicySource {
    Base,
    Role,
    Link,
    Site,
    File,
}

/// A site policy to load.
#[derive(Debug, Clone)]
pub struct SiteSource {
    pub id: String,
    pub text: String,
    pub source: PolicySource,
}

struct Sets {
    full: PolicySet,
    base_only: PolicySet,
    sources: HashMap<String, PolicySource>,
}

pub struct Engine {
    schema: Schema,
    sets: ArcSwap<Sets>,
}

/// Parse `src` into policies and templates re-registered under their
/// `@id` annotations (Cedar would otherwise name them `policy0`, …).
fn parse_with_ids(src: &str) -> Result<(Vec<Policy>, Vec<Template>), AuthzError> {
    let parsed = PolicySet::from_str(src).map_err(|e| AuthzError::Invalid(e.to_string()))?;
    let id_of = |ann: Option<&str>| -> Result<PolicyId, AuthzError> {
        ann.map(PolicyId::new)
            .ok_or_else(|| AuthzError::Invalid("every policy needs an @id(\"…\") annotation".into()))
    };
    let policies = parsed
        .policies()
        .map(|p| Ok(p.new_id(id_of(p.annotation("id"))?)))
        .collect::<Result<Vec<_>, AuthzError>>()?;
    let templates = parsed
        .templates()
        .map(|t| Ok(t.new_id(id_of(t.annotation("id"))?)))
        .collect::<Result<Vec<_>, AuthzError>>()?;
    Ok((policies, templates))
}

fn validation_errors(schema: &Schema, set: &PolicySet) -> Vec<String> {
    let result = Validator::new(schema.clone()).validate(set, ValidationMode::Strict);
    result.validation_errors().map(|e| e.to_string()).collect()
}

impl Engine {
    /// The shipped schema and policies, with no links or site policies.
    pub fn new() -> Result<Self, AuthzError> {
        let (schema, _) = Schema::from_cedarschema_str(SCHEMA_SRC).map_err(|e| AuthzError::Invalid(e.to_string()))?;
        let engine = Self {
            schema,
            sets: ArcSwap::from_pointee(Sets {
                full: PolicySet::new(),
                base_only: PolicySet::new(),
                sources: HashMap::new(),
            }),
        };
        engine.install(&[], &[])?;
        Ok(engine)
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    fn base_set(&self) -> Result<(PolicySet, HashMap<String, PolicySource>), AuthzError> {
        let mut set = PolicySet::new();
        let mut sources = HashMap::new();
        for (src, kind) in [(BASE_SRC, PolicySource::Base), (ROLES_SRC, PolicySource::Role)] {
            let (policies, templates) = parse_with_ids(src)?;
            for p in policies {
                sources.insert(p.id().to_string(), kind);
                set.add(p).map_err(|e| AuthzError::Invalid(e.to_string()))?;
            }
            for t in templates {
                sources.insert(t.id().to_string(), kind);
                set.add_template(t).map_err(|e| AuthzError::Invalid(e.to_string()))?;
            }
        }
        Ok((set, sources))
    }

    /// Build and validate the full set without publishing it.
    pub fn build(&self, links: &[Link], site: &[SiteSource]) -> Result<PolicySet, AuthzError> {
        self.build_inner(links, site).map(|(set, _)| set)
    }

    fn build_inner(
        &self,
        links: &[Link],
        site: &[SiteSource],
    ) -> Result<(PolicySet, HashMap<String, PolicySource>), AuthzError> {
        let (mut set, mut sources) = self.base_set()?;
        for l in links {
            let vals = HashMap::from([(SlotId::principal(), l.principal.uid()), (SlotId::resource(), l.resource.uid())]);
            set.link(PolicyId::new(&l.template), PolicyId::new(&l.id), vals)
                .map_err(|e| AuthzError::Invalid(format!("link {}: {}", l.id, e)))?;
            sources.insert(l.id.clone(), PolicySource::Link);
        }
        for s in site {
            let p = parse_site_policy(&s.id, &s.text)?;
            set.add(p).map_err(|e| AuthzError::Invalid(format!("{}: {}", s.id, e)))?;
            sources.insert(s.id.clone(), s.source);
        }
        let errors = validation_errors(&self.schema, &set);
        if !errors.is_empty() {
            return Err(AuthzError::Validation(errors));
        }
        Ok((set, sources))
    }

    /// Build, validate and atomically publish a new policy set.
    pub fn install(&self, links: &[Link], site: &[SiteSource]) -> Result<(), AuthzError> {
        let (full, sources) = self.build_inner(links, site)?;
        let (base_only, _) = self.base_set()?;
        self.sets.store(Arc::new(Sets { full, base_only, sources }));
        Ok(())
    }

    /// Decide `q` against the published set. Principals in the
    /// break-glass team are decided by the base policies alone.
    pub fn check(&self, q: &Query) -> Decision {
        let sets = self.sets.load();
        let set = if self.is_break_glass(q) { &sets.base_only } else { &sets.full };
        self.check_with(set, q)
    }

    fn is_break_glass(&self, q: &Query) -> bool {
        match q.entities.entities.get(&q.principal) {
            Some((_, parents)) => parents.contains(&Ent::Team(BREAK_GLASS_TEAM.into())),
            None => false,
        }
    }

    /// Decide `q` against `set` (for `simulate`, and tests).
    pub fn check_with(&self, set: &PolicySet, q: &Query) -> Decision {
        match self.evaluate(set, q) {
            Ok(d) => d,
            Err(e) => Decision { allowed: false, policies: Vec::new(), errors: vec![e.to_string()] },
        }
    }

    fn evaluate(&self, set: &PolicySet, q: &Query) -> Result<Decision, AuthzError> {
        let action: EntityUid = format!("Glidex::Action::{:?}", q.action)
            .parse()
            .map_err(|e: cedar_policy::ParseErrors| AuthzError::Request(e.to_string()))?;
        let mut ctx = json!({ "auth": q.auth.to_json() });
        for (k, e) in &q.extra {
            ctx[*k] = e.json_ref();
        }
        let context = Context::from_json_value(ctx, Some((&self.schema, &action)))
            .map_err(|e| AuthzError::Request(e.to_string()))?;
        let request = Request::new(q.principal.uid(), action, q.resource.uid(), context, Some(&self.schema))
            .map_err(|e| AuthzError::Request(e.to_string()))?;
        let entities = Entities::from_json_value(q.entities.to_json(), Some(&self.schema))
            .map_err(|e| AuthzError::Request(e.to_string()))?;
        let resp = Authorizer::new().is_authorized(&request, set, &entities);
        let errors: Vec<String> = resp.diagnostics().errors().map(|e| e.to_string()).collect();
        let policies = resp.diagnostics().reason().map(|id| id.to_string()).collect();
        if !errors.is_empty() {
            tracing::error!(action = %q.action, principal = %q.principal, ?errors, "policy evaluation error; denying");
        }
        Ok(Decision { allowed: resp.decision() == CedarDecision::Allow && errors.is_empty(), policies, errors })
    }

    /// Validate one candidate site policy on its own (syntax, id, schema).
    pub fn validate_site_policy(&self, id: &str, text: &str) -> Result<(), AuthzError> {
        let p = parse_site_policy(id, text)?;
        let mut set = PolicySet::new();
        set.add(p).map_err(|e| AuthzError::Invalid(e.to_string()))?;
        let errors = validation_errors(&self.schema, &set);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AuthzError::Validation(errors))
        }
    }

    /// Every loaded policy and template, with its source and text.
    pub fn listing(&self) -> Vec<PolicyInfo> {
        let sets = self.sets.load();
        let src = |id: &str| sets.sources.get(id).copied().unwrap_or(PolicySource::Site);
        let mut out: Vec<PolicyInfo> = sets
            .full
            .templates()
            .map(|t| PolicyInfo { id: t.id().to_string(), source: src(&t.id().to_string()), template: true, text: t.to_string() })
            .chain(sets.full.policies().map(|p| PolicyInfo {
                id: p.id().to_string(),
                source: src(&p.id().to_string()),
                template: false,
                text: p.to_string(),
            }))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Whether `template` is a known role template.
    pub fn is_template(&self, template: &str) -> bool {
        self.sets.load().full.template(&PolicyId::new(template)).is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyInfo {
    pub id: String,
    pub source: PolicySource,
    pub template: bool,
    pub text: String,
}

/// Parse a site policy: exactly one static policy whose `@id` is `id`,
/// with a `site.` prefix.
pub fn parse_site_policy(id: &str, text: &str) -> Result<Policy, AuthzError> {
    check_site_id(id)?;
    let (policies, templates) = parse_with_ids(text)?;
    if !templates.is_empty() || policies.len() != 1 {
        return Err(AuthzError::Invalid("a site policy must be exactly one policy, not a template".into()));
    }
    let p = policies.into_iter().next().expect("one policy");
    if p.id().to_string() != id {
        return Err(AuthzError::Invalid(format!("@id must be \"{}\" (found \"{}\")", id, p.id())));
    }
    Ok(p)
}

pub fn check_site_id(id: &str) -> Result<(), AuthzError> {
    if RESERVED_PREFIXES.iter().any(|p| id.starts_with(p)) || !id.starts_with(SITE_PREFIX) {
        return Err(AuthzError::Invalid(format!("site policy ids must start with \"{}\"", SITE_PREFIX)));
    }
    let rest = &id[SITE_PREFIX.len()..];
    if rest.is_empty()
        || rest.len() > 64
        || !rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
    {
        return Err(AuthzError::Invalid("site policy ids are site.[a-z0-9._-]{1,64}".into()));
    }
    Ok(())
}

/// Role templates and the resource kind each may be linked to.
pub const PROJECT_ROLES: [&str; 4] = ["role.viewer", "role.operator", "role.editor", "role.owner"];
pub const HOST_ROLES: [&str; 5] =
    ["role.auditor", "role.image-admin", "role.net-admin", "role.system-admin", "grant.host-paths"];

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(age: i64) -> AuthContext {
        AuthContext { method: "pam".into(), transport: "tcp".into(), age_secs: age, source_ip: None }
    }

    fn link(id: &str, t: &str, p: Ent, r: Ent) -> Link {
        Link { id: id.into(), template: t.into(), principal: p, resource: r }
    }

    fn engine_with(links: &[Link]) -> Engine {
        let e = Engine::new().unwrap();
        e.install(links, &[]).unwrap();
        e
    }

    fn base_entities() -> EntitySet {
        let mut es = EntitySet::new();
        es.project("pa").project("pb");
        es
    }

    #[test]
    fn shipped_policies_validate_strictly() {
        let e = Engine::new().unwrap();
        let listing = e.listing();
        assert!(listing.iter().any(|p| p.id == "base.step-up" && p.source == PolicySource::Base));
        assert_eq!(listing.iter().filter(|p| p.template).count(), 9);
    }

    #[test]
    fn team_link_grants_members() {
        let e = engine_with(&[link("link.1", "role.editor", Ent::Team("t".into()), Ent::Project("pa".into()))]);
        let mut es = base_entities();
        es.user("alice", false, &["t".into()]).user("bob", false, &[]);
        es.in_project(Ent::Vm("v".into()), "pa");
        let q = |who: &str| Query::new(Ent::User(who.into()), "startVm", Ent::Vm("v".into()), auth(0), es.clone());
        let d = e.check(&q("alice"));
        assert!(d.allowed, "{:?}", d);
        assert_eq!(d.policies, vec!["link.1"]);
        assert!(!e.check(&q("bob")).allowed);
    }

    #[test]
    fn host_link_covers_projects_and_roles_nest() {
        let e = engine_with(&[link("link.a", "role.auditor", Ent::User("aud".into()), Ent::Host)]);
        let mut es = base_entities();
        es.user("aud", false, &[]).in_project(Ent::Vm("v".into()), "pb");
        let q = |a: &str| Query::new(Ent::User("aud".into()), a, Ent::Vm("v".into()), auth(0), es.clone());
        assert!(e.check(&q("readVm")).allowed);
        assert!(!e.check(&q("startVm")).allowed);
        assert!(!e.check(&q("openConsole")).allowed);
    }

    #[test]
    fn step_up_window() {
        let e = engine_with(&[link("link.n", "role.net-admin", Ent::User("n".into()), Ent::Host)]);
        let mut es = base_entities();
        es.user("n", false, &[]);
        let q = |age| Query::new(Ent::User("n".into()), "installOvs", Ent::Host, auth(age), es.clone());
        assert!(e.check(&q(600)).allowed);
        let d = e.check(&q(601));
        assert!(d.denied_by("base.step-up"), "{:?}", d);
    }

    #[test]
    fn disabled_user_and_expired_token_are_denied() {
        let e = engine_with(&[
            link("link.u", "role.owner", Ent::User("u".into()), Ent::Project("pa".into())),
            link("link.t", "role.owner", Ent::Token("t".into()), Ent::Project("pa".into())),
        ]);
        let mut es = base_entities();
        es.user("u", true, &[]).token("t", None, true);
        let q = |p: Ent| Query::new(p, "readProject", Ent::Project("pa".into()), auth(0), es.clone());
        assert!(e.check(&q(Ent::User("u".into()))).denied_by("base.disabled-user"));
        assert!(e.check(&q(Ent::Token("t".into()))).denied_by("base.expired-token"));
    }

    #[test]
    fn same_project_and_network_rules() {
        let e = engine_with(&[
            link("link.1", "role.owner", Ent::User("o".into()), Ent::Project("pa".into())),
            link("link.2", "role.editor", Ent::User("o".into()), Ent::Project("pb".into())),
            link("link.3", "role.net-admin", Ent::User("n".into()), Ent::Host),
        ]);
        let mut es = base_entities();
        es.user("o", false, &[]).user("n", false, &[]);
        es.in_project(Ent::Disk("d".into()), "pa");
        es.network("pnet", Some("pa"), false, &[], &[]);
        es.network("shared", Some("pa"), false, &[], &["pb".into()]);
        es.network("hostnet", None, false, &["pa".into()], &[]);
        let q = |p: &str, a: &str, r: Ent, proj: &str| {
            Query::new(Ent::User(p.into()), a, r, auth(0), es.clone()).with("project", Ent::Project(proj.into()))
        };
        assert!(e.check(&q("o", "useDisk", Ent::Disk("d".into()), "pa")).allowed);
        assert!(e.check(&q("o", "useDisk", Ent::Disk("d".into()), "pb")).denied_by("base.same-project"));
        // An owner of pa editing pb still can't put pa's network on a pb VM.
        assert!(e.check(&q("o", "useNetwork", Ent::Network("pnet".into()), "pa")).allowed);
        assert!(e.check(&q("o", "useNetwork", Ent::Network("pnet".into()), "pb")).denied_by("base.network-grant"));
        assert!(e.check(&q("o", "useNetwork", Ent::Network("shared".into()), "pb")).allowed);
        assert!(e.check(&q("n", "useNetwork", Ent::Network("hostnet".into()), "pb")).denied_by("base.network-grant"));
        // Project networks can't be granted, only shared.
        let g = Query::new(Ent::User("n".into()), "grantNetwork", Ent::Network("pnet".into()), auth(0), es.clone());
        assert!(e.check(&g).denied_by("base.project-network-private"));
        let offer = q("o", "offerNetworkShare", Ent::Network("pnet".into()), "pb");
        assert!(e.check(&offer).allowed);
        let own = q("o", "offerNetworkShare", Ent::Network("pnet".into()), "pa");
        assert!(e.check(&own).denied_by("base.share-elsewhere"));
    }

    #[test]
    fn quota_exceed_is_system_admin_only() {
        let e = engine_with(&[
            link("link.o", "role.owner", Ent::User("o".into()), Ent::Project("pa".into())),
            link("link.s", "role.system-admin", Ent::User("s".into()), Ent::Host),
        ]);
        let mut es = base_entities();
        es.user("o", false, &[]).user("s", false, &[]);
        let q = |p: &str| Query::new(Ent::User(p.into()), "exceedQuota", Ent::Project("pa".into()), auth(0), es.clone());
        assert!(!e.check(&q("o")).allowed);
        assert!(e.check(&q("s")).allowed);
    }

    #[test]
    fn site_policies_forbid_wins_and_break_glass_ignores_them() {
        let e = Engine::new().unwrap();
        let links = [link("link.o", "role.owner", Ent::User("o".into()), Ent::Project("pa".into()))];
        let site = [SiteSource {
            id: "site.lockdown".into(),
            text: "@id(\"site.lockdown\")\nforbid (principal, action, resource);".into(),
            source: PolicySource::Site,
        }];
        e.install(&links, &site).unwrap();
        let mut es = base_entities();
        es.user("o", false, &[]).user("root", false, &[BREAK_GLASS_TEAM.into()]);
        let q = |p: &str| Query::new(Ent::User(p.into()), "readProject", Ent::Project("pa".into()), auth(0), es.clone());
        assert!(e.check(&q("o")).denied_by("site.lockdown"));
        let d = e.check(&q("root"));
        assert!(d.allowed && d.policies == vec!["base.break-glass"], "{:?}", d);
    }

    #[test]
    fn site_policy_parsing_rules() {
        let e = Engine::new().unwrap();
        assert!(e.validate_site_policy("site.a", "@id(\"site.a\")\npermit (principal, action == Glidex::Action::\"readVm\", resource);").is_ok());
        assert!(matches!(e.validate_site_policy("base.x", "@id(\"base.x\") permit(principal, action, resource);"), Err(AuthzError::Invalid(_))));
        assert!(matches!(e.validate_site_policy("site.a", "@id(\"site.b\") permit(principal, action, resource);"), Err(AuthzError::Invalid(_))));
        assert!(matches!(e.validate_site_policy("site.a", "permit(principal, action, resource);"), Err(AuthzError::Invalid(_))));
        assert!(matches!(
            e.validate_site_policy("site.a", "@id(\"site.a\") permit(principal, action, resource);\n@id(\"site.b\") permit(principal, action, resource);"),
            Err(AuthzError::Invalid(_))
        ));
        // Schema errors are caught by strict validation.
        assert!(matches!(
            e.validate_site_policy("site.a", "@id(\"site.a\") permit(principal, action == Glidex::Action::\"noSuchAction\", resource);"),
            Err(AuthzError::Validation(_))
        ));
        assert!(matches!(
            e.validate_site_policy("site.a", "@id(\"site.a\") permit(principal, action, resource) when { principal.nope };"),
            Err(AuthzError::Validation(_))
        ));
    }

    #[test]
    fn evaluation_errors_fail_closed() {
        // Validation bypassed on purpose: a forbid that errors at runtime
        // must not turn into an allow.
        let e = Engine::new().unwrap();
        let mut set = e.build(&[link("link.o", "role.owner", Ent::User("o".into()), Ent::Project("pa".into()))], &[]).unwrap();
        let bad = Policy::parse(
            Some(PolicyId::new("site.bad")),
            "forbid (principal, action, resource) when { context.auth.source_ip.isLoopback() };",
        )
        .unwrap();
        set.add(bad).unwrap();
        let mut es = base_entities();
        es.user("o", false, &[]);
        let q = Query::new(Ent::User("o".into()), "readProject", Ent::Project("pa".into()), auth(0), es);
        let d = e.check_with(&set, &q);
        assert!(!d.allowed);
        assert!(!d.errors.is_empty());
    }

    #[test]
    fn source_ip_reaches_site_policies() {
        let e = Engine::new().unwrap();
        let site = [SiteSource {
            id: "site.campus".into(),
            text: "@id(\"site.campus\")\nforbid (principal, action == Glidex::Action::\"openConsole\", resource)\nwhen { context.auth has source_ip && !context.auth.source_ip.isInRange(ip(\"10.0.0.0/8\")) };".into(),
            source: PolicySource::Site,
        }];
        e.install(&[link("link.o", "role.operator", Ent::User("o".into()), Ent::Project("pa".into()))], &site).unwrap();
        let mut es = base_entities();
        es.user("o", false, &[]).in_project(Ent::Vm("v".into()), "pa");
        let q = |ip: &str| {
            let mut a = auth(0);
            a.source_ip = Some(ip.parse().unwrap());
            Query::new(Ent::User("o".into()), "openConsole", Ent::Vm("v".into()), a, es.clone())
        };
        assert!(e.check(&q("10.1.2.3")).allowed);
        assert!(e.check(&q("192.0.2.1")).denied_by("site.campus"));
    }

    #[test]
    fn own_tokens_only() {
        let e = Engine::new().unwrap();
        let mut es = base_entities();
        es.user("a", false, &[]).user("b", false, &[]);
        let q = |r: &str| Query::new(Ent::User("a".into()), "manageOwnTokens", Ent::User(r.into()), auth(0), es.clone());
        assert!(e.check(&q("a")).allowed);
        assert!(!e.check(&q("b")).allowed);
    }
}

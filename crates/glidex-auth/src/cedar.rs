//! A Cedar policy engine (glidex spec/11-security.md §7).
//!
//! The policy set is: the application's shipped policies and role
//! templates, role links (template-linked policies, one per role
//! assignment), and enabled site policies. It is validated in strict mode
//! against the schema before it is published, and swapped atomically.
//!
//! Callers build a [`Query`] (principal, action, resource, context and the
//! entities those refer to) and get a [`Decision`]. A decision is an allow
//! only if Cedar allows **and** no policy errored: Cedar skips a policy
//! whose evaluation errors, which for a `forbid` would mean allow.
//!
//! What is the application's comes in through a [`Model`]: its schema and
//! shipped policies, its entity type, how a query's entities become
//! Cedar's, and who is break-glass.

use arc_swap::ArcSwap;
use cedar_policy::{
    Authorizer, Context, Decision as CedarDecision, Entities, EntityUid, Policy, PolicyId,
    PolicySet, Request, Schema, SlotId, Template, ValidationMode, Validator,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;

/// Id prefixes reserved for shipped policies and links (spec §7.6).
pub const RESERVED_PREFIXES: [&str; 4] = ["base.", "role.", "grant.", "link."];
pub const SITE_PREFIX: &str = "site.";

#[derive(Debug, Error)]
pub enum AuthzError {
    #[error("invalid policy: {0}")]
    Invalid(String),
    #[error("policy validation failed: {}", .0.join("; "))]
    Validation(Vec<String>),
    #[error("authorization request error: {0}")]
    Request(String),
}

/// What an application brings to the engine.
pub trait Model: 'static {
    /// An entity, by type and id.
    type Ent: Clone + PartialEq + std::fmt::Display;
    /// The entities a query refers to, built per request.
    type Entities: Clone;
    /// The Cedar namespace of the schema's actions (`<NAMESPACE>::Action`).
    const NAMESPACE: &'static str;
    /// The schema [`Engine::new`] loads, in Cedar's schema syntax.
    const SCHEMA: &'static str;
    /// The shipped policies and templates [`Engine::new`] loads, each a
    /// source text with every policy `@id`-annotated.
    const POLICIES: &'static [(&'static str, PolicySource)];

    fn uid(e: &Self::Ent) -> EntityUid;
    /// `{"__entity": …}` for use inside a context.
    fn json_ref(e: &Self::Ent) -> Value;
    /// The entities in Cedar's JSON entity format.
    fn entities_json(es: &Self::Entities) -> Value;
    /// Whether `principal` is decided by the shipped policies alone,
    /// without links or site policies.
    fn is_break_glass(es: &Self::Entities, principal: &Self::Ent) -> bool;
}

/// How the principal authenticated, for `context.auth`.
#[derive(Debug, Clone, Serialize)]
pub struct AuthContext {
    pub method: String,
    pub transport: String,
    pub age_secs: i64,
    /// A multi-factor login (glidex spec/19-tenancy.md §6), for site policies.
    pub mfa: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<std::net::IpAddr>,
}

impl AuthContext {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "method": self.method, "transport": self.transport, "age_secs": self.age_secs,
            "mfa": self.mfa,
        });
        if let Some(ip) = self.source_ip {
            v["source_ip"] = json!({ "__extn": { "fn": "ip", "arg": ip.to_string() } });
        }
        v
    }
}

/// One authorization question.
pub struct Query<M: Model> {
    pub principal: M::Ent,
    pub action: String,
    pub resource: M::Ent,
    pub auth: AuthContext,
    /// Extra context fields (`project`, `network`) besides `auth`.
    pub extra: Vec<(&'static str, M::Ent)>,
    pub entities: M::Entities,
}

impl<M: Model> Clone for Query<M> {
    fn clone(&self) -> Self {
        Self {
            principal: self.principal.clone(),
            action: self.action.clone(),
            resource: self.resource.clone(),
            auth: self.auth.clone(),
            extra: self.extra.clone(),
            entities: self.entities.clone(),
        }
    }
}

impl<M: Model> Query<M> {
    pub fn new(
        principal: M::Ent,
        action: &str,
        resource: M::Ent,
        auth: AuthContext,
        entities: M::Entities,
    ) -> Self {
        Self {
            principal,
            action: action.to_string(),
            resource,
            auth,
            extra: Vec::new(),
            entities,
        }
    }

    pub fn with(mut self, key: &'static str, e: M::Ent) -> Self {
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
pub struct Link<E> {
    pub id: String,
    pub template: String,
    pub principal: E,
    pub resource: E,
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

pub struct Engine<M: Model> {
    schema: Schema,
    /// The shipped policies and templates, parsed and validated once: they
    /// never change at run time, and validating the role templates is
    /// most of the cost of a build.
    shipped: PolicySet,
    shipped_sources: HashMap<String, PolicySource>,
    /// The links (id, template and slot values) known to validate: Cedar
    /// validates a link against its own template only, so a link is
    /// validated when it first appears, not on every build.
    valid_links: std::sync::Mutex<HashSet<(String, String, String, String)>>,
    sets: ArcSwap<Sets>,
    /// Bumped on every install: clients re-run their capability checks.
    generation: std::sync::atomic::AtomicU64,
    model: std::marker::PhantomData<fn() -> M>,
}

/// Parse `src` into policies and templates re-registered under their
/// `@id` annotations (Cedar would otherwise name them `policy0`, …).
pub fn parse_with_ids(src: &str) -> Result<(Vec<Policy>, Vec<Template>), AuthzError> {
    let parsed = PolicySet::from_str(src).map_err(|e| AuthzError::Invalid(e.to_string()))?;
    let id_of = |ann: Option<&str>| -> Result<PolicyId, AuthzError> {
        ann.map(PolicyId::new).ok_or_else(|| {
            AuthzError::Invalid("every policy needs an @id(\"…\") annotation".into())
        })
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

/// The shipped sources as one set, with each id's source.
fn parse_shipped(
    shipped: &[(&str, PolicySource)],
) -> Result<(PolicySet, HashMap<String, PolicySource>), AuthzError> {
    let mut set = PolicySet::new();
    let mut sources = HashMap::new();
    for (src, kind) in shipped {
        let (policies, templates) = parse_with_ids(src)?;
        for p in policies {
            sources.insert(p.id().to_string(), *kind);
            set.add(p).map_err(|e| AuthzError::Invalid(e.to_string()))?;
        }
        for t in templates {
            sources.insert(t.id().to_string(), *kind);
            set.add_template(t)
                .map_err(|e| AuthzError::Invalid(e.to_string()))?;
        }
    }
    Ok((set, sources))
}

fn validation_errors(schema: &Schema, set: &PolicySet) -> Vec<String> {
    let result = Validator::new(schema.clone()).validate(set, ValidationMode::Strict);
    result.validation_errors().map(|e| e.to_string()).collect()
}

impl<M: Model> Engine<M> {
    /// The model's schema and shipped policies, with no links or site
    /// policies.
    pub fn new() -> Result<Self, AuthzError> {
        let shipped: Vec<(&str, PolicySource)> = M::POLICIES.to_vec();
        Self::with_sources(M::SCHEMA, &shipped)
    }

    /// As [`new`](Self::new), with a schema and shipped policies given at
    /// run time instead of the model's.
    pub fn with_sources(
        schema: &str,
        shipped: &[(&str, PolicySource)],
    ) -> Result<Self, AuthzError> {
        let (schema, _) =
            Schema::from_cedarschema_str(schema).map_err(|e| AuthzError::Invalid(e.to_string()))?;
        let (shipped, shipped_sources) = parse_shipped(shipped)?;
        let errors = validation_errors(&schema, &shipped);
        if !errors.is_empty() {
            return Err(AuthzError::Validation(errors));
        }
        let engine = Self {
            schema,
            shipped,
            shipped_sources,
            valid_links: Default::default(),
            sets: ArcSwap::from_pointee(Sets {
                full: PolicySet::new(),
                base_only: PolicySet::new(),
                sources: HashMap::new(),
            }),
            generation: std::sync::atomic::AtomicU64::new(0),
            model: std::marker::PhantomData,
        };
        engine.install(&[], &[])?;
        Ok(engine)
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Build and validate the full set without publishing it.
    pub fn build(
        &self,
        links: &[Link<M::Ent>],
        site: &[SiteSource],
    ) -> Result<PolicySet, AuthzError> {
        self.build_inner(links, site).map(|(set, _)| set)
    }

    fn build_inner(
        &self,
        links: &[Link<M::Ent>],
        site: &[SiteSource],
    ) -> Result<(PolicySet, HashMap<String, PolicySource>), AuthzError> {
        let mut set = self.shipped.clone();
        let mut sources = self.shipped_sources.clone();
        // Only what changes at run time is validated here: the shipped part
        // was at construction, and Cedar validates each policy, template
        // and link independently of the others.
        let mut errors = Vec::new();
        let mut known = self.valid_links.lock().unwrap_or_else(|e| e.into_inner());
        let mut current = HashSet::new();
        // The links not seen before, with their templates, validated together.
        let mut new = PolicySet::new();
        for l in links {
            let (principal, resource) = (M::uid(&l.principal), M::uid(&l.resource));
            let vals = HashMap::from([
                (SlotId::principal(), principal.clone()),
                (SlotId::resource(), resource.clone()),
            ]);
            set.link(
                PolicyId::new(&l.template),
                PolicyId::new(&l.id),
                vals.clone(),
            )
            .map_err(|e| AuthzError::Invalid(format!("link {}: {}", l.id, e)))?;
            sources.insert(l.id.clone(), PolicySource::Link);
            let key = (
                l.id.clone(),
                l.template.clone(),
                principal.to_string(),
                resource.to_string(),
            );
            if !known.contains(&key) {
                let template = PolicyId::new(&l.template);
                if new.template(&template).is_none()
                    && let Some(t) = self.shipped.template(&template)
                {
                    new.add_template(t.clone())
                        .map_err(|e| AuthzError::Invalid(e.to_string()))?;
                }
                new.link(template, PolicyId::new(&l.id), vals)
                    .map_err(|e| AuthzError::Invalid(format!("link {}: {}", l.id, e)))?;
            }
            current.insert(key);
        }
        if new.policies().next().is_some() {
            errors = validation_errors(&self.schema, &new);
        }
        // Remember the current links once they validate; forget the rest.
        if errors.is_empty() {
            *known = current;
        }
        drop(known);
        let mut changed = PolicySet::new();
        for s in site {
            let p = parse_site_policy(&s.id, &s.text)?;
            set.add(p.clone())
                .map_err(|e| AuthzError::Invalid(format!("{}: {}", s.id, e)))?;
            changed
                .add(p)
                .map_err(|e| AuthzError::Invalid(format!("{}: {}", s.id, e)))?;
            sources.insert(s.id.clone(), s.source);
        }
        errors.extend(validation_errors(&self.schema, &changed));
        if !errors.is_empty() {
            return Err(AuthzError::Validation(errors));
        }
        Ok((set, sources))
    }

    /// Build, validate and atomically publish a new policy set.
    pub fn install(&self, links: &[Link<M::Ent>], site: &[SiteSource]) -> Result<(), AuthzError> {
        let (full, sources) = self.build_inner(links, site)?;
        let base_only = self.shipped.clone();
        self.sets.store(Arc::new(Sets {
            full,
            base_only,
            sources,
        }));
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(())
    }

    /// How many times policies and links were installed.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Decide `q` against the published set. Break-glass principals
    /// ([`Model::is_break_glass`]) are decided by the shipped policies
    /// alone.
    pub fn check(&self, q: &Query<M>) -> Decision {
        let sets = self.sets.load();
        let set = if M::is_break_glass(&q.entities, &q.principal) {
            &sets.base_only
        } else {
            &sets.full
        };
        self.check_with(set, q)
    }

    /// Decide `q` against `set` (for `simulate`, and tests).
    pub fn check_with(&self, set: &PolicySet, q: &Query<M>) -> Decision {
        match self.evaluate(set, q) {
            Ok(d) => d,
            Err(e) => Decision {
                allowed: false,
                policies: Vec::new(),
                errors: vec![e.to_string()],
            },
        }
    }

    /// Decide `q` against `set`; `Err` when the request itself is bad.
    pub fn evaluate(&self, set: &PolicySet, q: &Query<M>) -> Result<Decision, AuthzError> {
        let action: EntityUid = format!("{}::Action::{:?}", M::NAMESPACE, q.action)
            .parse()
            .map_err(|e: cedar_policy::ParseErrors| AuthzError::Request(e.to_string()))?;
        let mut ctx = json!({ "auth": q.auth.to_json() });
        for (k, e) in &q.extra {
            ctx[*k] = M::json_ref(e);
        }
        let context = Context::from_json_value(ctx, Some((&self.schema, &action)))
            .map_err(|e| AuthzError::Request(e.to_string()))?;
        let request = Request::new(
            M::uid(&q.principal),
            action,
            M::uid(&q.resource),
            context,
            Some(&self.schema),
        )
        .map_err(|e| AuthzError::Request(e.to_string()))?;
        let entities = Entities::from_json_value(M::entities_json(&q.entities), Some(&self.schema))
            .map_err(|e| AuthzError::Request(e.to_string()))?;
        let resp = Authorizer::new().is_authorized(&request, set, &entities);
        let errors: Vec<String> = resp.diagnostics().errors().map(|e| e.to_string()).collect();
        let policies = resp
            .diagnostics()
            .reason()
            .map(|id| id.to_string())
            .collect();
        if !errors.is_empty() {
            tracing::error!(action = %q.action, principal = %q.principal, ?errors, "policy evaluation error; denying");
        }
        Ok(Decision {
            allowed: resp.decision() == CedarDecision::Allow && errors.is_empty(),
            policies,
            errors,
        })
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
            .map(|t| PolicyInfo {
                id: t.id().to_string(),
                source: src(t.id().as_ref()),
                template: true,
                text: t.to_string(),
            })
            .chain(sets.full.policies().map(|p| PolicyInfo {
                id: p.id().to_string(),
                source: src(p.id().as_ref()),
                template: false,
                text: p.to_string(),
            }))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Whether `template` is a known role template.
    pub fn is_template(&self, template: &str) -> bool {
        self.sets
            .load()
            .full
            .template(&PolicyId::new(template))
            .is_some()
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
        return Err(AuthzError::Invalid(
            "a site policy must be exactly one policy, not a template".into(),
        ));
    }
    let p = policies.into_iter().next().expect("one policy");
    if p.id().to_string() != id {
        return Err(AuthzError::Invalid(format!(
            "@id must be \"{}\" (found \"{}\")",
            id,
            p.id()
        )));
    }
    Ok(p)
}

pub fn check_site_id(id: &str) -> Result<(), AuthzError> {
    if RESERVED_PREFIXES.iter().any(|p| id.starts_with(p)) || !id.starts_with(SITE_PREFIX) {
        return Err(AuthzError::Invalid(format!(
            "site policy ids must start with \"{}\"",
            SITE_PREFIX
        )));
    }
    let rest = &id[SITE_PREFIX.len()..];
    if rest.is_empty()
        || rest.len() > 64
        || !rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
    {
        return Err(AuthzError::Invalid(
            "site policy ids are site.[a-z0-9._-]{1,64}".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model of two entity types: users in groups, and documents.
    struct Tiny;

    #[derive(Debug, Clone, PartialEq)]
    enum E {
        User(String),
        Doc(String),
    }

    impl std::fmt::Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{self:?}")
        }
    }

    impl E {
        fn parts(&self) -> (&'static str, &str) {
            match self {
                E::User(id) => ("Tiny::User", id),
                E::Doc(id) => ("Tiny::Doc", id),
            }
        }
    }

    /// Users and whether each is an admin (the break-glass rule).
    #[derive(Clone, Default)]
    struct Ents(Vec<(String, bool)>);

    const SCHEMA: &str = r#"
        namespace Tiny {
            entity User = { "admin": Bool };
            entity Doc;
            action "read", "write" appliesTo {
                principal: [User], resource: [Doc],
                context: { "auth": { "age_secs": Long, "method": String, "mfa": Bool,
                                     "transport": String, "source_ip"?: ipaddr } }
            };
        }
    "#;
    const BASE: &str = r#"
        @id("base.admins")
        permit (principal, action, resource) when { principal.admin };
        @id("base.step-up")
        forbid (principal, action == Tiny::Action::"write", resource)
        when { context.auth.age_secs > 600 };
    "#;
    const ROLES: &str = r#"
        @id("role.reader")
        permit (principal == ?principal, action == Tiny::Action::"read", resource == ?resource);
        @id("role.writer")
        permit (principal == ?principal, action, resource == ?resource);
    "#;

    impl Model for Tiny {
        type Ent = E;
        type Entities = Ents;
        const NAMESPACE: &'static str = "Tiny";
        const SCHEMA: &'static str = SCHEMA;
        const POLICIES: &'static [(&'static str, PolicySource)] =
            &[(BASE, PolicySource::Base), (ROLES, PolicySource::Role)];

        fn uid(e: &E) -> EntityUid {
            let (ty, id) = e.parts();
            EntityUid::from_type_name_and_id(ty.parse().unwrap(), cedar_policy::EntityId::new(id))
        }
        fn json_ref(e: &E) -> Value {
            let (ty, id) = e.parts();
            json!({ "__entity": { "type": ty, "id": id } })
        }
        fn entities_json(es: &Ents) -> Value {
            Value::Array(
                es.0.iter()
                    .map(|(u, admin)| {
                        json!({ "uid": { "type": "Tiny::User", "id": u },
                                "attrs": { "admin": admin }, "parents": [] })
                    })
                    .chain(std::iter::once(
                        json!({ "uid": { "type": "Tiny::Doc", "id": "d" }, "attrs": {}, "parents": [] }),
                    ))
                    .collect(),
            )
        }
        fn is_break_glass(es: &Ents, principal: &E) -> bool {
            matches!(principal, E::User(u) if es.0.iter().any(|(n, a)| n == u && *a))
        }
    }

    fn auth(age: i64) -> AuthContext {
        AuthContext {
            method: "oidc".into(),
            transport: "tcp".into(),
            age_secs: age,
            mfa: false,
            source_ip: None,
        }
    }

    fn ents() -> Ents {
        Ents(vec![("ann".into(), false), ("root".into(), true)])
    }

    fn q(who: &str, action: &str, age: i64) -> Query<Tiny> {
        Query::new(
            E::User(who.into()),
            action,
            E::Doc("d".into()),
            auth(age),
            ents(),
        )
    }

    fn reader(who: &str) -> Link<E> {
        Link {
            id: format!("link.{who}"),
            template: "role.reader".into(),
            principal: E::User(who.into()),
            resource: E::Doc("d".into()),
        }
    }

    #[test]
    fn links_allow_and_nothing_else_does() {
        let e = Engine::<Tiny>::new().unwrap();
        assert!(!e.check(&q("ann", "read", 0)).allowed);
        assert_eq!(e.generation(), 1);
        e.install(&[reader("ann")], &[]).unwrap();
        assert_eq!(e.generation(), 2);
        let d = e.check(&q("ann", "read", 0));
        assert!(d.allowed, "{d:?}");
        assert_eq!(d.policies, vec!["link.ann"]);
        assert!(!e.check(&q("ann", "write", 0)).allowed);
        assert!(e.is_template("role.writer") && !e.is_template("base.admins"));
        let listing = e.listing();
        assert_eq!(listing.iter().filter(|p| p.template).count(), 2);
        assert!(
            listing
                .iter()
                .any(|p| p.id == "link.ann" && p.source == PolicySource::Link)
        );
    }

    #[test]
    fn forbids_win_and_break_glass_skips_site_policies() {
        let e = Engine::<Tiny>::new().unwrap();
        let lockdown = SiteSource {
            id: "site.lockdown".into(),
            text: "@id(\"site.lockdown\")\nforbid (principal, action, resource);".into(),
            source: PolicySource::Site,
        };
        e.install(&[reader("ann")], &[lockdown]).unwrap();
        assert!(e.check(&q("ann", "read", 0)).denied_by("site.lockdown"));
        let d = e.check(&q("root", "write", 0));
        assert!(d.allowed && d.policies == vec!["base.admins"], "{d:?}");
        // Shipped forbids still bind break-glass principals.
        assert!(e.check(&q("root", "write", 601)).denied_by("base.step-up"));
    }

    #[test]
    fn a_policy_that_errors_denies() {
        let e = Engine::<Tiny>::new().unwrap();
        let mut set = e.build(&[reader("ann")], &[]).unwrap();
        // Validation bypassed on purpose: `source_ip` is absent at run time.
        let bad = Policy::parse(
            Some(PolicyId::new("site.bad")),
            "forbid (principal, action, resource) when { context.auth.source_ip.isLoopback() };",
        )
        .unwrap();
        set.add(bad).unwrap();
        let d = e.check_with(&set, &q("ann", "read", 0));
        assert!(!d.allowed && !d.errors.is_empty(), "{d:?}");
        // An action outside the schema is a bad request, also a deny.
        let d = e.check(&q("ann", "delete", 0));
        assert!(!d.allowed && !d.errors.is_empty(), "{d:?}");
    }

    #[test]
    fn a_link_to_an_unknown_entity_type_is_refused() {
        // A schema without documents: a link to one names no known type.
        const USERS_ONLY: &str = r#"
            namespace Tiny {
                entity User = { "admin": Bool };
                action "read", "write" appliesTo { principal: [User], resource: [User] };
            }
        "#;
        let e = Engine::<Tiny>::with_sources(USERS_ONLY, &[(ROLES, PolicySource::Role)]).unwrap();
        assert!(matches!(
            e.install(&[reader("ann")], &[]),
            Err(AuthzError::Validation(_))
        ));
        assert_eq!(e.generation(), 1, "a failed install publishes nothing");
    }

    #[test]
    fn site_policies_are_checked_strictly() {
        let e = Engine::<Tiny>::new().unwrap();
        let ok = "@id(\"site.a\") permit (principal, action == Tiny::Action::\"read\", resource);";
        assert!(e.validate_site_policy("site.a", ok).is_ok());
        for (id, text) in [
            ("role.a", ok),
            ("site.A", ok),
            ("site.b", ok),
            ("site.a", "permit (principal, action, resource);"),
        ] {
            assert!(
                matches!(
                    e.validate_site_policy(id, text),
                    Err(AuthzError::Invalid(_))
                ),
                "{id}"
            );
        }
        assert!(matches!(
            e.validate_site_policy(
                "site.a",
                "@id(\"site.a\") permit (principal, action, resource) when { principal.nope };"
            ),
            Err(AuthzError::Validation(_))
        ));
        let bad = SiteSource {
            id: "site.a".into(),
            text: "@id(\"site.a\") permit (principal, action == Tiny::Action::\"nope\", resource);"
                .into(),
            source: PolicySource::File,
        };
        assert!(matches!(
            e.install(&[], &[bad]),
            Err(AuthzError::Validation(_))
        ));
        assert_eq!(e.generation(), 1, "a failed install publishes nothing");
    }

    #[test]
    fn sources_can_come_at_run_time() {
        let e = Engine::<Tiny>::with_sources(SCHEMA, &[(ROLES, PolicySource::Role)]).unwrap();
        // Without the shipped base policies, admins are nobody special.
        assert!(!e.check(&q("root", "read", 0)).allowed);
        assert!(
            Engine::<Tiny>::with_sources(
                SCHEMA,
                &[("permit(principal, action, resource);", PolicySource::Base)]
            )
            .is_err()
        );
    }
}

//! Cedar authorization (spec/11-security.md §7).
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
//!
//! The engine is [`glidex_auth::cedar`]'s; this module is glidex's model
//! of it: the schema and policies, the entities ([`Ent`], [`EntitySet`])
//! and the break-glass team.

use cedar_policy::EntityUid;
#[cfg(test)]
use cedar_policy::{Policy, PolicyId};
use glidex_auth::cedar::{self, Model};
pub use glidex_auth::cedar::{
    AuthContext, AuthzError, Decision, PolicyInfo, PolicySource, SiteSource, check_site_id,
    parse_site_policy,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const SCHEMA_SRC: &str = include_str!("../policies/glidex.cedarschema");
pub const BASE_SRC: &str = include_str!("../policies/base.cedar");
pub const ROLES_SRC: &str = include_str!("../policies/roles.cedar");

/// Ids of this process's host (node) and cluster entities. A standalone
/// host is a cluster of one: `Host::"local"` in `Cluster::"local"`. After
/// `gxctl cluster init` they are the node and cluster UUIDs
/// (spec/14-clustering.md §12.2).
pub const LOCAL_ID: &str = "local";

fn identity_cell() -> &'static std::sync::RwLock<(String, String)> {
    static IDS: std::sync::OnceLock<std::sync::RwLock<(String, String)>> =
        std::sync::OnceLock::new();
    IDS.get_or_init(|| std::sync::RwLock::new((LOCAL_ID.to_string(), LOCAL_ID.to_string())))
}

/// Set once at startup, from the node's identity.
pub fn set_identity(cluster: &str, node: &str) {
    *identity_cell().write().unwrap() = (cluster.to_string(), node.to_string());
}

fn account_scope_cell() -> &'static std::sync::RwLock<Option<String>> {
    static S: std::sync::OnceLock<std::sync::RwLock<Option<String>>> = std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::RwLock::new(None))
}

/// D13: in a cluster, `unix:` and `pam:` identities are scoped to the node
/// they were seen on (`alice@<node>`), unless the site manages local
/// accounts centrally (`cluster.shared_local_accounts`).
pub fn set_local_account_scope(node: Option<String>) {
    *account_scope_cell().write().unwrap() = node;
}

/// Whether local login names are per node here: in a cluster unless
/// `cluster.shared_local_accounts` (D13). The same on every server.
pub fn local_accounts_per_node() -> bool {
    account_scope_cell().read().unwrap().is_some()
}

/// The subject a local login name has in the identity store.
pub fn local_subject(name: &str) -> String {
    match &*account_scope_cell().read().unwrap() {
        Some(node) => format!("{name}@{node}"),
        None => name.to_string(),
    }
}

pub fn cluster_id() -> String {
    identity_cell().read().unwrap().0.clone()
}

pub fn node_id() -> String {
    identity_cell().read().unwrap().1.clone()
}

/// Team whose members (root, `glidex-admin` on api.sock) are break-glass
/// principals: allowed everything by `base.break-glass` and evaluated
/// without site policies or links.
pub const BREAK_GLASS_TEAM: &str = "unix:glidex-admin";

/// A Cedar entity, by type and id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", content = "id")]
pub enum Ent {
    /// The cluster: the root of everything cluster-wide (projects, images,
    /// identity). System roles link here and so cover every host.
    Cluster,
    /// This process's host. Host-specific actions (OVS, uplinks, PCI
    /// devices, host paths) take it, so a site can grant them per node.
    Host,
    /// Another node's host.
    Node(String),
    User(String),
    Team(String),
    Token(String),
    Project(String),
    Vm(String),
    Disk(String),
    Credential(String),
    Image(String),
    /// A datastore (spec/24-storage.md §13), in the cluster.
    Datastore(String),
    Network(String),
    PciDevice(String),
    PciPool(String),
    /// spec/19-tenancy.md §10.1.
    Tenant(String),
    Directory(String),
}

impl Ent {
    /// Node `node`'s PCI device `bdf`.
    pub fn pci_device(node: &str, bdf: &str) -> Ent {
        Ent::PciDevice(format!("{node}/{bdf}"))
    }

    /// The `Host` entity of node `id`.
    pub fn host_of(id: &str) -> Ent {
        if id == node_id() {
            Ent::Host
        } else {
            Ent::Node(id.to_string())
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Ent::Cluster => "Cluster",
            Ent::Host | Ent::Node(_) => "Host",
            Ent::User(_) => "User",
            Ent::Team(_) => "Team",
            Ent::Token(_) => "Token",
            Ent::Project(_) => "Project",
            Ent::Vm(_) => "Vm",
            Ent::Disk(_) => "Disk",
            Ent::Credential(_) => "Credential",
            Ent::Image(_) => "Image",
            Ent::Datastore(_) => "Datastore",
            Ent::Network(_) => "Network",
            Ent::PciDevice(_) => "PciDevice",
            Ent::PciPool(_) => "PciPool",
            Ent::Tenant(_) => "Tenant",
            Ent::Directory(_) => "Directory",
        }
    }

    pub fn id(&self) -> std::borrow::Cow<'_, str> {
        use std::borrow::Cow;
        match self {
            Ent::Cluster => Cow::Owned(cluster_id()),
            Ent::Host => Cow::Owned(node_id()),
            Ent::Node(s)
            | Ent::User(s)
            | Ent::Team(s)
            | Ent::Token(s)
            | Ent::Project(s)
            | Ent::Vm(s)
            | Ent::Disk(s)
            | Ent::Credential(s)
            | Ent::Image(s)
            | Ent::Datastore(s)
            | Ent::Network(s)
            | Ent::PciDevice(s)
            | Ent::PciPool(s)
            | Ent::Tenant(s)
            | Ent::Directory(s) => Cow::Borrowed(s),
        }
    }

    pub fn from_parts(ty: &str, id: &str) -> Option<Ent> {
        let id = id.to_string();
        Some(match ty {
            "Cluster" => Ent::Cluster,
            "Host" if id == node_id() => Ent::Host,
            "Host" => Ent::Node(id),
            "User" => Ent::User(id),
            "Team" => Ent::Team(id),
            "Token" => Ent::Token(id),
            "Project" => Ent::Project(id),
            "Vm" => Ent::Vm(id),
            "Disk" => Ent::Disk(id),
            "Credential" => Ent::Credential(id),
            "Image" => Ent::Image(id),
            "Datastore" => Ent::Datastore(id),
            "Network" => Ent::Network(id),
            "PciDevice" => Ent::PciDevice(id),
            "PciPool" => Ent::PciPool(id),
            "Tenant" => Ent::Tenant(id),
            "Directory" => Ent::Directory(id),
            _ => return None,
        })
    }

    pub fn uid(&self) -> EntityUid {
        EntityUid::from_type_name_and_id(
            format!("Glidex::{}", self.type_name())
                .parse()
                .expect("static type name"),
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
    /// The cluster and this host, with the defaults of a host that is its
    /// own directory (tenant `admin` owns it and operates the cluster).
    /// `AuthService` replaces them with the stored tenants before deciding.
    pub fn new() -> Self {
        let mut s = Self::default();
        let admin = crate::tenants::ADMIN_TENANT;
        s.directory(crate::tenants::LOCAL_DIRECTORY, admin);
        s.tenant(
            admin,
            crate::tenants::LOCAL_DIRECTORY,
            true,
            true,
            false,
            true,
        );
        s.cluster(admin, &[admin.to_string()]);
        s.add(Ent::Host, json!({}), vec![Ent::Cluster]);
        s
    }

    pub fn directory(&mut self, id: &str, owner: &str) -> &mut Self {
        self.add(
            Ent::Directory(id.into()),
            json!({ "owner": Ent::Tenant(owner.into()).json_ref() }),
            vec![],
        )
    }

    /// A tenant; `here` is whether it may allocate on this cluster (its
    /// `clusters` opt-ins, spec/19-tenancy.md D9).
    pub fn tenant(
        &mut self,
        id: &str,
        directory: &str,
        owns_directory: bool,
        operator: bool,
        suspended: bool,
        here: bool,
    ) -> &mut Self {
        let clusters = if here {
            vec![Ent::Cluster.json_ref()]
        } else {
            Vec::new()
        };
        self.add(
            Ent::Tenant(id.into()),
            json!({
                "directory": Ent::Directory(directory.into()).json_ref(),
                "owns_directory": owns_directory,
                "operator": operator,
                "suspended": suspended,
                "clusters": clusters,
            }),
            vec![Ent::Directory(directory.into())],
        )
    }

    pub fn cluster(&mut self, operator: &str, admitted: &[String]) -> &mut Self {
        let admitted: Vec<Value> = admitted
            .iter()
            .map(|t| Ent::Tenant(t.clone()).json_ref())
            .collect();
        self.add(
            Ent::Cluster,
            json!({ "operator": Ent::Tenant(operator.into()).json_ref(), "admitted": admitted }),
            vec![],
        )
    }

    /// Projects in the set.
    pub fn projects(&self) -> Vec<String> {
        self.entities
            .keys()
            .filter_map(|e| {
                if let Ent::Project(id) = e {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Users in the set.
    pub fn users(&self) -> Vec<String> {
        self.entities
            .keys()
            .filter_map(|e| {
                if let Ent::User(id) = e {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Tenants in the set.
    pub fn tenants(&self) -> Vec<String> {
        self.entities
            .keys()
            .filter_map(|e| {
                if let Ent::Tenant(id) = e {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Give a project its tenant (attribute and parent).
    pub fn set_project_tenant(&mut self, project: &str, tenant: &str) -> &mut Self {
        let t = Ent::Tenant(tenant.into());
        self.add(
            Ent::Project(project.into()),
            json!({ "tenant": t.json_ref() }),
            vec![Ent::Cluster, t],
        )
    }

    /// Make `e` (a user, team or token) a member of `tenants`.
    pub fn add_parents(&mut self, e: &Ent, tenants: &[String]) -> &mut Self {
        if let Some((_, parents)) = self.entities.get_mut(e) {
            for t in tenants {
                let t = Ent::Tenant(t.clone());
                if !parents.contains(&t) {
                    parents.push(t);
                }
            }
        }
        self
    }

    /// The first `Project` among `e` and its ancestors.
    pub fn project_of(&self, e: &Ent) -> Option<String> {
        let mut todo = vec![e.clone()];
        let mut seen = Vec::new();
        while let Some(x) = todo.pop() {
            if let Ent::Project(id) = &x {
                return Some(id.clone());
            }
            if seen.contains(&x) {
                continue;
            }
            if let Some((_, parents)) = self.entities.get(&x) {
                todo.extend(parents.iter().cloned());
            }
            seen.push(x);
        }
        None
    }

    /// Add (or replace) an entity with its attributes and parents.
    pub fn add(&mut self, e: Ent, attrs: Value, parents: Vec<Ent>) -> &mut Self {
        self.entities.insert(e, (attrs, parents));
        self
    }

    pub fn contains(&self, e: &Ent) -> bool {
        self.entities.contains_key(e)
    }

    /// The `Host` entity of node `id`, in the cluster.
    pub fn host(&mut self, id: &str) -> &mut Self {
        self.add(Ent::host_of(id), json!({}), vec![Ent::Cluster])
    }

    /// A project, in the `default` tenant until `AuthService` gives it its own.
    pub fn project(&mut self, id: &str) -> &mut Self {
        if self.contains(&Ent::Project(id.into())) {
            return self;
        }
        let d = crate::tenants::DEFAULT_TENANT;
        if !self.contains(&Ent::Tenant(d.into())) {
            self.tenant(
                d,
                crate::tenants::LOCAL_DIRECTORY,
                false,
                false,
                false,
                true,
            );
        }
        self.set_project_tenant(id, d)
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
        self.add(Ent::Image(id.into()), json!({}), vec![Ent::Cluster])
    }

    pub fn datastore(&mut self, id: &str) -> &mut Self {
        self.add(Ent::Datastore(id.into()), json!({}), vec![Ent::Cluster])
    }

    pub fn network(
        &mut self,
        name: &str,
        project: Option<&str>,
        all_projects: bool,
        grants: &[String],
        shares: &[String],
        tenant_grants: &[String],
    ) -> &mut Self {
        for p in grants
            .iter()
            .chain(shares)
            .map(String::as_str)
            .chain(project)
        {
            self.project(p);
        }
        let set = |v: &[String]| {
            Value::Array(
                v.iter()
                    .map(|p| Ent::Project(p.clone()).json_ref())
                    .collect(),
            )
        };
        let mut attrs = json!({
            "all_projects": all_projects, "grants": set(grants), "shares": set(shares),
            "tenant_grants": tenant_set(tenant_grants),
        });
        let parent = match project {
            Some(p) => {
                attrs["project"] = Ent::Project(p.into()).json_ref();
                Ent::Project(p.into())
            }
            None => Ent::Cluster,
        };
        self.add(Ent::Network(name.into()), attrs, vec![parent])
    }

    /// Pool `name`, granted to `grants` and `tenant_grants`.
    pub fn pci_pool(
        &mut self,
        name: &str,
        grants: &[String],
        tenant_grants: &[String],
    ) -> &mut Self {
        for p in grants {
            self.project(p);
        }
        let set = Value::Array(
            grants
                .iter()
                .map(|p| Ent::Project(p.clone()).json_ref())
                .collect(),
        );
        self.add(
            Ent::PciPool(name.into()),
            json!({ "grants": set, "tenant_grants": tenant_set(tenant_grants) }),
            vec![Ent::Cluster],
        )
    }

    /// PCI device `bdf` of node `node`: `PciDevice::"<node>/<bdf>"` in that
    /// node's `Host`, so a role on the host covers its devices
    /// (spec/22-host-topology.md §8.5).
    pub fn pci_device(
        &mut self,
        node: &str,
        bdf: &str,
        grants: &[String],
        tenant_grants: &[String],
    ) -> &mut Self {
        self.host(node);
        for p in grants {
            self.project(p);
        }
        let set = Value::Array(
            grants
                .iter()
                .map(|p| Ent::Project(p.clone()).json_ref())
                .collect(),
        );
        self.add(
            Ent::pci_device(node, bdf),
            json!({ "grants": set, "tenant_grants": tenant_set(tenant_grants) }),
            vec![Ent::host_of(node)],
        )
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

/// glidex's side of the engine: its schema and shipped policies, [`Ent`]
/// and [`EntitySet`], and the break-glass team.
pub struct Glidex;

impl Model for Glidex {
    type Ent = Ent;
    type Entities = EntitySet;
    const NAMESPACE: &'static str = "Glidex";
    const SCHEMA: &'static str = SCHEMA_SRC;
    const POLICIES: &'static [(&'static str, PolicySource)] = &[
        (BASE_SRC, PolicySource::Base),
        (ROLES_SRC, PolicySource::Role),
    ];

    fn uid(e: &Ent) -> EntityUid {
        e.uid()
    }

    fn json_ref(e: &Ent) -> Value {
        e.json_ref()
    }

    fn entities_json(es: &EntitySet) -> Value {
        es.to_json()
    }

    /// Principals in the break-glass team are decided by the base
    /// policies alone.
    fn is_break_glass(es: &EntitySet, principal: &Ent) -> bool {
        match es.entities.get(principal) {
            Some((_, parents)) => parents.contains(&Ent::Team(BREAK_GLASS_TEAM.into())),
            None => false,
        }
    }
}

pub type Engine = cedar::Engine<Glidex>;
/// One authorization question.
pub type Query = cedar::Query<Glidex>;
/// A role assignment: one link of a role template.
pub type Link = cedar::Link<Ent>;

/// Role templates and the resource kind each may be linked to.
pub const PROJECT_ROLES: [&str; 4] = ["role.viewer", "role.operator", "role.editor", "role.owner"];
/// Roles linked on a `Tenant` and on a `Directory` (spec/19-tenancy.md §10.5).
pub const TENANT_ROLES: [&str; 2] = ["role.tenant-admin", "role.tenant-auditor"];
pub const DIRECTORY_ROLES: [&str; 2] = ["role.directory-admin", "role.directory-auditor"];
/// Marker groups; every concrete action is in exactly one.
pub const MARKERS: [&str; 4] = ["tenant-scoped", "host-scoped", "directory-scoped", "shared"];
pub const HOST_ROLES: [&str; 5] = [
    "role.auditor",
    "role.image-admin",
    "role.net-admin",
    "role.system-admin",
    "grant.host-paths",
];
/// Host roles that may also be linked on one node's `Host` (spec/14-clustering.md
/// §12.2). The others are about cluster-wide things (images, the cluster
/// itself), which a host link wouldn't cover.
pub const PER_HOST_ROLES: [&str; 3] = ["role.auditor", "role.net-admin", "grant.host-paths"];

/// A set of tenant references (grants by tenant, spec/19-tenancy.md D25).
fn tenant_set(ids: &[String]) -> Value {
    Value::Array(
        ids.iter()
            .map(|t| Ent::Tenant(t.clone()).json_ref())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(age: i64) -> AuthContext {
        AuthContext {
            method: "pam".into(),
            transport: "tcp".into(),
            age_secs: age,
            mfa: false,
            source_ip: None,
        }
    }

    /// A query as `AuthService` would make it on a host that is its own
    /// directory: every user a member of `default` and of the operator
    /// `admin`, with the cluster, directory and resource tenant in context.
    fn tq(p: Ent, a: &str, r: Ent, auth: AuthContext, mut es: EntitySet) -> Query {
        for u in es.users() {
            es.add_parents(&Ent::User(u), &["default".into(), "admin".into()]);
        }
        let tenant = match &r {
            Ent::Tenant(t) => Some(t.clone()),
            _ => es.project_of(&r).map(|_| "default".to_string()),
        };
        if !es.contains(&Ent::Tenant("default".into())) {
            es.tenant("default", "local", false, false, false, true);
        }
        let mut q = Query::new(p, a, r, auth, es)
            .with("cluster", Ent::Cluster)
            .with("directory", Ent::Directory("local".into()));
        if let Some(t) = tenant {
            q = q.with("tenant", Ent::Tenant(t));
        }
        q
    }

    fn link(id: &str, t: &str, p: Ent, r: Ent) -> Link {
        Link {
            id: id.into(),
            template: t.into(),
            principal: p,
            resource: r,
        }
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

    /// spec/19-tenancy.md §10.1: every concrete action is in exactly one
    /// marker group, so base.cedar bounds every request by one of the
    /// tenant, operator or directory rules (or none, for `shared`).
    #[test]
    fn every_action_has_exactly_one_marker() {
        let e = Engine::new().unwrap();
        let ents = e.schema().action_entities().unwrap();
        let mut checked = 0;
        for a in e.schema().actions() {
            let name = a.id().unescaped().to_string();
            // Groups and roles are quoted names with dots or dashes.
            if name.contains('.') || name.contains('-') || MARKERS.contains(&name.as_str()) {
                continue;
            }
            let anc: Vec<String> = ents
                .ancestors(a)
                .map(|i| i.map(|u| u.id().unescaped().to_string()).collect())
                .unwrap_or_default();
            let markers: Vec<&String> = anc
                .iter()
                .filter(|g| MARKERS.contains(&g.as_str()))
                .collect();
            assert_eq!(markers.len(), 1, "{name}: markers {markers:?}");
            checked += 1;
        }
        assert!(checked > 130, "{checked}");
    }

    /// The tenancy boundaries themselves, on the engine (spec/19-tenancy.md §17).
    #[test]
    fn tenancy_boundaries() {
        let e = engine_with(&[
            link(
                "link.o",
                "role.owner",
                Ent::User("bob".into()),
                Ent::Project("pb".into()),
            ),
            link(
                "link.n",
                "role.net-admin",
                Ent::User("bob".into()),
                Ent::Cluster,
            ),
            link(
                "link.d",
                "role.directory-admin",
                Ent::User("bob".into()),
                Ent::Directory("local".into()),
            ),
        ]);
        // pb in tenant lab; bob a member of lab only.
        let mut es = EntitySet::new();
        es.tenant("lab", "local", false, false, false, true)
            .tenant("acme", "local", false, false, false, false);
        es.set_project_tenant("pb", "lab")
            .set_project_tenant("pa", "acme");
        es.in_project(Ent::Vm("vb".into()), "pb")
            .in_project(Ent::Vm("va".into()), "pa");
        es.user("bob", false, &[]);
        es.add_parents(&Ent::User("bob".into()), &["lab".into()]);
        let ctx = |q: Query, t: &str| {
            q.with("cluster", Ent::Cluster)
                .with("directory", Ent::Directory("local".into()))
                .with("tenant", Ent::Tenant(t.into()))
        };
        let bob = |a: &str, r: Ent| Query::new(Ent::User("bob".into()), a, r, auth(0), es.clone());
        assert!(
            e.check(&ctx(bob("startVm", Ent::Vm("vb".into())), "lab"))
                .allowed
        );
        // A role on the wrong tenant's project still hands out nothing.
        let e2 = engine_with(&[link(
            "link.x",
            "role.owner",
            Ent::User("bob".into()),
            Ent::Project("pa".into()),
        )]);
        let d = e2.check(&ctx(bob("startVm", Ent::Vm("va".into())), "acme"));
        assert!(d.denied_by("base.tenant-boundary"), "{d:?}");
        // Host roles need the operator (admin); directory roles the owner (admin).
        let d = e.check(
            &bob("installOvs", Ent::Host)
                .with("cluster", Ent::Cluster)
                .with("directory", Ent::Directory("local".into())),
        );
        assert!(d.denied_by("base.host-operator"), "{d:?}");
        let d = e.check(
            &bob("manageUsers", Ent::Directory("local".into()))
                .with("cluster", Ent::Cluster)
                .with("directory", Ent::Directory("local".into())),
        );
        assert!(d.denied_by("base.directory-owner"), "{d:?}");
        // Admission: lab is admitted here (in `admitted` and opted in), acme isn't.
        es.cluster("admin", &["admin".into(), "lab".into()]);
        let place = |p: &str| {
            Query::new(
                Ent::User("bob".into()),
                "useCluster",
                Ent::Cluster,
                auth(0),
                es.clone(),
            )
            .with("cluster", Ent::Cluster)
            .with("directory", Ent::Directory("local".into()))
            .with("project", Ent::Project(p.into()))
        };
        assert!(e.check(&place("pb")).allowed);
        assert!(e.check(&place("pa")).denied_by("base.admission"));
        // A suspended tenant's members can't act on its resources.
        es.tenant("lab", "local", false, false, true, true);
        let d = e.check(&ctx(
            Query::new(
                Ent::User("bob".into()),
                "startVm",
                Ent::Vm("vb".into()),
                auth(0),
                es.clone(),
            ),
            "lab",
        ));
        assert!(d.denied_by("base.tenant-suspended"), "{d:?}");
    }

    #[test]
    fn shipped_policies_validate_strictly() {
        let e = Engine::new().unwrap();
        let listing = e.listing();
        assert!(
            listing
                .iter()
                .any(|p| p.id == "base.step-up" && p.source == PolicySource::Base)
        );
        assert_eq!(listing.iter().filter(|p| p.template).count(), 13);
    }

    #[test]
    fn team_link_grants_members() {
        let e = engine_with(&[link(
            "link.1",
            "role.editor",
            Ent::Team("t".into()),
            Ent::Project("pa".into()),
        )]);
        let mut es = base_entities();
        es.user("alice", false, &["t".into()])
            .user("bob", false, &[]);
        es.in_project(Ent::Vm("v".into()), "pa");
        let q = |who: &str| {
            tq(
                Ent::User(who.into()),
                "startVm",
                Ent::Vm("v".into()),
                auth(0),
                es.clone(),
            )
        };
        let d = e.check(&q("alice"));
        assert!(d.allowed, "{:?}", d);
        assert_eq!(d.policies, vec!["link.1"]);
        assert!(!e.check(&q("bob")).allowed);
    }

    #[test]
    fn host_link_covers_projects_and_roles_nest() {
        let e = engine_with(&[link(
            "link.a",
            "role.auditor",
            Ent::User("aud".into()),
            Ent::Cluster,
        )]);
        let mut es = base_entities();
        es.user("aud", false, &[])
            .in_project(Ent::Vm("v".into()), "pb");
        let q = |a: &str| {
            tq(
                Ent::User("aud".into()),
                a,
                Ent::Vm("v".into()),
                auth(0),
                es.clone(),
            )
        };
        assert!(e.check(&q("readVm")).allowed);
        assert!(!e.check(&q("startVm")).allowed);
        assert!(!e.check(&q("openConsole")).allowed);
    }

    /// C1 acceptance: host-specific grants cover one node, cluster grants
    /// cover every node and the cluster-wide actions.
    #[test]
    fn host_and_cluster_scopes() {
        let e = engine_with(&[
            link(
                "link.h",
                "role.net-admin",
                Ent::User("hostadm".into()),
                Ent::Node("n2".into()),
            ),
            link(
                "link.c",
                "role.net-admin",
                Ent::User("clusteradm".into()),
                Ent::Cluster,
            ),
            link(
                "link.a",
                "role.auditor",
                Ent::User("aud".into()),
                Ent::Node("n2".into()),
            ),
        ]);
        let mut es = base_entities();
        es.user("hostadm", false, &[])
            .user("clusteradm", false, &[])
            .user("aud", false, &[]);
        es.host(LOCAL_ID).host("n2");
        let q = |who: &str, action: &str, res: Ent| {
            tq(Ent::User(who.into()), action, res, auth(0), es.clone())
        };
        // A grant on n2's host covers n2 only.
        assert!(
            e.check(&q("hostadm", "installOvs", Ent::Node("n2".into())))
                .allowed
        );
        assert!(
            !e.check(&q("hostadm", "installOvs", Ent::Host)).allowed,
            "the local host is another node"
        );
        // A cluster grant covers every host.
        assert!(e.check(&q("clusteradm", "installOvs", Ent::Host)).allowed);
        assert!(
            e.check(&q("clusteradm", "installOvs", Ent::Node("n2".into())))
                .allowed
        );
        // A host's devices are the host's (spec/22-host-topology.md §8.5): the
        // same address on another node is another device.
        let mut dev_es = es.clone();
        dev_es
            .project("p")
            .pci_device("n2", "0000:41:00.0", &[], &[])
            .pci_device(LOCAL_ID, "0000:41:00.0", &[], &[]);
        let use_dev = |who: &str, node: &str| {
            tq(
                Ent::User(who.into()),
                "usePciDevice",
                Ent::pci_device(node, "0000:41:00.0"),
                auth(0),
                dev_es.clone(),
            )
            .with("project", Ent::Project("p".into()))
        };
        assert!(e.check(&use_dev("hostadm", "n2")).allowed);
        assert!(!e.check(&use_dev("hostadm", LOCAL_ID)).allowed);
        assert!(e.check(&use_dev("clusteradm", LOCAL_ID)).allowed);
        // Cluster-wide actions need a cluster grant: a host grant isn't one.
        assert!(!e.check(&q("aud", "listNodes", Ent::Cluster)).allowed);
        assert!(
            e.check(&q("aud", "readNode", Ent::Node("n2".into())))
                .allowed
        );
        assert!(!e.check(&q("aud", "readNode", Ent::Host)).allowed);
        // Host roles say nothing about projects.
        let mut es2 = es.clone();
        es2.in_project(Ent::Vm("v".into()), "pa");
        assert!(
            !e.check(&tq(
                Ent::User("aud".into()),
                "readVm",
                Ent::Vm("v".into()),
                auth(0),
                es2
            ))
            .allowed
        );
    }

    #[test]
    fn step_up_window() {
        let e = engine_with(&[link(
            "link.n",
            "role.net-admin",
            Ent::User("n".into()),
            Ent::Host,
        )]);
        let mut es = base_entities();
        es.user("n", false, &[]);
        let q = |age| {
            tq(
                Ent::User("n".into()),
                "installOvs",
                Ent::Host,
                auth(age),
                es.clone(),
            )
        };
        assert!(e.check(&q(600)).allowed);
        let d = e.check(&q(601));
        assert!(d.denied_by("base.step-up"), "{:?}", d);
    }

    #[test]
    fn disabled_user_and_expired_token_are_denied() {
        let e = engine_with(&[
            link(
                "link.u",
                "role.owner",
                Ent::User("u".into()),
                Ent::Project("pa".into()),
            ),
            link(
                "link.t",
                "role.owner",
                Ent::Token("t".into()),
                Ent::Project("pa".into()),
            ),
        ]);
        let mut es = base_entities();
        es.user("u", true, &[]).token("t", None, true);
        let q = |p: Ent| {
            tq(
                p,
                "readProject",
                Ent::Project("pa".into()),
                auth(0),
                es.clone(),
            )
        };
        assert!(
            e.check(&q(Ent::User("u".into())))
                .denied_by("base.disabled-user")
        );
        assert!(
            e.check(&q(Ent::Token("t".into())))
                .denied_by("base.expired-token")
        );
    }

    #[test]
    fn same_project_and_network_rules() {
        let e = engine_with(&[
            link(
                "link.1",
                "role.owner",
                Ent::User("o".into()),
                Ent::Project("pa".into()),
            ),
            link(
                "link.2",
                "role.editor",
                Ent::User("o".into()),
                Ent::Project("pb".into()),
            ),
            link(
                "link.3",
                "role.net-admin",
                Ent::User("n".into()),
                Ent::Cluster,
            ),
        ]);
        let mut es = base_entities();
        es.user("o", false, &[]).user("n", false, &[]);
        es.in_project(Ent::Disk("d".into()), "pa");
        es.network("pnet", Some("pa"), false, &[], &[], &[]);
        es.network("shared", Some("pa"), false, &[], &["pb".into()], &[]);
        es.network("hostnet", None, false, &["pa".into()], &[], &[]);
        let q = |p: &str, a: &str, r: Ent, proj: &str| {
            tq(Ent::User(p.into()), a, r, auth(0), es.clone())
                .with("project", Ent::Project(proj.into()))
        };
        assert!(
            e.check(&q("o", "useDisk", Ent::Disk("d".into()), "pa"))
                .allowed
        );
        assert!(
            e.check(&q("o", "useDisk", Ent::Disk("d".into()), "pb"))
                .denied_by("base.same-project")
        );
        // An owner of pa editing pb still can't put pa's network on a pb VM.
        assert!(
            e.check(&q("o", "useNetwork", Ent::Network("pnet".into()), "pa"))
                .allowed
        );
        assert!(
            e.check(&q("o", "useNetwork", Ent::Network("pnet".into()), "pb"))
                .denied_by("base.network-grant")
        );
        assert!(
            e.check(&q("o", "useNetwork", Ent::Network("shared".into()), "pb"))
                .allowed
        );
        assert!(
            e.check(&q("n", "useNetwork", Ent::Network("hostnet".into()), "pb"))
                .denied_by("base.network-grant")
        );
        // Project networks can't be granted, only shared.
        let g = tq(
            Ent::User("n".into()),
            "grantNetwork",
            Ent::Network("pnet".into()),
            auth(0),
            es.clone(),
        );
        assert!(e.check(&g).denied_by("base.project-network-private"));
        let offer = q("o", "offerNetworkShare", Ent::Network("pnet".into()), "pb");
        assert!(e.check(&offer).allowed);
        let own = q("o", "offerNetworkShare", Ent::Network("pnet".into()), "pa");
        assert!(e.check(&own).denied_by("base.share-elsewhere"));
    }

    /// D25: host networks and devices granted to a tenant reach each of its
    /// projects; project networks are shared within their tenant only.
    #[test]
    fn tenant_grants_and_shares_within_a_tenant() {
        let e = engine_with(&[
            link(
                "link.1",
                "role.owner",
                Ent::User("o".into()),
                Ent::Project("pa".into()),
            ),
            link(
                "link.2",
                "role.owner",
                Ent::User("o".into()),
                Ent::Project("pc".into()),
            ),
        ]);
        let mut es = base_entities();
        es.project("pc");
        es.set_project_tenant("pa", "default");
        es.set_project_tenant("pb", "default");
        es.set_project_tenant("pc", "acme");
        es.tenant("acme", "local", false, false, false, true);
        es.user("o", false, &[]);
        es.network("acmenet", None, false, &[], &[], &["acme".into()]);
        es.network("pnet", Some("pa"), false, &[], &[], &[]);
        es.pci_device(LOCAL_ID, "0000:01:00.0", &[], &["acme".into()]);
        let q = |a: &str, r: Ent, proj: &str| {
            tq(Ent::User("o".into()), a, r, auth(0), es.clone())
                .with("project", Ent::Project(proj.into()))
        };
        let net = || Ent::Network("acmenet".into());
        assert!(e.check(&q("useNetwork", net(), "pc")).allowed);
        assert!(
            e.check(&q("useNetwork", net(), "pa"))
                .denied_by("base.network-grant")
        );
        let dev = || Ent::pci_device(LOCAL_ID, "0000:01:00.0");
        assert!(e.check(&q("usePciDevice", dev(), "pc")).allowed);
        assert!(!e.check(&q("usePciDevice", dev(), "pa")).allowed);
        let pnet = || Ent::Network("pnet".into());
        assert!(e.check(&q("offerNetworkShare", pnet(), "pb")).allowed);
        assert!(
            e.check(&q("offerNetworkShare", pnet(), "pc"))
                .denied_by("base.share-same-tenant")
        );
    }

    #[test]
    fn quota_exceed_is_system_admin_only() {
        let e = engine_with(&[
            link(
                "link.o",
                "role.owner",
                Ent::User("o".into()),
                Ent::Project("pa".into()),
            ),
            link(
                "link.s",
                "role.system-admin",
                Ent::User("s".into()),
                Ent::Cluster,
            ),
        ]);
        let mut es = base_entities();
        es.user("o", false, &[]).user("s", false, &[]);
        let q = |p: &str| {
            tq(
                Ent::User(p.into()),
                "exceedQuota",
                Ent::Project("pa".into()),
                auth(0),
                es.clone(),
            )
        };
        assert!(!e.check(&q("o")).allowed);
        assert!(e.check(&q("s")).allowed);
    }

    #[test]
    fn site_policies_forbid_wins_and_break_glass_ignores_them() {
        let e = Engine::new().unwrap();
        let links = [link(
            "link.o",
            "role.owner",
            Ent::User("o".into()),
            Ent::Project("pa".into()),
        )];
        let site = [SiteSource {
            id: "site.lockdown".into(),
            text: "@id(\"site.lockdown\")\nforbid (principal, action, resource);".into(),
            source: PolicySource::Site,
        }];
        e.install(&links, &site).unwrap();
        let mut es = base_entities();
        es.user("o", false, &[])
            .user("root", false, &[BREAK_GLASS_TEAM.into()]);
        let q = |p: &str| {
            tq(
                Ent::User(p.into()),
                "readProject",
                Ent::Project("pa".into()),
                auth(0),
                es.clone(),
            )
        };
        assert!(e.check(&q("o")).denied_by("site.lockdown"));
        let d = e.check(&q("root"));
        assert!(
            d.allowed && d.policies == vec!["base.break-glass"],
            "{:?}",
            d
        );
    }

    #[test]
    fn site_policy_parsing_rules() {
        let e = Engine::new().unwrap();
        assert!(e.validate_site_policy("site.a", "@id(\"site.a\")\npermit (principal, action == Glidex::Action::\"readVm\", resource);").is_ok());
        assert!(matches!(
            e.validate_site_policy(
                "base.x",
                "@id(\"base.x\") permit(principal, action, resource);"
            ),
            Err(AuthzError::Invalid(_))
        ));
        assert!(matches!(
            e.validate_site_policy(
                "site.a",
                "@id(\"site.b\") permit(principal, action, resource);"
            ),
            Err(AuthzError::Invalid(_))
        ));
        assert!(matches!(
            e.validate_site_policy("site.a", "permit(principal, action, resource);"),
            Err(AuthzError::Invalid(_))
        ));
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
            e.validate_site_policy(
                "site.a",
                "@id(\"site.a\") permit(principal, action, resource) when { principal.nope };"
            ),
            Err(AuthzError::Validation(_))
        ));
    }

    #[test]
    fn evaluation_errors_fail_closed() {
        // Validation bypassed on purpose: a forbid that errors at runtime
        // must not turn into an allow.
        let e = Engine::new().unwrap();
        let mut set = e
            .build(
                &[link(
                    "link.o",
                    "role.owner",
                    Ent::User("o".into()),
                    Ent::Project("pa".into()),
                )],
                &[],
            )
            .unwrap();
        let bad = Policy::parse(
            Some(PolicyId::new("site.bad")),
            "forbid (principal, action, resource) when { context.auth.source_ip.isLoopback() };",
        )
        .unwrap();
        set.add(bad).unwrap();
        let mut es = base_entities();
        es.user("o", false, &[]);
        let q = tq(
            Ent::User("o".into()),
            "readProject",
            Ent::Project("pa".into()),
            auth(0),
            es,
        );
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
        e.install(
            &[link(
                "link.o",
                "role.operator",
                Ent::User("o".into()),
                Ent::Project("pa".into()),
            )],
            &site,
        )
        .unwrap();
        let mut es = base_entities();
        es.user("o", false, &[])
            .in_project(Ent::Vm("v".into()), "pa");
        let q = |ip: &str| {
            let mut a = auth(0);
            a.source_ip = Some(ip.parse().unwrap());
            tq(
                Ent::User("o".into()),
                "openConsole",
                Ent::Vm("v".into()),
                a,
                es.clone(),
            )
        };
        assert!(e.check(&q("10.1.2.3")).allowed);
        assert!(e.check(&q("192.0.2.1")).denied_by("site.campus"));
    }

    #[test]
    fn own_sign_ins_only_and_with_a_recent_login() {
        let e = Engine::new().unwrap();
        let mut es = base_entities();
        es.user("a", false, &[]).user("b", false, &[]);
        let q = |r: &str, age: i64| {
            tq(
                Ent::User("a".into()),
                "manageOwnSignIns",
                Ent::User(r.into()),
                auth(age),
                es.clone(),
            )
        };
        assert!(e.check(&q("a", 0)).allowed);
        assert!(!e.check(&q("b", 0)).allowed, "someone else's");
        let late = e.check(&q("a", 3600));
        assert!(
            !late.allowed && late.denied_by("base.step-up"),
            "an old login"
        );
    }

    #[test]
    fn own_tokens_only() {
        let e = Engine::new().unwrap();
        let mut es = base_entities();
        es.user("a", false, &[]).user("b", false, &[]);
        let q = |r: &str| {
            tq(
                Ent::User("a".into()),
                "manageOwnTokens",
                Ent::User(r.into()),
                auth(0),
                es.clone(),
            )
        };
        assert!(e.check(&q("a")).allowed);
        assert!(!e.check(&q("b")).allowed);
    }
}

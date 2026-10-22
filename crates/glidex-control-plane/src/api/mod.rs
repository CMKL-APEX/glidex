//! REST API (spec/rest-api.md) with authentication and Cedar
//! authorization (spec/security.md §5, §7).
//!
//! Every route is declared once in [`routes`] together with the Cedar
//! action it maps to. A per-route layer hands that action to the handler
//! (through [`Caller`]) and audits the request; a global layer
//! authenticates it. Handlers ask Cedar before touching anything.

#![allow(clippy::result_large_err)] // handlers return (StatusCode, Json<ApiError>), as before

mod access;
mod cluster;
mod connectors;
pub(crate) mod errors;
mod exchange;
mod federations;
pub mod gate;
mod invitations;
pub mod issuer;
mod net;
pub(crate) mod nodes;
pub mod op;
mod passkeys;
mod rates;
pub mod relay;
mod rpc;
mod scim;
mod storage;
mod tenants;
pub mod trust;
mod trusted_login;
pub(crate) use rates::live_vm_stats;
pub(crate) use vms::read_log_tail;
mod usage;
pub(crate) mod vms;
mod watch;

pub use errors::{ApiErr, error_to_response};

use crate::api::errors::ApiError;
use crate::auth::{self, AuthError, AuthService, Method, Principal, Transport};
use crate::authz::{Decision, Ent, EntitySet};
use crate::state::VmManager;
use crate::tenancy::QuotaMode;
use axum::{
    Extension, Json, Router,
    extract::{FromRequestParts, Request, State},
    http::{HeaderMap, HeaderValue, Method as HttpMethod, StatusCode, header, request::Parts},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{MethodRouter, delete, get, patch, post, put},
};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

pub struct App {
    pub manager: Arc<VmManager>,
    pub auth: Arc<AuthService>,
}

pub type AppState = Arc<App>;

/// Which listener a request came in on; set by the listener (main.rs).
/// Absent means a plain TCP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listener {
    Tcp,
    /// `api.sock`: local users, identified by peer uid.
    Api,
    /// `ui.sock`: glidex-ui proxying browsers.
    Ui,
    /// A request another server authenticated and forwarded over :8842
    /// (spec/clustering.md §6.4); the principal comes from its header.
    Cluster,
    /// A browser request an agent's UI socket relayed over :8842
    /// (spec/clustering-ui.md §3.6): authenticated here like `Ui`, with the
    /// Origin already checked by the agent.
    Relay,
}

/// The node that relayed a UI request (`Listener::Relay`).
#[derive(Debug, Clone)]
pub struct RelayedBy(pub String);

/// A relayed host-local request meant for this other node's host: the
/// route's action is authorized here on that node's `Host`, then the
/// request runs there (spec/clustering-ui.md §3.6).
#[derive(Debug, Clone)]
pub struct RemoteHost(pub String);

/// A host-local request a server authorized for this host: the route's
/// action on `Host` is allowed without asking this node's policies (an
/// agent has none).
#[derive(Debug, Clone)]
pub struct PreAuthorized(pub Vec<String>);

/// The server that forwarded a request.
#[derive(Debug, Clone)]
pub struct ForwardedBy(pub String);

/// Peer uid of a Unix-socket connection.
#[derive(Debug, Clone, Copy)]
pub struct PeerUid(pub u32);

/// Remote address of a TCP connection.
#[derive(Debug, Clone, Copy)]
pub struct ClientAddr(pub SocketAddr);

#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// The Cedar action a route maps to (set per route).
#[derive(Debug, Clone, Copy)]
pub struct RouteAction(pub &'static str);

/// What a handler learned for the audit entry.
#[derive(Debug, Default)]
struct AuditInfo {
    policies: Vec<String>,
    project: Option<String>,
    target: Option<String>,
    details: serde_json::Map<String, serde_json::Value>,
    denied: bool,
    /// Audit this request even though it is a read (bulk exports).
    always: bool,
}

#[derive(Debug, Clone, Default)]
struct AuditSlot(Arc<Mutex<AuditInfo>>);

/// Route actions that aren't Cedar actions.
pub const PUBLIC: &str = "public";
/// Largest SCIM body (spec/tenancy.md §8.1).
const SCIM_BODY: usize = 1 << 20;
/// Any authenticated principal; the handler scopes the result itself.
pub const AUTHENTICATED: &str = "authenticated";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteSpec {
    pub method: &'static str,
    pub path: &'static str,
    pub action: &'static str,
}

/// Router with authentication off: every request is the break-glass
/// system user. For embedding and tests.
pub fn create_router(manager: Arc<VmManager>) -> Router {
    let auth = AuthService::disabled(manager.database()).expect("auth store");
    router(Arc::new(App { manager, auth }))
}

pub fn router(app: AppState) -> Router {
    // What a server publishes as its web UI's address until its glidex-ui
    // reports one (spec/host-page.md W3); the next self-report writes it.
    let cfg = &app.auth.config;
    app.manager.set_ui_url(
        cfg.cluster
            .ui_origin()
            .or_else(|| cfg.auth.allowed_origins.first().cloned()),
    );
    let (r, _) = routes(&app);
    r.merge(rpc::routes(&app))
        .layer(from_fn_with_state(app.clone(), gate::cluster_gate))
        .layer(from_fn_with_state(app.clone(), authenticate))
        .layer(from_fn_with_state(app.clone(), relay::ui_relay))
        .with_state(app)
}

/// Every route with its action, for tests and documentation.
pub fn route_table(app: &AppState) -> Vec<RouteSpec> {
    routes(app).1
}

struct Routes<'a> {
    app: &'a AppState,
    router: Router<AppState>,
    table: Vec<RouteSpec>,
}

impl Routes<'_> {
    fn add(
        mut self,
        method: &'static str,
        path: &'static str,
        action: &'static str,
        mr: MethodRouter<AppState>,
    ) -> Self {
        self.table.push(RouteSpec {
            method,
            path,
            action,
        });
        let mr: MethodRouter<AppState> = mr
            .layer::<_, std::convert::Infallible>(from_fn_with_state(
                self.app.clone(),
                relay::remote_host,
            ))
            .layer::<_, std::convert::Infallible>(from_fn_with_state(self.app.clone(), audit_layer))
            .layer::<_, std::convert::Infallible>(Extension(RouteAction(action)));
        self.router = self.router.route(path, mr);
        self
    }
}

fn routes(app: &AppState) -> (Router<AppState>, Vec<RouteSpec>) {
    let r = Routes {
        app,
        router: Router::new(),
        table: Vec::new(),
    }
    // ---- VMs
    .add("GET", "/vms", "listVms", get(vms::list))
    .add("POST", "/vms", "createVm", post(vms::create))
    .add(
        "POST",
        "/scheduler/preview",
        AUTHENTICATED,
        post(vms::preview),
    )
    .add("GET", "/vms/{id}", "readVm", get(vms::get_one))
    .add("DELETE", "/vms/{id}", "deleteVm", delete(vms::delete_one))
    .add(
        "PATCH",
        "/vms/{id}",
        "updateVm",
        axum::routing::patch(vms::patch_one),
    )
    .add("GET", "/vms/{id}/events", "readVm", get(vms::events))
    .add("POST", "/vms/{id}/start", "startVm", post(vms::start))
    .add("POST", "/vms/{id}/place", "placeVm", post(vms::place))
    .add("POST", "/vms/{id}/stop", "stopVm", post(vms::stop))
    .add("POST", "/vms/{id}/pause", "pauseVm", post(vms::pause))
    .add(
        "POST",
        "/vms/{id}/extract-kernel",
        "updateVm",
        post(storage::extract_vm_kernel),
    )
    .add(
        "GET",
        "/vms/{id}/console",
        "getConsoleInfo",
        get(vms::console_info),
    )
    .add(
        "POST",
        "/vms/{id}/console/ticket",
        "openConsole",
        post(vms::console_ticket),
    )
    .add(
        "GET",
        "/vms/{id}/console/ws",
        "openConsole",
        get(vms::console_ws),
    )
    .add(
        "GET",
        "/vms/{id}/console/log",
        "openConsole",
        get(vms::console_log),
    )
    .add(
        "POST",
        "/vms/{id}/devices",
        "attachDevice",
        post(vms::attach_device),
    )
    .add(
        "DELETE",
        "/vms/{id}/devices",
        "detachDevice",
        delete(vms::detach_device),
    )
    .add(
        "POST",
        "/vms/{id}/disks",
        "attachDisk",
        post(vms::attach_disk),
    )
    .add(
        "DELETE",
        "/vms/{id}/disks/{disk}",
        "detachDisk",
        delete(vms::detach_disk),
    )
    // ---- guest credentials
    .add(
        "GET",
        "/credentials",
        "listCredentials",
        get(storage::list_credentials),
    )
    .add(
        "POST",
        "/credentials",
        "createCredential",
        post(storage::create_credential),
    )
    .add(
        "GET",
        "/credentials/{username}",
        "readCredential",
        get(storage::get_credential),
    )
    .add(
        "PUT",
        "/credentials/{username}",
        "updateCredential",
        put(storage::update_credential),
    )
    .add(
        "DELETE",
        "/credentials/{username}",
        "deleteCredential",
        delete(storage::delete_credential),
    )
    // ---- images and disks
    .add(
        "GET",
        "/images/catalog",
        "readImage",
        get(storage::image_catalog),
    )
    .add(
        "GET",
        "/images/firmware-catalog",
        "readImage",
        get(storage::firmware_catalog),
    )
    .add(
        "GET",
        "/datastores",
        "readDatastore",
        get(storage::list_datastores),
    )
    .add(
        "POST",
        "/datastores",
        "manageDatastore",
        post(storage::create_datastore),
    )
    .add(
        "GET",
        "/datastores/{id}",
        "readDatastore",
        get(storage::get_datastore),
    )
    .add(
        "PATCH",
        "/datastores/{id}",
        "manageDatastore",
        patch(storage::update_datastore),
    )
    .add(
        "DELETE",
        "/datastores/{id}",
        "manageDatastore",
        delete(storage::delete_datastore),
    )
    .add("GET", "/images", "readImage", get(storage::list_images))
    .add("POST", "/images", "pullImage", post(storage::pull_image))
    .add("GET", "/images/{id}", "readImage", get(storage::get_image))
    .add(
        "DELETE",
        "/images/{id}",
        "deleteImage",
        delete(storage::delete_image),
    )
    .add(
        "POST",
        "/images/{id}/retry",
        "pullImage",
        post(storage::retry_image),
    )
    .add(
        "POST",
        "/images/{id}/copies",
        "pullImage",
        post(storage::add_image_copy),
    )
    .add(
        "DELETE",
        "/images/{id}/copies/{node}",
        "pullImage",
        delete(storage::remove_image_copy),
    )
    .add(
        "GET",
        "/images/{id}/events",
        "readImage",
        get(storage::image_events),
    )
    .add(
        "GET",
        "/disks/{id}/events",
        "readDisk",
        get(storage::disk_events),
    )
    .add("GET", "/disks", "listDisks", get(storage::list_disks))
    .add("POST", "/disks", "createDisk", post(storage::create_disk))
    .add("GET", "/disks/{id}", "readDisk", get(storage::get_disk))
    .add(
        "DELETE",
        "/disks/{id}",
        "deleteDisk",
        delete(storage::delete_disk),
    )
    .add(
        "POST",
        "/disks/{id}/move",
        "moveDisk",
        post(storage::move_disk),
    )
    .add(
        "POST",
        "/disks/{id}/resize",
        "resizeDisk",
        post(storage::resize_disk),
    )
    .add(
        "POST",
        "/disks/{id}/extend-root",
        "extendRoot",
        post(storage::extend_root),
    )
    // ---- networks
    .add("GET", "/networks", "readNetwork", get(net::list_networks))
    .add(
        "POST",
        "/networks",
        "createNetwork",
        post(net::create_network),
    )
    .add(
        "GET",
        "/networks/{id}",
        "readNetwork",
        get(net::get_network),
    )
    .add(
        "DELETE",
        "/networks/{id}",
        "deleteNetwork",
        delete(net::delete_network),
    )
    .add(
        "GET",
        "/networks/{id}/events",
        "readNetwork",
        get(net::network_events),
    )
    .add(
        "GET",
        "/networks/{id}/ovn",
        "readNetwork",
        get(net::network_ovn),
    )
    .add(
        "PUT",
        "/networks/{id}/grants",
        "grantNetwork",
        put(net::grant_network),
    )
    .add(
        "POST",
        "/networks/{id}/shares",
        "offerNetworkShare",
        post(net::offer_share),
    )
    .add(
        "DELETE",
        "/networks/{id}/shares/{project}",
        "unshareNetwork",
        delete(net::unshare),
    )
    .add(
        "POST",
        "/projects/{id}/networks",
        "createProjectNetwork",
        post(net::create_project_network),
    )
    .add(
        "PUT",
        "/projects/{id}/networks/{network}/router",
        "createRouter",
        put(net::set_network_router),
    )
    .add(
        "GET",
        "/projects/{id}/routers",
        "readRouter",
        get(net::list_routers),
    )
    .add(
        "POST",
        "/projects/{id}/routers",
        "createRouter",
        post(net::create_router),
    )
    .add(
        "GET",
        "/projects/{id}/routers/{router}",
        "readRouter",
        get(net::get_router),
    )
    .add(
        "DELETE",
        "/projects/{id}/routers/{router}",
        "deleteRouter",
        delete(net::delete_router),
    )
    .add(
        "GET",
        "/projects/{id}/network-shares",
        "listNetworkShares",
        get(net::list_shares),
    )
    .add(
        "POST",
        "/projects/{id}/network-shares/{network}/accept",
        "acceptNetworkShare",
        post(net::accept_share),
    )
    .add(
        "DELETE",
        "/projects/{id}/network-shares/{network}",
        "leaveNetworkShare",
        delete(net::leave_share),
    )
    // ---- host networking
    .add("GET", "/ovs/status", "readOvsStatus", get(net::ovs_status))
    .add("POST", "/ovs/install", "installOvs", post(net::ovs_install))
    .add(
        "POST",
        "/ovs/dpdk-init",
        "initDpdk",
        post(net::ovs_dpdk_init),
    )
    .add("GET", "/ovs/bridges", "listBridges", get(net::list_bridges))
    .add(
        "POST",
        "/ovs/bridges",
        "createBridge",
        post(net::create_bridge),
    )
    .add(
        "DELETE",
        "/ovs/bridges/{name}",
        "deleteBridge",
        delete(net::delete_bridge),
    )
    .add(
        "GET",
        "/ovs/bridges/{name}/uplinks",
        "listUplinks",
        get(net::list_uplinks),
    )
    .add(
        "POST",
        "/ovs/bridges/{name}/uplinks",
        "ensureUplink",
        post(net::create_uplink),
    )
    .add(
        "DELETE",
        "/ovs/bridges/{name}/uplinks/{uplink}",
        "deleteUplink",
        delete(net::delete_uplink),
    )
    .add(
        "POST",
        "/ovs/bridges/{name}/uplinks/{uplink}/commit",
        "commitUplink",
        post(net::commit_uplink),
    )
    .add(
        "POST",
        "/ovs/bridges/{name}/uplinks/{uplink}/reapply",
        "confirmUplink",
        post(net::reapply_uplink),
    )
    .add(
        "GET",
        "/pci-devices",
        "listPciDevices",
        get(net::list_pci_devices),
    )
    .add(
        "GET",
        "/host/topology",
        "readHostTopology",
        get(net::host_topology),
    )
    .add(
        "GET",
        "/system/reconcile",
        "readSystemStatus",
        get(vms::system_reconcile),
    )
    // ---- cluster (spec/clustering.md §5)
    .add("POST", "/cluster/init", "initCluster", post(cluster::init))
    .add("POST", "/cluster/join", "joinCluster", post(cluster::join))
    .add(
        "GET",
        "/cluster/status",
        "readCluster",
        get(cluster::status),
    )
    .add(
        "GET",
        "/cluster/ovn",
        "readCluster",
        get(cluster::cluster_ovn),
    )
    .add(
        "GET",
        "/cluster/ovn/tunnels",
        "readCluster",
        get(cluster::cluster_ovn_tunnels),
    )
    .add(
        "GET",
        "/cluster/settings/ovn",
        AUTHENTICATED,
        get(cluster::get_ovn_settings),
    )
    .add(
        "PUT",
        "/cluster/settings/ovn",
        "configureOvn",
        axum::routing::put(cluster::put_ovn_settings),
    )
    .add(
        "GET",
        "/cluster/settings/ovn/edge-defaults",
        "readCluster",
        get(cluster::edge_defaults),
    )
    .add(
        "POST",
        "/cluster/join-tokens",
        "createJoinToken",
        post(cluster::create_token),
    )
    .add(
        "POST",
        "/cluster/promote",
        "promoteNode",
        post(cluster::promote),
    )
    .add(
        "POST",
        "/cluster/import",
        "importCluster",
        post(cluster::import_join),
    )
    .add(
        "POST",
        "/cluster/rejoin",
        "rejoinCluster",
        post(cluster::rejoin),
    )
    .add(
        "POST",
        "/cluster/dissolve",
        "dissolveCluster",
        post(cluster::dissolve),
    )
    .add(
        "POST",
        "/cluster/leave",
        "leaveCluster",
        post(cluster::leave),
    )
    .add(
        "POST",
        "/cluster/rotate-ca",
        "rotateCa",
        post(cluster::rotate_ca),
    )
    .add(
        "POST",
        "/cluster/rotate-secrets-key",
        "rotateSecretsKey",
        post(cluster::rotate_secrets_key),
    )
    .add(
        "GET",
        "/cluster/snapshot",
        "snapshotCluster",
        get(cluster::snapshot),
    )
    // ---- nodes (spec/clustering.md §7)
    .add("GET", "/nodes", "listNodes", get(nodes::list))
    .add("GET", "/nodes/{id}", "readNode", get(nodes::get_node))
    .add(
        "GET",
        "/nodes/{id}/topology",
        "readHostTopology",
        get(nodes::topology),
    )
    .add("GET", "/pci-pools", "listPciPools", get(nodes::pci_pools))
    .add(
        "POST",
        "/nodes/{id}/devices/{address}/prepare",
        "prepareDevice",
        post(nodes::prepare_device),
    )
    .add(
        "POST",
        "/nodes/{id}/devices/{address}/release",
        "releaseDevice",
        post(nodes::release_device),
    )
    .add(
        "POST",
        "/nodes/{id}/devices/{address}/sriov",
        "setSriovVfs",
        post(nodes::set_sriov_vfs),
    )
    .add(
        "POST",
        "/nodes/{id}/devices/{address}/cordon",
        "cordonDevice",
        post(nodes::cordon_device),
    )
    .add(
        "POST",
        "/nodes/{id}/devices/{address}/uncordon",
        "cordonDevice",
        post(nodes::uncordon_device),
    )
    .add("POST", "/nodes/{id}/drain", "drainNode", post(nodes::drain))
    .add(
        "PUT",
        "/nodes/{id}/ovn",
        "configureNode",
        axum::routing::put(nodes::put_node_ovn),
    )
    .add(
        "POST",
        "/nodes/{id}/spare-image-files/delete",
        "pullImage",
        post(nodes::delete_spare_image_files),
    )
    .add(
        "POST",
        "/nodes/{id}/undrain",
        "undrainNode",
        post(nodes::undrain),
    )
    .add(
        "POST",
        "/nodes/{id}/remove",
        "removeNode",
        post(nodes::remove),
    )
    .add(
        "POST",
        "/nodes/{id}/forget",
        "forgetNode",
        post(nodes::forget),
    )
    .add(
        "POST",
        "/nodes/{id}/detach",
        "detachNode",
        post(nodes::detach),
    )
    .add("GET", "/imports", "listImports", get(nodes::list_imports))
    .add(
        "GET",
        "/departures",
        "readCluster",
        get(nodes::list_departures),
    )
    .add(
        "GET",
        "/imports/{plan}",
        "readImport",
        get(nodes::get_import),
    )
    .add(
        "POST",
        "/imports/{plan}/approve",
        "approveImport",
        post(nodes::approve_import),
    )
    .add(
        "POST",
        "/imports/{plan}/reject",
        "rejectImport",
        post(nodes::reject_import),
    )
    .add("POST", "/nodes/{id}/purge", "purgeNode", post(nodes::purge))
    .add(
        "POST",
        "/nodes/{id}/rejoin-token",
        "createRejoinToken",
        post(nodes::rejoin_token),
    )
    // Live changes; each kind filtered like its list endpoint.
    .add("GET", "/watch", AUTHENTICATED, get(watch::watch))
    // ---- authentication
    .add("GET", "/health", PUBLIC, get(health_check))
    .add("GET", "/auth/methods", PUBLIC, get(access::methods))
    .add("GET", "/auth/server-info", PUBLIC, get(access::server_info))
    .add("POST", "/auth/login", PUBLIC, post(access::login))
    .add("POST", "/auth/token", PUBLIC, post(access::pam_token))
    .add("GET", "/auth/oidc/start", PUBLIC, get(access::oidc_start))
    .add(
        "GET",
        "/auth/oidc/callback",
        PUBLIC,
        get(access::oidc_callback),
    )
    .add(
        "POST",
        "/auth/oidc/device",
        PUBLIC,
        post(access::oidc_device_start),
    )
    .add(
        "POST",
        "/auth/oidc/device/poll",
        PUBLIC,
        post(access::oidc_device_poll),
    )
    // Directory connectors (spec/tenancy.md §6): discovery and each
    // connector's own login routes.
    .add("POST", "/auth/discover", PUBLIC, post(access::discover))
    // Workload federation (spec/tenancy.md §9): a CI job's JWT for a
    // short-lived token; the JWT is the credential.
    .add("POST", "/auth/exchange", PUBLIC, post(exchange::exchange))
    // A directory as an issuer (spec/tenancy.md D52, D53).
    .add(
        "GET",
        "/d/{dir}/.well-known/openid-configuration",
        PUBLIC,
        get(issuer::discovery),
    )
    .add("GET", "/d/{dir}/jwks.json", PUBLIC, get(issuer::jwks))
    // The directory as an OpenID provider (D54–D57).
    .add("GET", "/d/{dir}/authorize", PUBLIC, get(op::authorize))
    .add(
        "POST",
        "/d/{dir}/device_authorization",
        PUBLIC,
        post(op::device_authorization),
    )
    .add("POST", "/d/{dir}/token", PUBLIC, post(op::token))
    .add("POST", "/d/{dir}/revoke", PUBLIC, post(op::revoke))
    // Trust between organizations (D63–D66): peer assertions, not users.
    .add("POST", "/trust/v1/redeem", PUBLIC, post(trust::redeem))
    .add("POST", "/trust/v1/end", PUBLIC, post(trust::end_at_cluster))
    .add(
        "POST",
        "/d/{dir}/trust/v1/end",
        PUBLIC,
        post(trust::end_at_directory),
    )
    .add("POST", "/d/{dir}/feed/v1", PUBLIC, post(trust::feed))
    .add(
        "POST",
        "/d/{dir}/device/inspect",
        AUTHENTICATED,
        post(op::device_inspect),
    )
    .add(
        "POST",
        "/d/{dir}/device/answer",
        AUTHENTICATED,
        post(op::device_answer),
    )
    // Passkeys (spec/passkeys.md): the browser's ceremonies.
    .add(
        "POST",
        "/auth/passkeys/login/start",
        PUBLIC,
        post(passkeys::login_start),
    )
    .add(
        "POST",
        "/auth/passkeys/login/finish",
        PUBLIC,
        post(passkeys::login_finish),
    )
    .add(
        "POST",
        "/auth/passkeys/register/start",
        AUTHENTICATED,
        post(passkeys::register_start),
    )
    .add(
        "POST",
        "/auth/passkeys/register/finish",
        AUTHENTICATED,
        post(passkeys::register_finish),
    )
    .add(
        "GET",
        "/auth/c/{id}/start",
        PUBLIC,
        get(access::connector_start),
    )
    // An invitation page's login: the invitation in the body (§6.1).
    .add(
        "POST",
        "/auth/c/{id}/start",
        PUBLIC,
        post(access::connector_start_post),
    )
    .add(
        "POST",
        "/auth/invitation",
        PUBLIC,
        post(invitations::inspect),
    )
    // A passkey invitation's first passkey (spec/tenancy.md D60).
    .add(
        "POST",
        "/auth/invitation/passkey/start",
        PUBLIC,
        post(passkeys::invite_start),
    )
    .add(
        "POST",
        "/auth/invitation/passkey/finish",
        PUBLIC,
        post(passkeys::invite_finish),
    )
    // Add a sign-in method to the caller (§6.2): `manageOwnSignIns` inside.
    .add(
        "POST",
        "/auth/link/c/{id}/start",
        AUTHENTICATED,
        post(access::link_start),
    )
    .add(
        "GET",
        "/auth/c/{id}/callback",
        PUBLIC,
        get(access::connector_callback),
    )
    // Logins at a trusted directory (spec/tenancy.md D69).
    .add(
        "GET",
        "/auth/t/{trusted}/start",
        PUBLIC,
        get(trusted_login::start),
    )
    .add(
        "GET",
        "/auth/t/{trusted}/callback",
        PUBLIC,
        get(trusted_login::callback),
    )
    .add(
        "POST",
        "/auth/c/{id}/device",
        PUBLIC,
        post(access::connector_device_start),
    )
    .add(
        "POST",
        "/auth/c/{id}/device/poll",
        PUBLIC,
        post(access::connector_device_poll),
    )
    .add("POST", "/auth/logout", AUTHENTICATED, post(access::logout))
    .add(
        "POST",
        "/auth/session",
        AUTHENTICATED,
        post(access::peer_session),
    )
    .add("GET", "/auth/whoami", AUTHENTICATED, get(access::whoami))
    .add(
        "POST",
        "/authz/check",
        AUTHENTICATED,
        post(access::authz_check),
    )
    .add(
        "POST",
        "/authz/allowed",
        AUTHENTICATED,
        post(access::authz_allowed),
    )
    // ---- tokens
    .add("GET", "/tokens", AUTHENTICATED, get(access::list_tokens))
    .add("POST", "/tokens", AUTHENTICATED, post(access::create_token))
    .add(
        "DELETE",
        "/tokens/{id}",
        AUTHENTICATED,
        delete(access::revoke_token),
    )
    // ---- users and teams
    .add("GET", "/users", "listUsers", get(access::list_users))
    .add("GET", "/users/me", AUTHENTICATED, get(access::whoami))
    .add(
        "PATCH",
        "/users/me",
        AUTHENTICATED,
        axum::routing::patch(access::update_me),
    )
    .add(
        "GET",
        "/users/me/ssh-keys",
        AUTHENTICATED,
        get(access::my_ssh_keys),
    )
    .add("POST", "/users", "manageUsers", post(access::create_user))
    .add(
        "PATCH",
        "/users/{id}",
        "manageUsers",
        axum::routing::patch(access::update_user),
    )
    .add(
        "DELETE",
        "/users/{id}",
        "manageUsers",
        delete(access::delete_user),
    )
    .add(
        "POST",
        "/users/{id}/identities",
        "manageUsers",
        post(access::link_identity),
    )
    .add(
        "DELETE",
        "/users/{id}/identities/{key}",
        "manageUsers",
        delete(access::unlink_identity),
    )
    .add(
        "GET",
        "/users/{id}/passkeys",
        "listUsers",
        get(access::list_passkeys),
    )
    .add(
        "PATCH",
        "/users/{id}/passkeys/{passkey}",
        "manageUsers",
        axum::routing::patch(access::rename_passkey),
    )
    .add(
        "DELETE",
        "/users/{id}/passkeys/{passkey}",
        "manageUsers",
        delete(access::delete_passkey),
    )
    .add("GET", "/teams", "listTeams", get(access::list_teams))
    .add("POST", "/teams", "manageTeams", post(access::create_team))
    .add(
        "PATCH",
        "/teams/{id}",
        "manageTeams",
        axum::routing::patch(access::update_team),
    )
    .add(
        "DELETE",
        "/teams/{id}",
        "manageTeams",
        delete(access::delete_team),
    )
    .add(
        "PUT",
        "/teams/{id}/members/{user}",
        "manageTeams",
        put(access::add_member),
    )
    .add(
        "DELETE",
        "/teams/{id}/members/{user}",
        "manageTeams",
        delete(access::remove_member),
    )
    // ---- tenants and admissions (spec/tenancy.md §14; REST twins of glidex.tenancy.v1)
    .add("GET", "/tenants", AUTHENTICATED, get(tenants::list))
    .add("POST", "/tenants", "createTenant", post(tenants::create))
    .add("GET", "/tenants/{id}", "readTenant", get(tenants::get))
    .add(
        "PATCH",
        "/tenants/{id}",
        "updateTenant",
        axum::routing::patch(tenants::update),
    )
    .add(
        "DELETE",
        "/tenants/{id}",
        "deleteTenant",
        delete(tenants::delete),
    )
    .add(
        "POST",
        "/tenants/{id}/suspend",
        "suspendTenant",
        post(tenants::suspend),
    )
    .add(
        "POST",
        "/tenants/{id}/resume",
        "resumeTenant",
        post(tenants::resume),
    )
    .add(
        "GET",
        "/tenants/{id}/members",
        "readTenantMembers",
        get(tenants::list_members),
    )
    .add(
        "POST",
        "/tenants/{id}/members",
        "manageTenantMembers",
        post(tenants::add_member),
    )
    .add(
        "DELETE",
        "/tenants/{id}/members/{principal}",
        "manageTenantMembers",
        delete(tenants::remove_member),
    )
    .add(
        "GET",
        "/tenants/{id}/device-pools",
        "readTenantDevicePools",
        get(tenants::list_device_pools),
    )
    .add(
        "PUT",
        "/tenants/{id}/device-pools",
        "manageTenantDevicePools",
        put(tenants::set_device_pools),
    )
    .add(
        "GET",
        "/tenants/{id}/bindings",
        "readTenantBindings",
        get(tenants::list_bindings),
    )
    .add(
        "POST",
        "/tenants/{id}/bindings",
        "manageTenantBindings",
        post(tenants::add_binding),
    )
    .add(
        "DELETE",
        "/tenants/{id}/bindings/{link}",
        "manageTenantBindings",
        delete(tenants::remove_binding),
    )
    // ---- invitations (spec/tenancy.md §6.1)
    .add(
        "GET",
        "/tenants/{id}/federations",
        "readTenantFederations",
        get(federations::list),
    )
    .add(
        "POST",
        "/tenants/{id}/federations",
        "manageTenantFederations",
        post(federations::create),
    )
    .add(
        "GET",
        "/tenants/{id}/federations/{federation}",
        "readTenantFederations",
        get(federations::get),
    )
    .add(
        "PATCH",
        "/tenants/{id}/federations/{federation}",
        "manageTenantFederations",
        axum::routing::patch(federations::update),
    )
    .add(
        "DELETE",
        "/tenants/{id}/federations/{federation}",
        "manageTenantFederations",
        delete(federations::delete),
    )
    .add(
        "GET",
        "/tenants/{id}/invitations",
        "readTenantInvitations",
        get(invitations::list),
    )
    .add(
        "POST",
        "/tenants/{id}/invitations",
        "manageTenantInvitations",
        post(invitations::create),
    )
    .add(
        "DELETE",
        "/invitations/{id}",
        "manageTenantInvitations",
        delete(invitations::revoke),
    )
    // ---- the directory's signing keys (spec/tenancy.md D53)
    .add(
        "GET",
        "/directory/keys",
        "readSigningKeys",
        get(issuer::list_keys),
    )
    .add(
        "POST",
        "/directory/keys/rotate",
        "rotateSigningKey",
        post(issuer::rotate_key),
    )
    // ---- directory connectors (spec/tenancy.md §5; REST twins of glidex.directory.v1)
    .add(
        "GET",
        "/connectors",
        "listConnectors",
        get(connectors::list),
    )
    .add(
        "POST",
        "/connectors",
        "createConnector",
        post(connectors::create),
    )
    .add(
        "GET",
        "/connectors/{id}",
        "readConnector",
        get(connectors::get),
    )
    .add(
        "PATCH",
        "/connectors/{id}",
        "updateConnector",
        axum::routing::patch(connectors::update),
    )
    .add(
        "DELETE",
        "/connectors/{id}",
        "deleteConnector",
        delete(connectors::delete),
    )
    .add(
        "PUT",
        "/connectors/{id}/secrets/{field}",
        "setConnectorSecret",
        put(connectors::set_secret),
    )
    .add(
        "POST",
        "/connectors/{id}/test",
        "testConnector",
        post(connectors::test),
    )
    .add(
        "POST",
        "/connectors/{id}/sync",
        "syncConnector",
        post(connectors::sync),
    )
    .add(
        "GET",
        "/connectors/{id}/scim-tokens",
        "readScimTokens",
        get(connectors::list_scim_tokens),
    )
    .add(
        "POST",
        "/connectors/{id}/scim-tokens",
        "manageScimTokens",
        post(connectors::create_scim_token),
    )
    .add(
        "DELETE",
        "/connectors/{id}/scim-tokens/{token}",
        "manageScimTokens",
        delete(connectors::revoke_scim_token),
    )
    // SCIM 2.0 (spec/tenancy.md §8.1): its own bearer tokens, checked by
    // the handlers; bodies up to 1 MiB.
    .add(
        "GET",
        "/scim/v2/c/{id}/ServiceProviderConfig",
        PUBLIC,
        get(scim::service_provider_config),
    )
    .add("GET", "/scim/v2/c/{id}/Schemas", PUBLIC, get(scim::schemas))
    .add(
        "GET",
        "/scim/v2/c/{id}/ResourceTypes",
        PUBLIC,
        get(scim::resource_types),
    )
    .add("GET", "/scim/v2/c/{id}/Users", PUBLIC, get(scim::users))
    .add(
        "POST",
        "/scim/v2/c/{id}/Users",
        PUBLIC,
        post(scim::users).layer(axum::extract::DefaultBodyLimit::max(SCIM_BODY)),
    )
    .add(
        "GET",
        "/scim/v2/c/{id}/Users/{uid}",
        PUBLIC,
        get(scim::user),
    )
    .add(
        "PUT",
        "/scim/v2/c/{id}/Users/{uid}",
        PUBLIC,
        put(scim::user).layer(axum::extract::DefaultBodyLimit::max(SCIM_BODY)),
    )
    .add(
        "PATCH",
        "/scim/v2/c/{id}/Users/{uid}",
        PUBLIC,
        axum::routing::patch(scim::user).layer(axum::extract::DefaultBodyLimit::max(SCIM_BODY)),
    )
    .add(
        "DELETE",
        "/scim/v2/c/{id}/Users/{uid}",
        PUBLIC,
        delete(scim::user),
    )
    .add("GET", "/scim/v2/c/{id}/Groups", PUBLIC, get(scim::groups))
    .add(
        "POST",
        "/scim/v2/c/{id}/Groups",
        PUBLIC,
        post(scim::groups).layer(axum::extract::DefaultBodyLimit::max(SCIM_BODY)),
    )
    .add(
        "GET",
        "/scim/v2/c/{id}/Groups/{gid}",
        PUBLIC,
        get(scim::group),
    )
    .add(
        "PUT",
        "/scim/v2/c/{id}/Groups/{gid}",
        PUBLIC,
        put(scim::group).layer(axum::extract::DefaultBodyLimit::max(SCIM_BODY)),
    )
    .add(
        "PATCH",
        "/scim/v2/c/{id}/Groups/{gid}",
        PUBLIC,
        axum::routing::patch(scim::group).layer(axum::extract::DefaultBodyLimit::max(SCIM_BODY)),
    )
    .add(
        "DELETE",
        "/scim/v2/c/{id}/Groups/{gid}",
        PUBLIC,
        delete(scim::group),
    )
    .add(
        "GET",
        "/directories",
        "listDirectories",
        get(tenants::list_directories),
    )
    .add(
        "POST",
        "/directories",
        "manageDirectories",
        post(tenants::create_directory),
    )
    .add(
        "DELETE",
        "/directories/{id}",
        "manageDirectories",
        delete(tenants::delete_directory),
    )
    .add(
        "GET",
        "/admissions",
        "listAdmissions",
        get(tenants::list_admissions),
    )
    .add(
        "POST",
        "/admissions/offers",
        "manageAdmissions",
        post(tenants::offer_admission),
    )
    .add(
        "GET",
        "/admissions/offers",
        "listAdmissions",
        get(tenants::list_offers),
    )
    .add(
        "GET",
        "/admissions/directories",
        "listAdmissions",
        get(tenants::list_trusted_directories),
    )
    .add(
        "GET",
        "/tenants/{id}/clusters",
        "readTenantClusters",
        get(tenants::list_tenant_clusters),
    )
    .add(
        "POST",
        "/tenants/{id}/clusters",
        "manageTenantClusters",
        post(tenants::opt_in_cluster),
    )
    .add(
        "DELETE",
        "/tenants/{id}/clusters/{cluster}",
        "manageTenantClusters",
        delete(tenants::opt_out_cluster),
    )
    .add(
        "DELETE",
        "/admissions/offers/{id}",
        "manageAdmissions",
        delete(tenants::revoke_offer),
    )
    .add(
        "PUT",
        "/admissions/{tenant}",
        "manageAdmissions",
        put(tenants::set_admission),
    )
    .add(
        "DELETE",
        "/admissions/{tenant}",
        "manageAdmissions",
        delete(tenants::remove_admission),
    )
    // ---- projects and role links
    .add(
        "GET",
        "/projects",
        AUTHENTICATED,
        get(access::list_projects),
    )
    .add(
        "POST",
        "/projects",
        "createProject",
        post(access::create_project),
    )
    .add(
        "GET",
        "/projects/{id}",
        "readProject",
        get(access::get_project),
    )
    .add(
        "PATCH",
        "/projects/{id}",
        "updateProject",
        axum::routing::patch(access::update_project),
    )
    .add(
        "DELETE",
        "/projects/{id}",
        "deleteProject",
        delete(access::delete_project),
    )
    .add(
        "GET",
        "/projects/{id}/bindings",
        "readBindings",
        get(access::list_bindings),
    )
    .add(
        "POST",
        "/projects/{id}/bindings",
        "manageBindings",
        post(access::add_binding),
    )
    .add(
        "DELETE",
        "/projects/{id}/bindings/{link}",
        "manageBindings",
        delete(access::remove_binding),
    )
    .add(
        "GET",
        "/system/bindings",
        "readSystemBindings",
        get(access::list_system_bindings),
    )
    .add(
        "POST",
        "/system/bindings",
        "manageSystemBindings",
        post(access::add_system_binding),
    )
    .add(
        "DELETE",
        "/system/bindings/{link}",
        "manageSystemBindings",
        delete(access::remove_system_binding),
    )
    // ---- policies and audit
    .add(
        "GET",
        "/authz/policies",
        "readPolicy",
        get(access::list_policies),
    )
    .add(
        "GET",
        "/authz/policies/{id}",
        "readPolicy",
        get(access::get_policy),
    )
    .add(
        "GET",
        "/authz/policies/{id}/versions",
        "readPolicy",
        get(access::policy_versions),
    )
    .add(
        "PUT",
        "/authz/policies/{id}",
        "writePolicy",
        put(access::put_policy),
    )
    .add(
        "DELETE",
        "/authz/policies/{id}",
        "deletePolicy",
        delete(access::delete_policy),
    )
    .add(
        "POST",
        "/authz/validate",
        "validatePolicy",
        post(access::validate_policy),
    )
    .add(
        "POST",
        "/authz/simulate",
        "simulatePolicy",
        post(access::simulate_policy),
    )
    .add(
        "POST",
        "/authz/reload",
        "reloadPolicies",
        post(access::reload_policies),
    )
    .add("GET", "/audit", AUTHENTICATED, get(access::read_audit))
    // ---- usage (spec/metering.md §9)
    .add("GET", "/usage", "readUsage", get(usage::usage))
    .add(
        "GET",
        "/projects/{id}/usage",
        "readProjectUsage",
        get(usage::project_usage),
    )
    .add(
        "GET",
        "/vms/{id}/usage",
        "readVmUsage",
        get(usage::vm_usage),
    )
    .add(
        "GET",
        "/disks/{id}/usage",
        "readDiskUsage",
        get(usage::disk_usage),
    )
    .add(
        "GET",
        "/usage/bandwidth",
        "readBandwidth",
        get(rates::bandwidth),
    )
    .add(
        "GET",
        "/usage/disk-io",
        "readDiskIoUsage",
        get(rates::disk_io),
    )
    .add(
        "GET",
        "/usage/compute",
        "readComputeUsage",
        get(rates::compute),
    )
    .add(
        "GET",
        "/vms/{id}/bandwidth",
        "readVmBandwidth",
        get(rates::vm_bandwidth),
    )
    .add("GET", "/vms/{id}/io", "readVmIo", get(rates::vm_io))
    .add(
        "GET",
        "/vms/{id}/compute",
        "readVmCompute",
        get(rates::vm_compute),
    )
    .add(
        "GET",
        "/vms/{id}/stats",
        "readVmStats",
        get(rates::vm_stats),
    )
    .add(
        "GET",
        "/disks/{id}/io",
        "readDiskIo",
        get(rates::disk_io_series),
    )
    .add(
        "GET",
        "/networks/{id}/bandwidth",
        "readNetworkBandwidth",
        get(rates::network_bandwidth),
    )
    .add(
        "GET",
        "/networks/{id}/stats",
        "readNetworkStats",
        get(rates::network_stats),
    );
    (r.router, r.table)
}

async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// A REST answer: the message as proto3 JSON (spec/grpc.md §9.1).
pub(crate) struct Proto<T>(pub StatusCode, pub T);

impl<T: serde::Serialize> axum::response::IntoResponse for Proto<T> {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(self.1)).into_response()
    }
}

/// `204`, a REST answer with no message.
pub(crate) fn proto_no_content() -> axum::response::Response {
    StatusCode::NO_CONTENT.into_response()
}

/// Read a REST body as proto message `T` (`422` when it isn't one).
pub(crate) fn proto_body<T: serde::de::DeserializeOwned + buffa::MessageName>(
    v: serde_json::Value,
) -> Result<T, ApiErr> {
    // proto3 JSON refuses unknown fields by default; a typo isn't ignored.
    if let Some(f) = glidex_api::unknown_field(T::FULL_NAME, &v) {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            format!("unknown field `{f}`"),
        ));
    }
    // As axum's `Json` answers a body that is JSON but not the message.
    serde_json::from_value(v)
        .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, "invalid", e.to_string()))
}

pub(crate) fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ApiErr {
    (status, Json(ApiError::new(code, message)))
}

fn plain_error(status: StatusCode, code: &str, message: &str) -> Response {
    err(status, code, message).into_response()
}

fn is_safe(m: &HttpMethod) -> bool {
    matches!(*m, HttpMethod::GET | HttpMethod::HEAD | HttpMethod::OPTIONS)
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
}

/// Client IP: the TCP peer, or (only from glidex-ui) X-Forwarded-For.
/// Anything a client can send is never trusted on its own: a rate limit
/// or an audit entry keyed on it would be the client's to choose.
pub(crate) fn client_ip(
    peer: Option<std::net::SocketAddr>,
    headers: &HeaderMap,
    listener: Listener,
) -> Option<IpAddr> {
    match listener {
        Listener::Ui | Listener::Relay => headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse().ok()),
        Listener::Tcp => peer.map(|a| a.ip()),
        Listener::Api | Listener::Cluster => None,
    }
}

/// Global layer: request id, Origin check, and the principal.
async fn authenticate(State(app): State<AppState>, mut req: Request, next: Next) -> Response {
    let request_id = auth::random_token(12);
    req.extensions_mut().insert(RequestId(request_id.clone()));
    let listener = req
        .extensions()
        .get::<Listener>()
        .copied()
        .unwrap_or(Listener::Tcp);
    let headers = req.headers().clone();
    let safe = is_safe(req.method());
    let websocket = headers.get(header::UPGRADE).is_some();

    // Origin (spec §5.6): browsers send it on cross-origin requests and
    // on every WebSocket upgrade.
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if let Some(o) = &origin {
        // A forwarded or relayed request was checked by the node that took it.
        if !matches!(listener, Listener::Cluster | Listener::Relay)
            && (!safe || websocket)
            && !app.auth.config.auth.origin_allowed(o)
            && !(listener == Listener::Ui
                && ui_same_origin(o, &headers)
                && ui_peer_ok(&app, req.extensions().get::<PeerUid>()))
        {
            return plain_error(
                StatusCode::FORBIDDEN,
                "origin_not_allowed",
                "this origin may not use the API",
            );
        }
    }

    // SCIM (spec/tenancy.md §8.1): the IdP's `gxs_` token is the SCIM
    // handlers' to check; it is no principal of the API.
    let scim = req.uri().path().starts_with("/scim/v2/");
    let principal = if app.auth.is_disabled() {
        Some(Principal::system())
    } else if scim {
        None
    } else {
        let resolved = match listener {
            Listener::Api => match req.extensions().get::<PeerUid>() {
                Some(PeerUid(uid)) => match app.auth.peer_principal(*uid) {
                    Ok(Some(p)) => Ok(Some(p)),
                    Ok(None) => {
                        return plain_error(
                            StatusCode::FORBIDDEN,
                            "forbidden",
                            "this user may not use the glidex socket",
                        );
                    }
                    Err(e) => Err(e),
                },
                None => Ok(None),
            },
            Listener::Cluster => gate::forwarded_principal(&headers),
            Listener::Ui | Listener::Tcp | Listener::Relay => {
                if listener == Listener::Ui && !ui_peer_ok(&app, req.extensions().get::<PeerUid>())
                {
                    return plain_error(
                        StatusCode::FORBIDDEN,
                        "forbidden",
                        "only glidex-ui may use this socket",
                    );
                }
                let peer = req.extensions().get::<ClientAddr>().map(|a| a.0);
                let ip = client_ip(peer, &headers, listener);
                if let Some(token) = bearer(&headers) {
                    // A directory's access token (D56), else a glidex token.
                    let jwt =
                        !token.starts_with(auth::TOKEN_PREFIX) && token.matches('.').count() == 2;
                    let found = if jwt {
                        Ok(op::access_principal(&app, &token, Transport::Tcp, ip))
                    } else {
                        app.auth.token_principal(&token, Transport::Tcp, ip)
                    };
                    match found {
                        Ok(Some(p)) => Ok(Some(p)),
                        Ok(None) => {
                            return plain_error(
                                StatusCode::UNAUTHORIZED,
                                "unauthenticated",
                                "invalid or expired token",
                            );
                        }
                        Err(e) => Err(e),
                    }
                } else if let Some(c) = cookie(&headers, auth::SESSION_COOKIE) {
                    match app.auth.session_principal(&c, Transport::Tcp, ip) {
                        Ok(Some(p)) => {
                            // Cookie-authenticated writes need a same-origin
                            // request carrying the CSRF value.
                            if !safe {
                                let csrf =
                                    headers.get(auth::CSRF_HEADER).and_then(|v| v.to_str().ok());
                                let ok = origin.is_some()
                                    && csrf.is_some_and(|c| {
                                        p.csrf.as_deref().is_some_and(|s| auth::constant_eq(c, s))
                                    });
                                if !ok {
                                    return plain_error(
                                        StatusCode::FORBIDDEN,
                                        "csrf_failed",
                                        "missing or wrong CSRF header or Origin",
                                    );
                                }
                            }
                            Ok(Some(p))
                        }
                        Ok(None) => Ok(None),
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(None)
                }
            }
        };
        match resolved {
            Ok(p) => p,
            // An agent records no users (D14): its local socket's logins are
            // refused by the cluster. Say where to log in instead.
            Err(e) if app.manager.node_link().is_some() => {
                tracing::warn!("a local login on an agent: {}", e);
                return plain_error(
                    StatusCode::FORBIDDEN,
                    "agent_no_local_api",
                    &agent_login_hint(&app.manager),
                );
            }
            Err(e) => {
                tracing::error!("authentication error: {}", e);
                return plain_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "authentication failed",
                );
            }
        }
    };
    if let Some(p) = principal {
        req.extensions_mut().insert(p);
    }
    let mut resp = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

/// From glidex-ui: an `Origin` equal to `https://` + `X-Forwarded-Host`.
/// The UI sets that header to the request's `Host`, which it has already
/// checked against its allowlist, so this is a same-origin check that
/// admits no name the UI wouldn't serve (spec/tailscale-tls.md T7).
fn ui_same_origin(origin: &str, headers: &HeaderMap) -> bool {
    let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    match (header("x-forwarded-proto"), header("x-forwarded-host")) {
        (Some("https"), Some(host)) if !host.is_empty() => origin
            .strip_prefix("https://")
            .is_some_and(|o| o.eq_ignore_ascii_case(host)),
        _ => false,
    }
}

fn ui_peer_ok(app: &App, peer: Option<&PeerUid>) -> bool {
    let Some(PeerUid(uid)) = peer else {
        return false;
    };
    nix::unistd::User::from_name(&app.auth.config.ui_user)
        .ok()
        .flatten()
        .is_some_and(|u| u.uid.as_raw() == *uid)
}

/// Per-route layer: audit writes, and denials of any request.
async fn audit_layer(State(app): State<AppState>, mut req: Request, next: Next) -> Response {
    let slot = AuditSlot::default();
    req.extensions_mut().insert(slot.clone());
    let action = req
        .extensions()
        .get::<RouteAction>()
        .map(|a| a.0)
        .unwrap_or("-");
    let principal = req.extensions().get::<Principal>().cloned();
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_default();
    let safe = is_safe(req.method());
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let resp = next.run(req).await;
    let status = resp.status();
    let failure = (!status.is_success()).then(|| {
        (
            format!("error:{}", status.as_u16()),
            status.as_str().to_string(),
        )
    });
    write_audit(
        &app,
        principal.as_ref(),
        &request_id,
        action,
        !safe,
        &slot,
        failure,
        ("http", format!("{} {}", method, path)),
    );
    resp
}

/// The audit entry of one request (spec/security.md §10), for REST and
/// RPCs alike: writes always, any request a policy denied, and reads a
/// handler asked to record. `failure` is (result, error code);
/// `transport` is the detail saying how the request came in.
#[allow(clippy::too_many_arguments)]
fn write_audit(
    app: &App,
    principal: Option<&Principal>,
    request_id: &str,
    action: &str,
    write: bool,
    slot: &AuditSlot,
    failure: Option<(String, String)>,
    transport: (&str, String),
) {
    let info = slot.0.lock().unwrap();
    // Public routes (logins) are audited by their handlers.
    if !(write || info.denied || info.always) || action == PUBLIC || app.auth.is_disabled() {
        return;
    }
    let (result, code) = match (info.denied, failure) {
        (true, f) => ("denied".to_string(), f.map(|f| f.1)),
        (false, Some((r, c))) => (r, Some(c)),
        (false, None) => ("ok".to_string(), None),
    };
    let mut details = info.details.clone();
    details.insert(transport.0.into(), serde_json::json!(transport.1));
    app.auth.audit(
        principal,
        request_id,
        action,
        info.project.as_deref(),
        info.target.as_deref(),
        &result,
        code.as_deref(),
        &info.policies,
        serde_json::Value::Object(details),
    );
}

/// The authenticated caller of a route.
pub struct Caller {
    pub p: Principal,
    pub action: &'static str,
    pub app: AppState,
    pub request_id: String,
    audit: AuditSlot,
    /// The actions a server authorized on this host (`PreAuthorized`).
    pre: Vec<String>,
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiErr;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let action = parts
            .extensions
            .get::<RouteAction>()
            .map(|a| a.0)
            .unwrap_or(AUTHENTICATED);
        Caller::from_extensions(&parts.extensions, app, action)
    }
}

impl Caller {
    /// The caller of `action`, from what the layers put on the request:
    /// for REST through the extractor above, for RPCs from the
    /// `RequestContext` (api::rpc).
    fn from_extensions(
        ext: &axum::http::Extensions,
        app: &AppState,
        action: &'static str,
    ) -> Result<Self, ApiErr> {
        let p = ext.get::<Principal>().cloned().ok_or_else(|| {
            err(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "authentication required",
            )
        })?;
        Ok(Caller {
            p,
            action,
            pre: ext
                .get::<PreAuthorized>()
                .map(|p| p.0.clone())
                .unwrap_or_default(),
            app: app.clone(),
            request_id: ext
                .get::<RequestId>()
                .map(|r| r.0.clone())
                .unwrap_or_default(),
            audit: ext.get::<AuditSlot>().cloned().unwrap_or_default(),
        })
    }
}

/// `401 reauth_required` for step-up, else `403`.
fn deny_response(action: &str, d: &Decision) -> ApiErr {
    if d.denied_by("base.step-up") {
        return err(
            StatusCode::UNAUTHORIZED,
            "reauth_required",
            "this action needs a recent login; log in again and retry",
        );
    }
    err(
        StatusCode::FORBIDDEN,
        "forbidden",
        format!("not allowed: {}", action),
    )
}

impl Caller {
    /// The same caller for another action (a stream that reads through an
    /// operation of its own): the same principal, request id and audit entry.
    pub fn with_action(&self, action: &'static str) -> Caller {
        Caller {
            p: self.p.clone(),
            action,
            app: self.app.clone(),
            request_id: self.request_id.clone(),
            audit: self.audit.clone(),
            pre: self.pre.clone(),
        }
    }

    pub fn manager(&self) -> &VmManager {
        &self.app.manager
    }

    pub fn auth(&self) -> &AuthService {
        &self.app.auth
    }

    /// The directory a request about directory records is about: the
    /// caller's own (spec/tenancy.md D58).
    pub fn directory_ent(&self) -> Ent {
        Ent::Directory(self.p.directory_id().to_string())
    }

    /// Ask Cedar, recording the decision for the audit entry.
    /// A server's decision for this host (`PreAuthorized`), when it covers `action` on `resource`.
    fn pre_authorized(&self, action: &str, resource: &Ent) -> Option<Decision> {
        (self.pre.iter().any(|a| a == action) && *resource == Ent::Host).then(|| Decision {
            allowed: true,
            policies: vec!["server-authorized".into()],
            errors: Vec::new(),
        })
    }

    pub fn decide(
        &self,
        action: &str,
        resource: Ent,
        es: EntitySet,
        extra: &[(&'static str, Ent)],
    ) -> Decision {
        let d = match self.pre_authorized(action, &resource) {
            Some(d) => d,
            None => self
                .app
                .auth
                .authorize(&self.p, action, resource, es, extra),
        };
        let mut a = self.audit.0.lock().unwrap();
        for p in &d.policies {
            if !a.policies.contains(p) {
                a.policies.push(p.clone());
            }
        }
        if !d.allowed {
            a.denied = true;
        }
        d
    }

    /// Whether a fresh login would allow what step-up now refuses: a
    /// browser session older than the step-up window. A token can't log
    /// in again, so it never would (spec/security.md §7.5).
    pub fn allowed_after_reauth(&self, action: &str, resource: Ent, es: EntitySet) -> bool {
        if !matches!(
            self.p.method,
            crate::auth::Method::Pam | crate::auth::Method::Oidc | crate::auth::Method::Passkey
        ) {
            return false;
        }
        let mut fresh = self.p.clone();
        fresh.authenticated_at = crate::auth::store::now();
        self.app
            .auth
            .authorize(&fresh, action, resource, es, &[])
            .allowed
    }

    pub fn allowed(&self, action: &str, resource: Ent, es: EntitySet) -> bool {
        if self.pre_authorized(action, &resource).is_some() {
            return true;
        }
        self.app
            .auth
            .authorize(&self.p, action, resource, es, &[])
            .allowed
    }

    /// Whether [`Self::require_action`] would allow it.
    pub fn allowed_action(
        &self,
        action: &str,
        resource: Ent,
        es: EntitySet,
        extra: &[(&'static str, Ent)],
    ) -> bool {
        self.decide(action, resource, es, extra).allowed
    }

    pub fn require_action(
        &self,
        action: &str,
        resource: Ent,
        es: EntitySet,
        extra: &[(&'static str, Ent)],
    ) -> Result<(), ApiErr> {
        let d = self.decide(action, resource, es, extra);
        if d.allowed {
            Ok(())
        } else {
            Err(deny_response(action, &d))
        }
    }

    /// The route's own action on `resource`.
    pub fn require(&self, resource: Ent, es: EntitySet) -> Result<(), ApiErr> {
        self.require_action(self.action, resource, es, &[])
    }

    /// A resource the caller can't `read_action` is reported as not found;
    /// nothing else is checked (the handler checks per field, e.g. PATCH).
    pub fn require_readable(
        &self,
        read_action: &str,
        resource: Ent,
        es: EntitySet,
        what: &str,
    ) -> Result<(), ApiErr> {
        if !self.allowed(read_action, resource, es) {
            self.audit.0.lock().unwrap().denied = true;
            return Err(err(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("{} not found", what),
            ));
        }
        Ok(())
    }

    /// D9: allocating on this cluster needs the project's tenant admitted
    /// here (`useCluster`, spec/tenancy.md §10.4); `403 not_admitted` otherwise.
    pub fn require_admitted(&self, project: &str) -> Result<(), ApiErr> {
        let d = self.decide(
            "useCluster",
            Ent::Cluster,
            project_entities(project),
            &[("project", Ent::Project(project.into()))],
        );
        if d.allowed {
            return Ok(());
        }
        Err(err(
            StatusCode::FORBIDDEN,
            "not_admitted",
            "this project's tenant isn't admitted on this cluster, or its admission is suspended",
        ))
    }

    /// Who made a request, for events (`api:<user>` or `api:token:<id>`).
    pub fn actor(&self) -> String {
        match (&self.p.user, &self.p.token) {
            (Some(u), _) => format!("api:{}", u.id),
            (None, Some(t)) => format!("api:token:{}", t.id),
            _ => "api".to_string(),
        }
    }

    /// Like `require`, but a resource the caller can't even `read` is
    /// reported as not found (spec §7.4).
    pub fn require_visible(
        &self,
        read_action: &str,
        resource: Ent,
        es: EntitySet,
        what: &str,
    ) -> Result<(), ApiErr> {
        if read_action != self.action && !self.allowed(read_action, resource.clone(), es.clone()) {
            self.audit.0.lock().unwrap().denied = true;
            return Err(err(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("{} not found", what),
            ));
        }
        let d = self.decide(self.action, resource, es, &[]);
        if d.allowed {
            Ok(())
        } else if read_action == self.action {
            Err(err(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("{} not found", what),
            ))
        } else {
            Err(deny_response(self.action, &d))
        }
    }

    pub fn set_project(&self, project: &str) {
        self.audit.0.lock().unwrap().project = Some(project.to_string());
    }

    pub fn set_target(&self, target: impl Into<String>) {
        self.audit.0.lock().unwrap().target = Some(target.into());
    }

    /// Write an audit entry for this request even if it is a read.
    pub fn audit_always(&self) {
        self.audit.0.lock().unwrap().always = true;
    }

    pub fn detail(&self, key: &str, value: serde_json::Value) {
        self.audit
            .0
            .lock()
            .unwrap()
            .details
            .insert(key.into(), value);
    }

    /// The project a new resource goes into: the requested one (id or
    /// name), else the caller's default project, else the only project
    /// the caller has a link in.
    pub fn target_project(&self, requested: Option<&str>) -> Result<String, ApiErr> {
        let projects = self.manager().projects();
        let resolve = |k: &str| {
            projects.resolve(k).map(|p| p.id).map_err(|_| {
                err(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("project {} not found", k),
                )
            })
        };
        if let Some(r) = requested {
            return resolve(r);
        }
        if let Some(d) = self.p.default_project() {
            return resolve(d);
        }
        if self.p.method == Method::Disabled || self.p.is_break_glass() {
            return Ok(self.manager().default_project_id());
        }
        match self.app.auth.linked_projects(&self.p).map_err(auth_error)? {
            auth::LinkedProjects::Some(v) if v.len() == 1 => Ok(v[0].clone()),
            _ => Err(err(
                StatusCode::BAD_REQUEST,
                "project_required",
                "name a project (\"project\" field or ?project=); you have no default project",
            )),
        }
    }

    /// Whether the caller may go over `project`'s quotas.
    pub fn quota_mode(&self, project: &str) -> QuotaMode {
        let mut es = EntitySet::new();
        es.project(project);
        if self.allowed("exceedQuota", Ent::Project(project.into()), es) {
            QuotaMode::MayExceed
        } else {
            QuotaMode::Enforce
        }
    }

    /// Record quota limits a request went over (spec §6.3).
    pub fn note_overruns(&self, over: &[crate::tenancy::QuotaOverrun]) {
        if !over.is_empty() {
            self.detail("quota_exceeded", serde_json::json!(over));
            let d = self.app.auth.authorize(
                &self.p,
                "exceedQuota",
                Ent::Cluster,
                EntitySet::new(),
                &[],
            );
            let mut a = self.audit.0.lock().unwrap();
            a.policies.extend(d.policies);
        }
    }

    /// Projects whose resources the caller may list.
    pub fn visible_projects(&self) -> Result<auth::LinkedProjects, ApiErr> {
        self.app.auth.linked_projects(&self.p).map_err(auth_error)
    }
}

pub(crate) fn auth_error(e: AuthError) -> ApiErr {
    match e {
        AuthError::Unauthenticated => {
            err(StatusCode::UNAUTHORIZED, "unauthenticated", e.to_string())
        }
        AuthError::Denied => err(StatusCode::UNAUTHORIZED, "login_failed", e.to_string()),
        AuthError::RateLimited => err(StatusCode::TOO_MANY_REQUESTS, "rate_limited", e.to_string()),
        AuthError::Unavailable(_) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            e.to_string(),
        ),
        AuthError::Invalid(_) => err(StatusCode::BAD_REQUEST, "invalid", e.to_string()),
        AuthError::NotAllowed(_) => err(StatusCode::FORBIDDEN, "login_not_allowed", e.to_string()),
        AuthError::Store(crate::auth::store::StoreError::NotFound(_)) => {
            err(StatusCode::NOT_FOUND, "not_found", e.to_string())
        }
        AuthError::Store(crate::auth::store::StoreError::AlreadyExists(_))
        | AuthError::Store(crate::auth::store::StoreError::Conflict(_)) => {
            err(StatusCode::CONFLICT, "conflict", e.to_string())
        }
        AuthError::Store(crate::auth::store::StoreError::Invalid(_)) => {
            err(StatusCode::BAD_REQUEST, "invalid", e.to_string())
        }
        AuthError::Store(_) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence_error",
            e.to_string(),
        ),
        AuthError::Authz(crate::authz::AuthzError::Validation(v)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(
                ApiError::new("invalid_policy", "policy validation failed")
                    .with_details(serde_json::json!({ "errors": v })),
            ),
        ),
        AuthError::Authz(e) => err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_policy",
            e.to_string(),
        ),
    }
}

// ---- entities for Cedar ---------------------------------------------------

pub(crate) fn project_entities(project: &str) -> EntitySet {
    let mut es = EntitySet::new();
    es.project(project);
    es
}

pub(crate) fn vm_entities(vm: &crate::models::Vm) -> (Ent, EntitySet) {
    let mut es = EntitySet::new();
    let e = Ent::Vm(vm.id.clone());
    es.in_project(e.clone(), &vm.project);
    (e, es)
}

pub(crate) fn disk_entities(id: &str, project: &str) -> (Ent, EntitySet) {
    let mut es = EntitySet::new();
    let e = Ent::Disk(id.to_string());
    es.in_project(e.clone(), project);
    (e, es)
}

pub(crate) fn credential_ent(project: &str, username: &str) -> Ent {
    Ent::Credential(format!("{}/{}", project, username))
}

pub(crate) fn credential_entities(project: &str, username: &str) -> (Ent, EntitySet) {
    let mut es = EntitySet::new();
    let e = credential_ent(project, username);
    es.in_project(e.clone(), project);
    (e, es)
}

pub(crate) fn add_network(es: &mut EntitySet, n: &crate::network::Network) -> Ent {
    es.network(
        &n.id,
        n.project.as_deref(),
        n.all_projects,
        &n.grants,
        &n.shares,
        &n.tenant_grants,
    );
    Ent::Network(n.id.clone())
}

/// Where `gxctl` on an agent logs in: a server's API (spec/gxctl-auth.md).
fn agent_login_hint(m: &crate::state::VmManager) -> String {
    let servers: Vec<String> = m
        .nodes()
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone())
        .filter_map(|n| {
            n.status
                .advertise
                .map(|a| format!("https://{}:8841", a.ip()))
        })
        .collect();
    let url = servers
        .first()
        .cloned()
        .unwrap_or_else(|| "https://<server>:8841".into());
    format!(
        "this host is a cluster agent: it has no local API, and the cluster can't vouch for a Unix user of another host. Log in to a server: `gxctl auth login --url {url}` (servers: {})",
        if servers.is_empty() {
            "unknown".into()
        } else {
            servers.join(", ")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_maps_to_a_schema_action() {
        let dir = tempfile::TempDir::new().unwrap();
        let manager = VmManager::with_db_path(dir.path().join("t.db")).unwrap();
        let auth = AuthService::disabled(manager.database()).unwrap();
        let app = Arc::new(App { manager, auth });
        let schema = app.auth.engine.schema();
        let actions: Vec<String> = schema
            .actions()
            .map(|a| a.id().unescaped().to_string())
            .collect();
        let table = route_table(&app);
        assert!(table.len() > 80);
        for r in &table {
            if r.action == PUBLIC || r.action == AUTHENTICATED {
                continue;
            }
            assert!(
                actions.iter().any(|a| a == r.action),
                "{} {} → unknown action {}",
                r.method,
                r.path,
                r.action
            );
            // Groups and roles are never requested directly.
            assert!(
                !r.action.contains('.') && !r.action.starts_with("role"),
                "{} {}",
                r.method,
                r.path
            );
        }
        let public: Vec<_> = table
            .iter()
            .filter(|r| r.action == PUBLIC)
            .map(|r| r.path)
            .collect();
        assert_eq!(
            public,
            [
                "/health",
                "/auth/methods",
                "/auth/server-info",
                "/auth/login",
                "/auth/token",
                "/auth/oidc/start",
                "/auth/oidc/callback",
                "/auth/oidc/device",
                "/auth/oidc/device/poll",
                "/auth/discover",
                "/auth/exchange",
                "/d/{dir}/.well-known/openid-configuration",
                "/d/{dir}/jwks.json",
                "/d/{dir}/authorize",
                "/d/{dir}/device_authorization",
                "/d/{dir}/token",
                "/d/{dir}/revoke",
                "/trust/v1/redeem",
                "/trust/v1/end",
                "/d/{dir}/trust/v1/end",
                "/d/{dir}/feed/v1",
                "/auth/passkeys/login/start",
                "/auth/passkeys/login/finish",
                "/auth/c/{id}/start",
                "/auth/c/{id}/start",
                "/auth/invitation",
                "/auth/invitation/passkey/start",
                "/auth/invitation/passkey/finish",
                "/auth/c/{id}/callback",
                "/auth/t/{trusted}/start",
                "/auth/t/{trusted}/callback",
                "/auth/c/{id}/device",
                "/auth/c/{id}/device/poll",
                "/scim/v2/c/{id}/ServiceProviderConfig",
                "/scim/v2/c/{id}/Schemas",
                "/scim/v2/c/{id}/ResourceTypes",
                "/scim/v2/c/{id}/Users",
                "/scim/v2/c/{id}/Users",
                "/scim/v2/c/{id}/Users/{uid}",
                "/scim/v2/c/{id}/Users/{uid}",
                "/scim/v2/c/{id}/Users/{uid}",
                "/scim/v2/c/{id}/Users/{uid}",
                "/scim/v2/c/{id}/Groups",
                "/scim/v2/c/{id}/Groups",
                "/scim/v2/c/{id}/Groups/{gid}",
                "/scim/v2/c/{id}/Groups/{gid}",
                "/scim/v2/c/{id}/Groups/{gid}",
                "/scim/v2/c/{id}/Groups/{gid}",
            ]
        );

        // The RPC operation table (spec/grpc.md §5): the same rules, and
        // each REST twin is registered with the same action.
        assert!(!glidex_api::OPERATIONS.is_empty());
        for op in glidex_api::OPERATIONS {
            if op.action != PUBLIC && op.action != AUTHENTICATED {
                assert!(
                    actions.iter().any(|a| a == op.action),
                    "{} → unknown action {}",
                    op.procedure,
                    op.action
                );
                assert!(
                    !op.action.contains('.') && !op.action.starts_with("role"),
                    "{}",
                    op.procedure
                );
            }
            if let Some((method, path)) = op.rest {
                let twin = table.iter().find(|r| r.method == method && r.path == path);
                let twin = twin.unwrap_or_else(|| {
                    panic!("{}: REST twin {method} {path} isn't a route", op.procedure)
                });
                assert_eq!(
                    twin.action, op.action,
                    "{}: REST twin has a different action",
                    op.procedure
                );
            }
        }
    }

    #[test]
    fn cookie_and_bearer_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; gx_session=abc; b=2".parse().unwrap());
        assert_eq!(cookie(&h, "gx_session").as_deref(), Some("abc"));
        assert_eq!(cookie(&h, "nope"), None);
        h.insert(header::AUTHORIZATION, "Bearer gxt_x".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("gxt_x"));
    }
}

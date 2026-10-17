//! Who may do what (spec/security.md §5–§7, §10, §13): `whoami`,
//! `project`, `binding`, `system-binding`, `user`, `team`, `token`,
//! `policy` and `audit` on the generated `glidex.access.v1` clients (and
//! `glidex.system.v1.SystemService.WhoAmI`), over the Connect protocol
//! (spec/grpc.md §9.1). Logging in (`/auth/token`, the OIDC device
//! routes) stays REST, in `admin.rs`.

use crate::admin::positional;
use crate::client::{ApiClient, RpcTransport, format_entity, parse_role_ref, role_name};
use crate::tenancy::{err, principal_ref, rpc_err, rpc_reason, transport, usage};
use crate::vm::enum_word;
use crate::{flag_value, flag_values, format_unix_time, has_flag, prompt};
use buffa_types::google::protobuf::{FieldMask, Timestamp};
use colored::Colorize;
use connectrpc::ErrorCode;
use glidex_api::proto::glidex::access::v1::*;
use glidex_api::proto::glidex::system::v1::{SystemServiceClient, WhoAmIRequest, WhoAmIResponse};
use glidex_api::proto::glidex::tenancy::v1::Quotas;
use std::collections::HashMap;

type PolicyOneof =
    glidex_api::proto::glidex::access::v1::__buffa::oneof::get_policy_response::Policy;

/// A service client on a fresh transport; on failure the error is shown
/// and the command returns.
macro_rules! svc {
    ($client:expr, $ty:ident) => {
        match transport($client).await {
            Some(t) => $ty::new(t.clone(), t.config()),
            None => return,
        }
    };
}

async fn rpc<T>(client: &ApiClient, make: impl FnOnce(RpcTransport) -> T) -> Result<T, String> {
    client
        .rpc_transport()
        .await
        .map(make)
        .map_err(|e| e.message)
}

fn ts(t: Option<&Timestamp>) -> String {
    t.map(|t| format_unix_time(t.seconds.max(0) as u64))
        .unwrap_or_else(|| "-".into())
}

fn entity(e: Option<&glidex_api::proto::glidex::api::v1::EntityRef>) -> String {
    let e = e.cloned().unwrap_or_default();
    format_entity(&e.r#type, e.id.as_deref())
}

// ---- calls --------------------------------------------------------------------

/// `GET /auth/whoami`, as `glidex.system.v1.SystemService.WhoAmI`.
pub(crate) async fn who_am_i(client: &ApiClient) -> Result<WhoAmIResponse, String> {
    rpc(client, |t| SystemServiceClient::new(t.clone(), t.config()))
        .await?
        .who_am_i(WhoAmIRequest::default())
        .await
        .map(|r| r.into_owned())
        .map_err(rpc_err)
}

/// The signed-in user's display name, when the caller is a user.
pub(crate) fn user_name(w: &WhoAmIResponse) -> Option<&str> {
    w.user.as_option().map(|u| u.display_name.as_str())
}

/// Revoke API token `id`.
pub(crate) async fn revoke_token(client: &ApiClient, id: &str) -> Result<(), String> {
    rpc(client, |t| TokenServiceClient::new(t.clone(), t.config()))
        .await?
        .revoke_token(RevokeTokenRequest {
            id: id.to_string(),
            ..Default::default()
        })
        .await
        .map(|_| ())
        .map_err(rpc_err)
}

/// The projects the caller may read.
pub(crate) async fn list_projects(client: &ApiClient) -> Result<Vec<Project>, String> {
    rpc(client, |t| ProjectServiceClient::new(t.clone(), t.config()))
        .await?
        .list_projects(ListProjectsRequest::default())
        .await
        .map(|r| r.into_owned().projects)
        .map_err(rpc_err)
}

/// Project names by id (best effort: an empty map when not allowed).
pub(crate) async fn project_names(client: &ApiClient) -> HashMap<String, String> {
    list_projects(client)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect()
}

// ---- whoami -------------------------------------------------------------------

pub async fn whoami(client: &ApiClient) {
    let w = match who_am_i(client).await {
        Ok(w) => w,
        Err(e) => return err(e),
    };
    match w.user.as_option() {
        None => println!("  User:     {}", "(service account)".dimmed()),
        Some(u) => println!("  User:     {} ({})", u.display_name.yellow(), u.id),
    }
    println!("  Method:   {} via {}", w.method, client.describe());
    if let Some(t) = w.token.as_option() {
        println!("  Token:    {} ({})", t.name, t.id);
    }
    if w.break_glass == Some(true) {
        println!(
            "  {}",
            "Break-glass administrator (base policies only)".red()
        );
    }
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_string()
        } else {
            v.join(", ")
        }
    };
    println!("  Teams:    {}", list(&w.teams));
    println!("  Host roles: {}", list(&w.host_roles));
    let names = project_names(client).await;
    match &w.default_project {
        Some(p) => println!(
            "  Default project: {}",
            names
                .get(p)
                .map(|n| format!("{} ({})", n, p))
                .unwrap_or_else(|| p.to_string())
        ),
        None => println!("  Default project: -"),
    }
    if let Some(p) = client.project() {
        println!("  This session: --project {}", p.cyan());
    }
    if !w.project_roles.is_empty() {
        println!("  Project roles:");
        for r in &w.project_roles {
            let via = r.via.as_option().cloned().unwrap_or_default();
            println!(
                "    {:<20} {:<14} via {}",
                r.project_name.as_deref().unwrap_or(&r.project).cyan(),
                r.role,
                format_entity(&via.r#type, via.id.as_deref())
            );
        }
    }
}

// ---- tokens -------------------------------------------------------------------

pub async fn token(client: &ApiClient, args: &[&str]) {
    let u = "Usage: token list | create <name> [--days N] [--service-account [--project P]] [--role ROLE[@PROJECT]]... | revoke <id>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => {
            let svc = svc!(client, TokenServiceClient);
            match svc
                .list_tokens(ListTokensRequest::default())
                .await
                .map(|r| r.into_owned().tokens)
            {
                Ok(list) if list.is_empty() => println!("No tokens."),
                Ok(tokens) => {
                    let names = project_names(client).await;
                    for t in tokens {
                        let kind =
                            if t.kind.as_known() == Some(TokenKind::TOKEN_KIND_SERVICE_ACCOUNT) {
                                let p = t.project.as_deref().unwrap_or("");
                                format!(
                                    "service account in {}",
                                    names.get(p).map(String::as_str).unwrap_or(p)
                                )
                            } else {
                                "personal".to_string()
                            };
                        let roles: Vec<String> = t
                            .roles
                            .iter()
                            .map(|l| format!("{}@{}", l.role, entity(l.resource.as_option())))
                            .collect();
                        println!(
                            "  {:<24} {:<18} {}  expires {}  last used {}{}",
                            t.id.cyan(),
                            t.name,
                            kind,
                            ts(t.expires_at.as_option()),
                            ts(t.last_used_at.as_option()),
                            if roles.is_empty() {
                                String::new()
                            } else {
                                format!("  roles: {}", roles.join(", "))
                            }
                        );
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "create" => {
            let pos = positional(&args[1..], &["--days", "--project", "--role"]);
            let Some(name) = pos.first() else {
                return usage(u);
            };
            let mut req = CreateTokenRequest {
                name: name.to_string(),
                ..Default::default()
            };
            if let Some(d) = flag_value(args, "--days") {
                match d.parse::<u32>() {
                    Ok(d) => req.expires_in_days = Some(d),
                    Err(_) => return err("--days must be a number"),
                }
            }
            if has_flag(args, "--service-account") {
                req.kind = TokenKind::TOKEN_KIND_SERVICE_ACCOUNT.into();
                match flag_value(args, "--project")
                    .map(str::to_string)
                    .or_else(|| client.project())
                {
                    Some(p) => req.project = Some(p),
                    None => println!(
                        "{} no --project; the control plane uses your default project.",
                        "Note:".dimmed()
                    ),
                }
            }
            for r in flag_values(args, "--role") {
                match parse_role_ref(r) {
                    Ok((role, project)) => req.roles.push(RoleRef {
                        role,
                        project,
                        ..Default::default()
                    }),
                    Err(e) => return err(e),
                }
            }
            let svc = svc!(client, TokenServiceClient);
            match svc.create_token(req).await.map(|r| r.into_owned()) {
                Ok(r) => {
                    let rec = r.record.as_option().cloned().unwrap_or_default();
                    println!(
                        "{} token {} ({}), expires {}",
                        "Created:".green(),
                        rec.name.yellow(),
                        rec.id,
                        ts(rec.expires_at.as_option())
                    );
                    println!();
                    println!("    {}", r.token.bold());
                    println!();
                    println!(
                        "{} This is the only time the token is shown. Store it in a secret manager or a 0600 file; \
                         anyone holding it acts with its rights. Use it with GLIDEX_TOKEN or 'gxctl --url … login --token'.",
                        "Warning:".yellow()
                    );
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "revoke" | "rm" | "delete" => match args.get(1) {
            Some(id) => match revoke_token(client, id).await {
                Ok(()) => println!("{} token {}", "Revoked:".green(), id),
                Err(e) => err(e),
            },
            None => usage(u),
        },
        _ => usage(u),
    }
}

// ---- projects and bindings ----------------------------------------------------

const QUOTA_KEYS: [&str; 6] = [
    "vms",
    "vcpus",
    "memory_mib",
    "disk_gib",
    "running_vms",
    "networks",
];

fn quota_slot<'a>(q: &'a mut Quotas, k: &str) -> Option<&'a mut Option<u32>> {
    Some(match k {
        "vms" => &mut q.vms,
        "vcpus" => &mut q.vcpus,
        "memory_mib" => &mut q.memory_mib,
        "disk_gib" => &mut q.disk_gib,
        "running_vms" => &mut q.running_vms,
        "networks" => &mut q.networks,
        _ => return None,
    })
}

fn quota_of(q: &Quotas, k: &str) -> Option<u32> {
    quota_slot(&mut q.clone(), k).and_then(|v| *v)
}

fn usage_of(u: &ProjectUsage, k: &str) -> u32 {
    match k {
        "vms" => u.vms,
        "vcpus" => u.vcpus,
        "memory_mib" => u.memory_mib,
        "disk_gib" => u.disk_gib,
        "running_vms" => u.running_vms,
        "networks" => u.networks,
        _ => 0,
    }
}

/// `key=value` quota edits applied to `quotas`; `none`/`unlimited` → no limit.
pub fn apply_quota_edits(quotas: &mut Quotas, edits: &[&str]) -> Result<(), String> {
    for e in edits {
        let (k, v) = e
            .split_once('=')
            .ok_or_else(|| format!("'{}': write quotas as key=value", e))?;
        let Some(slot) = quota_slot(quotas, k) else {
            return Err(format!(
                "unknown quota '{}'; one of {}",
                k,
                QUOTA_KEYS.join(", ")
            ));
        };
        *slot = match v {
            "none" | "null" | "unlimited" | "-" => None,
            n => Some(
                n.parse::<u32>()
                    .map_err(|_| format!("'{}': a quota is a number or 'none'", e))?,
            ),
        };
    }
    Ok(())
}

fn quota_line(q: Option<&Quotas>, usage: Option<&ProjectUsage>) -> String {
    let q = q.cloned().unwrap_or_default();
    QUOTA_KEYS
        .iter()
        .map(|k| {
            let limit = quota_of(&q, k)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "∞".into());
            match usage {
                Some(u) => format!("{} {}/{}", k, usage_of(u, k), limit),
                None => format!("{} {}", k, limit),
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

async fn get_project(
    svc: &ProjectServiceClient<RpcTransport>,
    id: &str,
) -> Result<Project, String> {
    svc.get_project(GetProjectRequest {
        id: id.to_string(),
        ..Default::default()
    })
    .await
    .map(|r| {
        r.into_owned()
            .project
            .as_option()
            .cloned()
            .unwrap_or_default()
    })
    .map_err(rpc_err)
}

pub async fn project(client: &ApiClient, args: &[&str]) {
    let u = "Usage: project list | show <p> | create <name> [--description TEXT] | delete <p> | quota <p> key=value... | use <p>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match list_projects(client).await {
            Ok(ps) if ps.is_empty() => println!("No projects visible to you."),
            Ok(ps) => {
                for p in ps {
                    println!(
                        "  {:<20} {}  vms {}/{}  {}",
                        p.name.cyan(),
                        p.id.dimmed(),
                        p.usage.as_option().map(|u| u.vms).unwrap_or(0),
                        p.quotas
                            .as_option()
                            .and_then(|q| q.vms)
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "∞".into()),
                        p.description
                    );
                }
            }
            Err(e) => err(e),
        },
        "show" | "get" => match args.get(1) {
            Some(p) => {
                let svc = svc!(client, ProjectServiceClient);
                match get_project(&svc, p).await {
                    Ok(p) => {
                        println!("  Name:        {}", p.name.yellow());
                        println!("  Id:          {}", p.id);
                        println!("  Description: {}", p.description);
                        println!("  Created:     {}", ts(p.created_at.as_option()));
                        println!(
                            "  Usage/quota: {}",
                            quota_line(p.quotas.as_option(), p.usage.as_option())
                        );
                    }
                    Err(e) => err(e),
                }
            }
            None => usage(u),
        },
        "create" => {
            let pos = positional(&args[1..], &["--description"]);
            let Some(name) = pos.first() else {
                return usage(u);
            };
            let svc = svc!(client, ProjectServiceClient);
            match svc
                .create_project(CreateProjectRequest {
                    name: name.to_string(),
                    description: flag_value(args, "--description").unwrap_or("").to_string(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    let p = r.project.as_option().cloned().unwrap_or_default();
                    println!(
                        "{} {} ({})",
                        "Project created:".green(),
                        p.name.yellow(),
                        p.id
                    )
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "delete" | "rm" => match args.get(1) {
            Some(p) => {
                if prompt(&format!("Delete project {} (it must be empty; its service-account tokens are revoked)? [y/N]: ", p)).to_lowercase() != "y" {
                    return println!("Cancelled");
                }
                let svc = svc!(client, ProjectServiceClient);
                match svc
                    .delete_project(DeleteProjectRequest {
                        id: p.to_string(),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(_) => println!("{} {}", "Project deleted:".green(), p),
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => usage(u),
        },
        "quota" | "quotas" => {
            let Some(p) = args.get(1) else {
                return usage(u);
            };
            let svc = svc!(client, ProjectServiceClient);
            let current = match get_project(&svc, p).await {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            if args.len() < 3 {
                return println!(
                    "  {}",
                    quota_line(current.quotas.as_option(), current.usage.as_option())
                );
            }
            // The update replaces the whole set, so start from the current one.
            let mut q = current.quotas.as_option().cloned().unwrap_or_default();
            if let Err(e) = apply_quota_edits(&mut q, &args[2..]) {
                return err(e);
            }
            match svc
                .update_project(UpdateProjectRequest {
                    id: p.to_string(),
                    quotas: q.into(),
                    update_mask: FieldMask {
                        paths: vec!["quotas".into()],
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    let p = r.project.as_option().cloned().unwrap_or_default();
                    println!(
                        "{} {}: {}",
                        "Quotas updated:".green(),
                        p.name,
                        quota_line(p.quotas.as_option(), current.usage.as_option())
                    )
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "use" => match args.get(1) {
            Some(p) => {
                let svc = svc!(client, UserServiceClient);
                let default_project = if *p == "-" || *p == "none" { "" } else { *p };
                match svc
                    .update_me(UpdateMeRequest {
                        default_project: default_project.to_string(),
                        ..Default::default()
                    })
                    .await
                    .map(|r| r.into_owned())
                {
                    Ok(r) => {
                        client.set_project(None);
                        match r
                            .user
                            .as_option()
                            .and_then(|u| u.default_project.as_deref())
                        {
                            Some(id) => println!(
                                "{} default project is now {} ({})",
                                "OK:".green(),
                                p.yellow(),
                                id
                            ),
                            None => println!("{} no default project", "OK:".green()),
                        }
                    }
                    Err(e)
                        if e.code == ErrorCode::InvalidArgument
                            && rpc_reason(&e).as_deref() == Some("invalid") =>
                    {
                        // Service accounts and auth-disabled servers have no profile.
                        client.set_project(Some(p.to_string()));
                        println!(
                            "{} {}; using project {} for this session only",
                            "Note:".yellow(),
                            rpc_err(e),
                            p
                        );
                    }
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => match client.project() {
                Some(p) => println!("This session uses --project {}", p),
                None => usage(u),
            },
        },
        _ => usage(u),
    }
}

fn print_links(links: &[Binding]) {
    if links.is_empty() {
        return println!("No bindings.");
    }
    for l in links {
        println!(
            "  {:<24} {:<18} {:<46} by {} {}",
            l.id.cyan(),
            l.role,
            entity(l.principal.as_option()),
            l.created_by,
            ts(l.created_at.as_option()).dimmed()
        );
    }
}

fn print_added(l: Option<&Binding>) {
    let l = l.cloned().unwrap_or_default();
    println!(
        "{} {} {} → {}",
        "Added:".green(),
        l.id,
        l.role,
        entity(l.principal.as_option())
    );
}

pub async fn binding(client: &ApiClient, args: &[&str]) {
    let u = "Usage: binding list <project> | add <project> <role> user:<id>|team:<id>|token:<id> | remove <project> <binding-id>";
    match (args.first().copied().unwrap_or("list"), args.get(1)) {
        ("list" | "ls", Some(p)) => {
            let svc = svc!(client, ProjectServiceClient);
            match svc
                .list_project_bindings(ListProjectBindingsRequest {
                    id: p.to_string(),
                    ..Default::default()
                })
                .await
            {
                Ok(r) => print_links(&r.into_owned().bindings),
                Err(e) => err(rpc_err(e)),
            }
        }
        ("add", Some(p)) => {
            let (Some(role), Some(who)) = (args.get(2), args.get(3)) else {
                return usage(u);
            };
            let principal = match principal_ref(who) {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            let svc = svc!(client, ProjectServiceClient);
            match svc
                .add_project_binding(AddProjectBindingRequest {
                    id: p.to_string(),
                    role: role_name(role),
                    principal: principal.into(),
                    ..Default::default()
                })
                .await
            {
                Ok(r) => print_added(r.into_owned().binding.as_option()),
                Err(e) => err(rpc_err(e)),
            }
        }
        ("remove" | "rm" | "delete", Some(p)) => match args.get(2) {
            Some(link) => {
                let svc = svc!(client, ProjectServiceClient);
                match svc
                    .remove_project_binding(RemoveProjectBindingRequest {
                        id: p.to_string(),
                        link: link.to_string(),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(_) => println!("{} {}", "Removed:".green(), link),
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => usage(u),
        },
        _ => usage(u),
    }
}

pub async fn system_binding(client: &ApiClient, args: &[&str]) {
    let u = "Usage: system-binding list | add <role> user:<id>|team:<id>|token:<id> | remove <binding-id>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => {
            let svc = svc!(client, SystemBindingServiceClient);
            match svc
                .list_system_bindings(ListSystemBindingsRequest::default())
                .await
            {
                Ok(r) => print_links(&r.into_owned().bindings),
                Err(e) => err(rpc_err(e)),
            }
        }
        "add" => {
            let (Some(role), Some(who)) = (args.get(1), args.get(2)) else {
                return usage(u);
            };
            let principal = match principal_ref(who) {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            let svc = svc!(client, SystemBindingServiceClient);
            match svc
                .add_system_binding(AddSystemBindingRequest {
                    role: role_name(role),
                    principal: principal.into(),
                    ..Default::default()
                })
                .await
            {
                Ok(r) => print_added(r.into_owned().binding.as_option()),
                Err(e) => err(rpc_err(e)),
            }
        }
        "remove" | "rm" | "delete" => match args.get(1) {
            Some(link) => {
                let svc = svc!(client, SystemBindingServiceClient);
                match svc
                    .remove_system_binding(RemoveSystemBindingRequest {
                        link: link.to_string(),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(_) => println!("{} {}", "Removed:".green(), link),
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => usage(u),
        },
        _ => usage(u),
    }
}

// ---- users and teams ----------------------------------------------------------

pub async fn user(client: &ApiClient, args: &[&str]) {
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => {
            let svc = svc!(client, UserServiceClient);
            match svc.list_users(ListUsersRequest::default()).await {
                Ok(r) => {
                    for u in r.into_owned().users {
                        let ids: Vec<String> = u
                            .identities
                            .iter()
                            .map(|i| format!("{}:{}", i.provider, i.subject))
                            .collect();
                        println!(
                            "  {:<38} {:<20} {}{}",
                            u.id.cyan(),
                            u.display_name,
                            ids.join(", "),
                            if u.disabled == Some(true) {
                                " (disabled)".red().to_string()
                            } else {
                                String::new()
                            }
                        );
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        _ => usage("Usage: user list"),
    }
}

/// A team id from an id or a name.
pub(crate) async fn resolve_team(
    svc: &TeamServiceClient<RpcTransport>,
    key: &str,
) -> Result<String, String> {
    let teams = svc
        .list_teams(ListTeamsRequest::default())
        .await
        .map(|r| r.into_owned().teams)
        .map_err(rpc_err)?;
    teams
        .iter()
        .find(|t| t.id == key)
        .or_else(|| teams.iter().find(|t| t.name == key))
        .map(|t| t.id.clone())
        .ok_or_else(|| format!("team '{}' not found", key))
}

pub async fn team(client: &ApiClient, args: &[&str]) {
    let u = "Usage: team list | create <name> | delete <team> | add-member <team> <user-id> | remove-member <team> <user-id>";
    let sub = args.first().copied().unwrap_or("list");
    if !matches!(
        sub,
        "list" | "ls" | "create" | "delete" | "rm" | "add-member" | "remove-member"
    ) {
        return usage(u);
    }
    let svc = svc!(client, TeamServiceClient);
    match sub {
        "list" | "ls" => match svc
            .list_teams(ListTeamsRequest::default())
            .await
            .map(|r| r.into_owned().teams)
        {
            Ok(ts) if ts.is_empty() => println!("No teams."),
            Ok(ts) => {
                for t in ts {
                    let members: Vec<String> = t
                        .members
                        .iter()
                        .map(|m| format!("{}({})", m.user_id, m.source))
                        .collect();
                    println!(
                        "  {:<20} {}  members: {}",
                        t.name.cyan(),
                        t.id.dimmed(),
                        if members.is_empty() {
                            "-".into()
                        } else {
                            members.join(", ")
                        }
                    );
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        "create" => match args.get(1) {
            Some(name) => match svc
                .create_team(CreateTeamRequest {
                    name: name.to_string(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    let t = r.team.as_option().cloned().unwrap_or_default();
                    println!("{} {} ({})", "Team created:".green(), t.name.yellow(), t.id)
                }
                Err(e) => err(rpc_err(e)),
            },
            None => usage(u),
        },
        "delete" | "rm" => match args.get(1) {
            Some(t) => {
                let id = match resolve_team(&svc, t).await {
                    Ok(id) => id,
                    Err(e) => return err(e),
                };
                match svc
                    .delete_team(DeleteTeamRequest {
                        id,
                        ..Default::default()
                    })
                    .await
                {
                    Ok(_) => println!("{} {}", "Team deleted:".green(), t),
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => usage(u),
        },
        sub => {
            let (Some(t), Some(user)) = (args.get(1), args.get(2)) else {
                return usage(u);
            };
            let id = match resolve_team(&svc, t).await {
                Ok(id) => id,
                Err(e) => return err(e),
            };
            let user = user.to_string();
            let r = if sub == "add-member" {
                svc.add_team_member(AddTeamMemberRequest {
                    id,
                    user,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned().team)
            } else {
                svc.remove_team_member(RemoveTeamMemberRequest {
                    id,
                    user,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned().team)
            };
            match r {
                Ok(t) => {
                    let t = t.as_option().cloned().unwrap_or_default();
                    println!(
                        "{} {} now has {} member(s)",
                        "OK:".green(),
                        t.name,
                        t.members.len()
                    )
                }
                Err(e) => err(rpc_err(e)),
            }
        }
    }
}

// ---- site policies ------------------------------------------------------------

fn source_word(s: &buffa::EnumValue<PolicySource>) -> String {
    enum_word(s, "POLICY_SOURCE_")
}

/// The current version of site policy `id`: 0 when it doesn't exist.
async fn policy_version(
    svc: &PolicyServiceClient<RpcTransport>,
    id: &str,
) -> Result<(u64, Option<SitePolicy>), String> {
    match svc
        .get_policy(GetPolicyRequest {
            id: id.to_string(),
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned())
    {
        Ok(GetPolicyResponse {
            policy: Some(PolicyOneof::Site(p)),
            ..
        }) => Ok((p.version, Some(*p))),
        Ok(v) => Err(format!(
            "{} is a {} policy and can't be changed through the API",
            id,
            source_word(&v.source)
        )),
        Err(e) if e.code == ErrorCode::NotFound => Ok((0, None)),
        Err(e) => Err(rpc_err(e)),
    }
}

fn read_policy_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path, e))
}

pub async fn policy(client: &ApiClient, args: &[&str]) {
    let u = "Usage: policy list | show <id> | put <id> <file> [--disable] [--description TEXT] | delete <id> | validate <id> <file> | history <id> | reload";
    let sub = args.first().copied().unwrap_or("list");
    if !matches!(
        sub,
        "list"
            | "ls"
            | "show"
            | "get"
            | "put"
            | "delete"
            | "rm"
            | "validate"
            | "check"
            | "history"
            | "versions"
            | "reload"
    ) {
        return usage(u);
    }
    let svc = svc!(client, PolicyServiceClient);
    match sub {
        "list" | "ls" => match svc
            .list_policies(ListPoliciesRequest::default())
            .await
            .map(|r| r.into_owned())
        {
            Ok(v) => {
                for p in &v.policies {
                    let extra = match v.site.iter().find(|x| x.id == p.id) {
                        Some(sp) => format!("v{} {}", sp.version, sp.description),
                        None if p.template == Some(true) => "template".into(),
                        None => String::new(),
                    };
                    println!(
                        "  {:<36} {:<6} {}",
                        p.id.cyan(),
                        source_word(&p.source),
                        extra
                    );
                }
                for sp in v.site.iter().filter(|x| x.enabled == Some(false)) {
                    println!(
                        "  {:<36} {:<6} v{} {} {}",
                        sp.id.cyan(),
                        "site",
                        sp.version,
                        "(disabled)".yellow(),
                        sp.description
                    );
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        "show" | "get" => match args.get(1) {
            Some(id) => match svc
                .get_policy(GetPolicyRequest {
                    id: id.to_string(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(v) => {
                    let (pid, text, site) = match &v.policy {
                        Some(PolicyOneof::Site(p)) => (p.id.clone(), p.text.clone(), Some(p)),
                        Some(PolicyOneof::Loaded(p)) => (p.id.clone(), p.text.clone(), None),
                        None => (String::new(), String::new(), None),
                    };
                    print!(
                        "{} {} ({}",
                        "Policy".bold(),
                        pid.yellow(),
                        source_word(&v.source)
                    );
                    if let Some(p) = site {
                        print!(
                            ", version {}, {}",
                            p.version,
                            if p.enabled == Some(false) {
                                "disabled"
                            } else {
                                "enabled"
                            }
                        );
                    }
                    println!(")");
                    if let Some(p) = site
                        && !p.description.is_empty()
                    {
                        println!("{}", p.description.dimmed());
                    }
                    println!("{}", text);
                }
                Err(e) => err(rpc_err(e)),
            },
            None => usage(u),
        },
        "put" => {
            let pos = positional(&args[1..], &["--description"]);
            let (Some(id), Some(file)) = (pos.first(), pos.get(1)) else {
                return usage(u);
            };
            let text = match read_policy_file(file) {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            let (version, current) = match policy_version(&svc, id).await {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            let description = flag_value(args, "--description")
                .map(str::to_string)
                .or_else(|| current.map(|c| c.description))
                .unwrap_or_default();
            match svc
                .put_policy(PutPolicyRequest {
                    id: id.to_string(),
                    text,
                    description,
                    enabled: Some(!has_flag(args, "--disable")),
                    version,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    let p = r.policy.as_option().cloned().unwrap_or_default();
                    println!(
                        "{} {} version {}{}",
                        if version == 0 { "Created:" } else { "Updated:" }.green(),
                        id,
                        p.version,
                        if p.enabled == Some(false) {
                            " (disabled)"
                        } else {
                            ""
                        }
                    )
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "delete" | "rm" => match args.get(1) {
            Some(id) => {
                let version = match policy_version(&svc, id).await {
                    Ok((0, _)) => return err(format!("site policy {} not found", id)),
                    Ok((v, _)) => v,
                    Err(e) => return err(e),
                };
                if prompt(&format!("Delete site policy {}? [y/N]: ", id)).to_lowercase() != "y" {
                    return println!("Cancelled");
                }
                match svc
                    .delete_policy(DeletePolicyRequest {
                        id: id.to_string(),
                        version,
                        ..Default::default()
                    })
                    .await
                {
                    Ok(_) => println!("{} {}", "Deleted:".green(), id),
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => usage(u),
        },
        "validate" | "check" => {
            let (Some(id), Some(file)) = (args.get(1), args.get(2)) else {
                return usage(u);
            };
            let text = match read_policy_file(file) {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            match svc
                .validate_policy(ValidatePolicyRequest {
                    id: id.to_string(),
                    text,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(v) if v.valid == Some(true) => println!("{} {} is valid", "OK:".green(), id),
                Ok(v) => {
                    println!("{} {} is not valid:", "Invalid:".red(), id);
                    for e in &v.errors {
                        println!("  {}", e);
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "history" | "versions" => match args.get(1) {
            Some(id) => match svc
                .list_policy_versions(ListPolicyVersionsRequest {
                    id: id.to_string(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned().versions)
            {
                Ok(vs) if vs.is_empty() => println!("No history for {}.", id),
                Ok(vs) => {
                    for v in vs {
                        println!(
                            "  v{:<4} {} by {}{}",
                            v.version,
                            ts(v.time.as_option()),
                            v.author,
                            if v.deleted == Some(true) {
                                " (deleted)".red().to_string()
                            } else if v.enabled == Some(false) {
                                " (disabled)".yellow().to_string()
                            } else {
                                String::new()
                            }
                        );
                    }
                }
                Err(e) => err(rpc_err(e)),
            },
            None => usage(u),
        },
        _ => match svc.reload_policies(ReloadPoliciesRequest::default()).await {
            Ok(_) => println!("{} policies reloaded", "OK:".green()),
            Err(e) => err(rpc_err(e)),
        },
    }
}

// ---- audit --------------------------------------------------------------------

pub async fn audit(client: &ApiClient, args: &[&str]) {
    let mut req = ReadAuditRequest {
        project: flag_value(args, "--project")
            .map(str::to_string)
            .or_else(|| client.project()),
        user: flag_value(args, "--user").map(str::to_string),
        ..Default::default()
    };
    if let Some(v) = flag_value(args, "--since") {
        let Ok(ms) = v.parse::<u64>() else {
            return err("--since must be a number (Unix milliseconds)");
        };
        req.since = Timestamp {
            seconds: (ms / 1000) as i64,
            nanos: ((ms % 1000) * 1_000_000) as i32,
            ..Default::default()
        }
        .into();
    }
    if let Some(v) = flag_value(args, "--limit") {
        let Ok(n) = v.parse::<u32>() else {
            return err("--limit must be a number");
        };
        req.limit = Some(n);
    }
    let svc = svc!(client, AuditServiceClient);
    match svc.read_audit(req).await.map(|r| r.into_owned().entries) {
        Ok(es) if es.is_empty() => println!("No audit entries."),
        Ok(es) => {
            for e in es {
                let principal = serde_json::to_value(e.principal.as_option()).unwrap_or_default();
                let details = serde_json::to_value(e.details.as_option()).unwrap_or_default();
                let who = principal["name"]
                    .as_str()
                    .or(principal["user"].as_str())
                    .map(str::to_string)
                    .or_else(|| principal["token"].as_str().map(|t| format!("token:{}", t)))
                    .unwrap_or_else(|| "-".into());
                let result = if e.result == "ok" {
                    e.result.green().to_string()
                } else {
                    e.result.red().to_string()
                };
                println!(
                    "  {} {:<16} {:<22} {:<10} {} {}",
                    e.time
                        .as_option()
                        .map(|t| format_unix_time(t.seconds.max(0) as u64))
                        .unwrap_or_default()
                        .dimmed(),
                    who,
                    e.action,
                    result,
                    e.target.as_deref().unwrap_or(""),
                    details["http"].as_str().unwrap_or("").dimmed()
                );
            }
        }
        Err(e) => err(rpc_err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_edits_merge_into_the_current_set() {
        let mut q = Quotas {
            vms: Some(5),
            networks: Some(2),
            routers: Some(1),
            ..Default::default()
        };
        apply_quota_edits(&mut q, &["vms=10", "networks=none", "memory_mib=4096"]).unwrap();
        assert_eq!(q.vms, Some(10));
        assert_eq!(q.vcpus, None);
        assert_eq!(q.networks, None);
        assert_eq!(q.memory_mib, Some(4096));
        // Limits gxctl doesn't edit are kept.
        assert_eq!(q.routers, Some(1));
        assert!(apply_quota_edits(&mut q, &["cpus=1"]).is_err());
        assert!(apply_quota_edits(&mut q, &["vms"]).is_err());
        assert!(apply_quota_edits(&mut q, &["vms=lots"]).is_err());
    }

    #[test]
    fn quota_line_shows_usage_when_known() {
        let q = Quotas {
            vms: Some(4),
            ..Default::default()
        };
        let used = ProjectUsage {
            vms: 1,
            ..Default::default()
        };
        let line = quota_line(Some(&q), Some(&used));
        assert!(line.starts_with("vms 1/4  vcpus 0/∞"), "{line}");
        let line = quota_line(Some(&q), None);
        assert!(line.starts_with("vms 4  vcpus ∞"), "{line}");
    }
}

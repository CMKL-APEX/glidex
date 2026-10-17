//! `tenant` and `admission` (spec/tenancy.md §15): the first commands on
//! the generated RPC clients of `glidex-api`, over the Connect protocol
//! (spec/grpc.md §10).

use crate::client::{ApiClient, RpcTransport, parse_principal};
use crate::{flag_value, has_flag, prompt};
use colored::Colorize;
use glidex_api::proto::glidex::api::v1::EntityRef;
use glidex_api::proto::glidex::tenancy::v1::*;

pub(crate) fn err(e: impl std::fmt::Display) {
    println!("{} {}", "Error:".red(), e);
}

pub(crate) fn usage(u: &str) {
    println!("{}", u.yellow());
}

/// A connectrpc error as gxctl shows errors: the message, and the stable
/// code when the server sent one (`ErrorInfo.reason`).
pub(crate) fn rpc_err(e: connectrpc::ConnectError) -> String {
    let reason = e
        .details
        .iter()
        .find(|d| d.type_url.ends_with("google.rpc.ErrorInfo"))
        .and_then(|d| d.value.as_deref())
        .and_then(|v| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(v.trim_end_matches('='))
                .ok()
        })
        .and_then(|b| {
            <glidex_api::proto::google::rpc::ErrorInfo as buffa::Message>::decode_from_slice(&b)
                .ok()
        })
        .map(|i| i.reason);
    let msg = e
        .message
        .clone()
        .unwrap_or_else(|| e.code.as_str().to_string());
    match reason {
        Some(r) => format!("{msg} ({r})"),
        None => msg,
    }
}

pub(crate) async fn transport(client: &ApiClient) -> Option<RpcTransport> {
    match client.rpc_transport().await {
        Ok(t) => Some(t),
        Err(e) => {
            err(e.message);
            None
        }
    }
}

fn status(t: &Tenant) -> String {
    let mut tags = Vec::new();
    if t.owns_directory == Some(true) {
        tags.push("directory owner");
    }
    if t.operator == Some(true) {
        tags.push("operator");
    }
    if t.suspended == Some(true) {
        tags.push("SUSPENDED");
    }
    match t.admission.as_option() {
        Some(a) if a.status.as_known() == Some(AdmissionStatus::ADMISSION_STATUS_SUSPENDED) => {
            tags.push("admission suspended")
        }
        Some(_) => tags.push("admitted here"),
        None => tags.push("not admitted here"),
    }
    tags.join(", ")
}

fn cap_line(q: Option<&Quotas>) -> String {
    let Some(q) = q else {
        return "unlimited".into();
    };
    let parts: Vec<String> = [
        ("vms", q.vms),
        ("vcpus", q.vcpus),
        ("memory_mib", q.memory_mib),
        ("disk_gib", q.disk_gib),
        ("running_vms", q.running_vms),
        ("networks", q.networks),
        ("routers", q.routers),
        ("external_ips", q.external_ips),
    ]
    .iter()
    .filter_map(|(k, v)| v.map(|v| format!("{k}={v}")))
    .collect();
    if parts.is_empty() {
        "unlimited".into()
    } else {
        parts.join(" ")
    }
}

pub async fn tenant(client: &ApiClient, args: &[&str]) {
    let u = "Usage: tenant list | show <t> | create <slug> [--name TEXT] [--operator] | rename <t> <name> \
             | suspend <t> | resume <t> | delete <t> | members <t> | add-member <t> user:<id>|team:<id> \
             | remove-member <t> user:<id>|team:<id> | bindings <t> | bind <t> <tenant-admin|tenant-auditor> <principal> \
             | unbind <t> <link>";
    let Some(t) = transport(client).await else {
        return;
    };
    let svc = TenantServiceClient::new(t.clone(), t.config());
    let id = |i: usize| args.get(i).map(|s| s.to_string());
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match svc
            .list_tenants(ListTenantsRequest::default())
            .await
            .map(|r| r.into_owned())
        {
            Ok(r) => {
                for t in &r.tenants {
                    println!(
                        "  {:<20} {}  {}  {}",
                        t.slug.cyan(),
                        t.id.dimmed(),
                        t.display_name,
                        status(t).dimmed()
                    );
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        "show" | "get" => {
            let Some(id) = id(1) else { return usage(u) };
            match svc
                .get_tenant(GetTenantRequest {
                    id,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    let t = r.tenant.as_option().cloned().unwrap_or_default();
                    println!("  Slug:      {}", t.slug.yellow());
                    println!("  Id:        {}", t.id);
                    println!("  Name:      {}", t.display_name);
                    println!("  Directory: {}", t.directory_id);
                    println!("  Status:    {}", status(&t));
                    if let Some(a) = t.admission.as_option() {
                        println!("  Cap here:  {}", cap_line(a.cap.as_option()));
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "create" => {
            let Some(slug) = id(1) else { return usage(u) };
            let req = CreateTenantRequest {
                slug,
                display_name: flag_value(args, "--name").unwrap_or("").to_string(),
                operator: has_flag(args, "--operator").then_some(true),
                ..Default::default()
            };
            match svc.create_tenant(req).await.map(|r| r.into_owned()) {
                Ok(r) => {
                    let t = r.tenant.as_option().cloned().unwrap_or_default();
                    println!(
                        "{} {} ({})",
                        "Tenant created:".green(),
                        t.slug.yellow(),
                        t.id
                    );
                    println!("  Admit it on this cluster with: admission set {}", t.slug);
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "rename" => {
            let (Some(id), Some(name)) = (id(1), id(2)) else {
                return usage(u);
            };
            match svc
                .update_tenant(UpdateTenantRequest {
                    id,
                    display_name: Some(name),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(_) => println!("{}", "Renamed.".green()),
                Err(e) => err(rpc_err(e)),
            }
        }
        "suspend" | "resume" => {
            let Some(id) = id(1) else { return usage(u) };
            let r = if args[0] == "suspend" {
                svc.suspend_tenant(SuspendTenantRequest {
                    id: id.clone(),
                    ..Default::default()
                })
                .await
                .map(|_| ())
            } else {
                svc.resume_tenant(ResumeTenantRequest {
                    id: id.clone(),
                    ..Default::default()
                })
                .await
                .map(|_| ())
            };
            match r {
                Ok(()) => println!(
                    "{} {}",
                    if args[0] == "suspend" {
                        "Suspended:"
                    } else {
                        "Resumed:"
                    }
                    .green(),
                    id
                ),
                Err(e) => err(rpc_err(e)),
            }
        }
        "delete" | "rm" => {
            let Some(id) = id(1) else { return usage(u) };
            if prompt(&format!(
                "Delete tenant {id} (it must have no projects)? [y/N]: "
            ))
            .to_lowercase()
                != "y"
            {
                return println!("Cancelled");
            }
            match svc
                .delete_tenant(DeleteTenantRequest {
                    id: id.clone(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(_) => println!("{} {}", "Tenant deleted:".green(), id),
                Err(e) => err(rpc_err(e)),
            }
        }
        "members" => {
            let Some(id) = id(1) else { return usage(u) };
            match svc
                .list_members(ListMembersRequest {
                    id,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) if r.members.is_empty() => println!(
                    "No listed members (role links on the tenant and its projects make members too)."
                ),
                Ok(r) => {
                    for m in &r.members {
                        let p = m.principal.as_option().cloned().unwrap_or_default();
                        println!("  {}:{}", p.r#type.to_lowercase(), p.id.unwrap_or_default());
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "add-member" | "remove-member" => {
            let (Some(id), Some(p)) = (id(1), id(2)) else {
                return usage(u);
            };
            let r = if args[0] == "add-member" {
                let principal = match principal_ref(&p) {
                    Ok(r) => r,
                    Err(e) => return err(e),
                };
                svc.add_member(AddMemberRequest {
                    id,
                    principal: principal.into(),
                    ..Default::default()
                })
                .await
                .map(|_| ())
            } else {
                svc.remove_member(RemoveMemberRequest {
                    id,
                    principal: p.clone(),
                    ..Default::default()
                })
                .await
                .map(|_| ())
            };
            match r {
                Ok(()) => println!("{}", "Done.".green()),
                Err(e) => err(rpc_err(e)),
            }
        }
        "bindings" => {
            let Some(id) = id(1) else { return usage(u) };
            match svc
                .list_bindings(ListBindingsRequest {
                    id,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    for b in &r.bindings {
                        let p = b.principal.as_option().cloned().unwrap_or_default();
                        println!(
                            "  {:<22} {}:{}  {}  {}",
                            b.role.cyan(),
                            p.r#type.to_lowercase(),
                            p.id.unwrap_or_default(),
                            b.principal_name,
                            b.id.dimmed()
                        );
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "bind" => {
            let (Some(id), Some(role), Some(p)) = (id(1), id(2), id(3)) else {
                return usage(u);
            };
            let role = if role.starts_with("role.") {
                role
            } else {
                format!("role.{role}")
            };
            let principal = match principal_ref(&p) {
                Ok(r) => r,
                Err(e) => return err(e),
            };
            match svc
                .add_binding(AddBindingRequest {
                    id,
                    role,
                    principal: principal.into(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => println!(
                    "{} {}",
                    "Bound:".green(),
                    r.binding
                        .as_option()
                        .map(|b| b.id.clone())
                        .unwrap_or_default()
                ),
                Err(e) => err(rpc_err(e)),
            }
        }
        "unbind" => {
            let (Some(id), Some(link)) = (id(1), id(2)) else {
                return usage(u);
            };
            match svc
                .remove_binding(RemoveBindingRequest {
                    id,
                    link,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(_) => println!("{}", "Unbound.".green()),
                Err(e) => err(rpc_err(e)),
            }
        }
        _ => usage(u),
    }
}

fn principal_ref(s: &str) -> Result<EntityRef, String> {
    let v = parse_principal(s)?;
    Ok(EntityRef {
        r#type: v["type"].as_str().unwrap_or_default().into(),
        id: v["id"].as_str().map(String::from),
        ..Default::default()
    })
}

/// `key=value` pairs into a cap; `unlimited` clears it.
fn parse_cap(args: &[&str]) -> Result<Option<Quotas>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut q = Quotas::default();
    for a in args {
        if *a == "unlimited" {
            return Ok(Some(Quotas::default()));
        }
        let (k, v) = a
            .split_once('=')
            .ok_or_else(|| format!("'{a}': write limits as key=value"))?;
        let v: u32 = v
            .parse()
            .map_err(|_| format!("'{a}': the limit is a number"))?;
        let slot = match k {
            "vms" => &mut q.vms,
            "vcpus" => &mut q.vcpus,
            "memory_mib" => &mut q.memory_mib,
            "disk_gib" => &mut q.disk_gib,
            "running_vms" => &mut q.running_vms,
            "networks" => &mut q.networks,
            "routers" => &mut q.routers,
            "external_ips" => &mut q.external_ips,
            _ => return Err(format!("'{k}': unknown limit")),
        };
        *slot = Some(v);
    }
    Ok(Some(q))
}

pub async fn admission(client: &ApiClient, args: &[&str]) {
    let u = "Usage: admission list | set <tenant> [key=value... | unlimited] [--suspend | --resume] | remove <tenant>";
    let Some(t) = transport(client).await else {
        return;
    };
    let svc = AdmissionServiceClient::new(t.clone(), t.config());
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match svc
            .list_admissions(ListAdmissionsRequest::default())
            .await
            .map(|r| r.into_owned())
        {
            Ok(r) => {
                for a in &r.admissions {
                    let st = if a.status.as_known()
                        == Some(AdmissionStatus::ADMISSION_STATUS_SUSPENDED)
                    {
                        "suspended".red().to_string()
                    } else {
                        "admitted".green().to_string()
                    };
                    println!(
                        "  {:<38} {:<10} cap: {}",
                        a.tenant_id.cyan(),
                        st,
                        cap_line(a.cap.as_option())
                    );
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        "set" | "admit" => {
            let Some(tenant) = args.get(1).map(|s| s.to_string()) else {
                return usage(u);
            };
            let rest: Vec<&str> = args[2..]
                .iter()
                .copied()
                .filter(|a| !a.starts_with("--"))
                .collect();
            let cap = match parse_cap(&rest) {
                Ok(c) => c,
                Err(e) => return err(e),
            };
            let suspended = if has_flag(args, "--suspend") {
                Some(true)
            } else if has_flag(args, "--resume") {
                Some(false)
            } else {
                None
            };
            match svc
                .set_admission(SetAdmissionRequest {
                    tenant,
                    cap: cap.into(),
                    suspended,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    let a = r.admission.as_option().cloned().unwrap_or_default();
                    println!(
                        "{} {}  cap: {}",
                        "Admission set:".green(),
                        a.tenant_id,
                        cap_line(a.cap.as_option())
                    );
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "remove" | "rm" => {
            let Some(tenant) = args.get(1).map(|s| s.to_string()) else {
                return usage(u);
            };
            match svc
                .remove_admission(RemoveAdmissionRequest {
                    tenant: tenant.clone(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(_) => println!("{} {}", "Admission removed:".green(), tenant),
                Err(e) => err(rpc_err(e)),
            }
        }
        _ => usage(u),
    }
}

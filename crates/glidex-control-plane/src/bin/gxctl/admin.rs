//! Identity, tenancy and policy commands (spec/security.md §5, §6, §7;
//! spec/cli.md "Access control").

use crate::client::{self, enc, format_entity, parse_principal, parse_role_ref, role_name, ApiClient};
use crate::{flag_value, flag_values, format_unix_time, has_flag, project_names, prompt, prompt_hidden};
use colored::Colorize;
use hyper::{Method, StatusCode};
use serde_json::{json, Value};
use zeroize::Zeroizing;

fn err(e: impl std::fmt::Display) {
    println!("{} {}", "Error:".red(), e);
}

fn usage(u: &str) {
    println!("{}", u.yellow());
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn time_secs(v: &Value) -> String {
    v.as_u64().map(format_unix_time).unwrap_or_else(|| "-".into())
}

/// Positional arguments: everything that isn't a `--flag` or the value
/// of one of `valued` flags.
fn positional<'a>(args: &[&'a str], valued: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if valued.contains(&a) {
            i += 2;
            continue;
        }
        if !a.starts_with("--") {
            out.push(a);
        }
        i += 1;
    }
    out
}


// ---- whoami / login / logout / ui ------------------------------------------

pub async fn whoami(client: &ApiClient) {
    let w: Value = match client.request_json(Method::GET, "/auth/whoami", None).await {
        Ok(w) => w,
        Err(e) => return err(e),
    };
    let user = &w["user"];
    if user.is_null() {
        println!("  User:     {}", "(service account)".dimmed());
    } else {
        println!("  User:     {} ({})", s(&user["display_name"]).yellow(), s(&user["id"]));
    }
    println!("  Method:   {} via {}", s(&w["method"]), client.describe());
    if let Some(t) = w["token"].as_object() {
        println!("  Token:    {} ({})", t.get("name").and_then(|v| v.as_str()).unwrap_or(""), t.get("id").and_then(|v| v.as_str()).unwrap_or(""));
    }
    if w["break_glass"] == true {
        println!("  {}", "Break-glass administrator (base policies only)".red());
    }
    let teams: Vec<&str> = w["teams"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).collect();
    println!("  Teams:    {}", if teams.is_empty() { "-".to_string() } else { teams.join(", ") });
    let host: Vec<&str> = w["host_roles"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).collect();
    println!("  Host roles: {}", if host.is_empty() { "-".to_string() } else { host.join(", ") });
    let names = project_names(client).await;
    match w["default_project"].as_str() {
        Some(p) => println!("  Default project: {}", names.get(p).map(|n| format!("{} ({})", n, p)).unwrap_or_else(|| p.to_string())),
        None => println!("  Default project: -"),
    }
    if let Some(p) = client.project() {
        println!("  This session: --project {}", p.cyan());
    }
    let roles = w["project_roles"].as_array().cloned().unwrap_or_default();
    if !roles.is_empty() {
        println!("  Project roles:");
        for r in roles {
            println!(
                "    {:<20} {:<14} via {}",
                r["project_name"].as_str().unwrap_or(s(&r["project"])).cyan(),
                s(&r["role"]),
                format_entity(&r["via"])
            );
        }
    }
}

pub async fn login(client: &ApiClient, args: &[&str]) {
    if client.is_unix() {
        println!(
            "{} No login needed: on {} the control plane identifies you by your Unix account (see 'whoami').",
            "Info:".cyan(),
            client.describe()
        );
        return;
    }
    let Some(path) = client::token_path() else {
        return err("no configuration directory (HOME unset?)");
    };
    let token = if has_flag(args, "--oidc") {
        match device_login(client).await {
            Some(t) => t,
            None => return,
        }
    } else if has_flag(args, "--token") {
        let t = Zeroizing::new(prompt_hidden("Access token (input hidden): "));
        if t.trim().is_empty() {
            return err("no token given");
        }
        Zeroizing::new(t.trim().to_string())
    } else {
        return usage("Usage: login --oidc | --token   (over the local socket no login is needed)");
    };
    // Check the token before saving it.
    let previous = client::load_token().ok().flatten();
    client.set_token(Some(token.clone()));
    match client.request_json::<Value>(Method::GET, "/auth/whoami", None).await {
        Ok(w) => {
            if let Err(e) = client::save_token_file(&path, &token) {
                return err(e);
            }
            let who = w["user"]["display_name"].as_str().unwrap_or("service account");
            println!("{} logged in as {}; token saved to {} (mode 0600)", "OK:".green(), who.yellow(), path.display());
            if std::env::var_os("GLIDEX_TOKEN").is_some() {
                println!("{} GLIDEX_TOKEN is set and takes precedence in new gxctl sessions.", "Note:".dimmed());
            }
        }
        Err(e) => {
            client.set_token(previous);
            err(format!("the token was not accepted: {}", e));
        }
    }
}

/// OAuth device flow through the control plane (spec §5.4).
async fn device_login(client: &ApiClient) -> Option<Zeroizing<String>> {
    let start: Value = match client.request_json(Method::POST, "/auth/oidc/device", Some(json!({}))).await {
        Ok(v) => v,
        Err(e) => {
            err(e);
            return None;
        }
    };
    let code = s(&start["device_code"]).to_string();
    let mut interval = start["interval"].as_u64().unwrap_or(5).max(1);
    let expires = start["expires_in"].as_u64().unwrap_or(600);
    println!("To log in, open:\n\n    {}\n", s(&start["verification_uri"]).cyan().bold());
    println!("and enter the code:  {}", s(&start["user_code"]).yellow().bold());
    if let Some(u) = start["verification_uri_complete"].as_str() {
        println!("(or open {} directly)", u);
    }
    println!("Waiting for you to finish (Ctrl-C to cancel)...");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(expires);
    let body = json!({ "device_code": code, "token_name": "gxctl" });
    while std::time::Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
            _ = tokio::signal::ctrl_c() => {
                println!("Cancelled");
                return None;
            }
        }
        let r = match client.send(Method::POST, "/auth/oidc/device/poll", Some(&body)).await {
            Ok(r) => r,
            Err(e) => {
                err(e);
                return None;
            }
        };
        match r.status {
            StatusCode::ACCEPTED => continue,
            StatusCode::TOO_MANY_REQUESTS => {
                interval += 5;
                continue;
            }
            st if st.is_success() => {
                let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                return match v["token"].as_str() {
                    Some(t) => Some(Zeroizing::new(t.to_string())),
                    None => {
                        err("the control plane returned no token");
                        None
                    }
                };
            }
            st => {
                err(client::render_error(st, &r.body, true));
                return None;
            }
        }
    }
    err("the login code expired; run 'login --oidc' again");
    None
}

pub async fn logout(client: &ApiClient, args: &[&str]) {
    if client.is_unix() {
        println!("{} Nothing to log out of on the local socket.", "Info:".cyan());
        return;
    }
    if has_flag(args, "--revoke") && client.has_token() {
        match client.request_json::<Value>(Method::GET, "/auth/whoami", None).await {
            Ok(w) => match w["token"]["id"].as_str() {
                Some(id) => match client.request_json::<()>(Method::DELETE, &format!("/tokens/{}", enc(id)), None).await {
                    Ok(()) => println!("{} token {} revoked", "OK:".green(), id),
                    Err(e) => err(e),
                },
                None => err("the current credential is not a token"),
            },
            Err(e) => err(e),
        }
    }
    client.set_token(None);
    match client::token_path().map(|p| (client::delete_token_file(&p), p)) {
        Some((Ok(true), p)) => println!("{} deleted {}", "OK:".green(), p.display()),
        Some((Ok(false), _)) => println!("No saved token."),
        Some((Err(e), _)) => err(e),
        None => {}
    }
    if std::env::var_os("GLIDEX_TOKEN").is_some() {
        println!("{} GLIDEX_TOKEN is still set in your environment.", "Note:".dimmed());
    }
    if !has_flag(args, "--revoke") {
        println!("{} The token stays valid until it expires; 'logout --revoke' or 'token revoke <id>' ends it now.", "Note:".dimmed());
    }
}

pub fn ui() {
    let url = std::env::var("GLIDEX_UI_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "https://localhost:5173".into());
    println!("Web UI: {}", url.cyan());
    println!("{} Log in there with your account (PAM or OIDC, whatever the site enables).", "Note:".dimmed());
}

// ---- tokens ----------------------------------------------------------------

pub async fn token(client: &ApiClient, args: &[&str]) {
    let u = "Usage: token list | create <name> [--days N] [--service-account [--project P]] [--role ROLE[@PROJECT]]... | revoke <id>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match client.request_json::<Vec<Value>>(Method::GET, "/tokens", None).await {
            Ok(ts) if ts.is_empty() => println!("No tokens."),
            Ok(ts) => {
                let names = project_names(client).await;
                for t in ts {
                    let kind = match s(&t["kind"]) {
                        "service_account" => {
                            let p = s(&t["project"]);
                            format!("service account in {}", names.get(p).map(String::as_str).unwrap_or(p))
                        }
                        _ => "personal".to_string(),
                    };
                    let roles: Vec<String> = t["roles"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|l| format!("{}@{}", s(&l["template"]), format_entity(&l["resource"])))
                        .collect();
                    println!(
                        "  {:<24} {:<18} {}  expires {}  last used {}{}",
                        s(&t["id"]).cyan(),
                        s(&t["name"]),
                        kind,
                        time_secs(&t["expires_at"]),
                        time_secs(&t["last_used_at"]),
                        if roles.is_empty() { String::new() } else { format!("  roles: {}", roles.join(", ")) }
                    );
                }
            }
            Err(e) => err(e),
        },
        "create" => {
            let pos = positional(&args[1..], &["--days", "--project", "--role"]);
            let Some(name) = pos.first() else { return usage(u) };
            let mut body = json!({ "name": name });
            if let Some(d) = flag_value(args, "--days") {
                match d.parse::<u64>() {
                    Ok(d) => body["expires_in_days"] = json!(d),
                    Err(_) => return err("--days must be a number"),
                }
            }
            if has_flag(args, "--service-account") {
                body["kind"] = json!("service_account");
                match flag_value(args, "--project").map(str::to_string).or_else(|| client.project()) {
                    Some(p) => body["project"] = json!(p),
                    None => println!("{} no --project; the control plane uses your default project.", "Note:".dimmed()),
                }
            }
            let mut roles = Vec::new();
            for r in flag_values(args, "--role") {
                match parse_role_ref(r) {
                    Ok(v) => roles.push(v),
                    Err(e) => return err(e),
                }
            }
            body["roles"] = json!(roles);
            match client.request_json::<Value>(Method::POST, "/tokens", Some(body)).await {
                Ok(r) => {
                    let rec = &r["record"];
                    println!("{} token {} ({}), expires {}", "Created:".green(), s(&rec["name"]).yellow(), s(&rec["id"]), time_secs(&rec["expires_at"]));
                    println!();
                    println!("    {}", s(&r["token"]).bold());
                    println!();
                    println!(
                        "{} This is the only time the token is shown. Store it in a secret manager or a 0600 file; \
                         anyone holding it acts with its rights. Use it with GLIDEX_TOKEN or 'gxctl --url … login --token'.",
                        "Warning:".yellow()
                    );
                }
                Err(e) => err(e),
            }
        }
        "revoke" | "rm" | "delete" => match args.get(1) {
            Some(id) => match client.request_json::<()>(Method::DELETE, &format!("/tokens/{}", enc(id)), None).await {
                Ok(()) => println!("{} token {}", "Revoked:".green(), id),
                Err(e) => err(e),
            },
            None => usage(u),
        },
        _ => usage(u),
    }
}

// ---- projects and bindings -------------------------------------------------

const QUOTA_KEYS: [&str; 6] = ["vms", "vcpus", "memory_mib", "disk_gib", "running_vms", "networks"];

/// `key=value` quota edits applied to `quotas`; `none`/`unlimited` → no limit.
pub fn apply_quota_edits(quotas: &mut Value, edits: &[&str]) -> Result<(), String> {
    if !quotas.is_object() {
        *quotas = json!({});
    }
    for e in edits {
        let (k, v) = e.split_once('=').ok_or_else(|| format!("'{}': write quotas as key=value", e))?;
        if !QUOTA_KEYS.contains(&k) {
            return Err(format!("unknown quota '{}'; one of {}", k, QUOTA_KEYS.join(", ")));
        }
        quotas[k] = match v {
            "none" | "null" | "unlimited" | "-" => Value::Null,
            n => json!(n.parse::<u64>().map_err(|_| format!("'{}': a quota is a number or 'none'", e))?),
        };
    }
    Ok(())
}

fn quota_line(q: &Value, usage: &Value) -> String {
    QUOTA_KEYS
        .iter()
        .map(|k| {
            let limit = q[*k].as_u64().map(|v| v.to_string()).unwrap_or_else(|| "∞".into());
            match usage[*k].as_u64() {
                Some(u) => format!("{} {}/{}", k, u, limit),
                None => format!("{} {}", k, limit),
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

pub async fn project(client: &ApiClient, args: &[&str]) {
    let u = "Usage: project list | show <p> | create <name> [--description TEXT] | delete <p> | quota <p> key=value... | use <p>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match client.request_json::<Vec<Value>>(Method::GET, "/projects", None).await {
            Ok(ps) if ps.is_empty() => println!("No projects visible to you."),
            Ok(ps) => {
                for p in ps {
                    println!(
                        "  {:<20} {}  vms {}/{}  {}",
                        s(&p["name"]).cyan(),
                        s(&p["id"]).dimmed(),
                        p["usage"]["vms"].as_u64().unwrap_or(0),
                        p["quotas"]["vms"].as_u64().map(|v| v.to_string()).unwrap_or_else(|| "∞".into()),
                        s(&p["description"])
                    );
                }
            }
            Err(e) => err(e),
        },
        "show" | "get" => match args.get(1) {
            Some(p) => match client.request_json::<Value>(Method::GET, &format!("/projects/{}", enc(p)), None).await {
                Ok(p) => {
                    println!("  Name:        {}", s(&p["name"]).yellow());
                    println!("  Id:          {}", s(&p["id"]));
                    println!("  Description: {}", s(&p["description"]));
                    println!("  Created:     {}", time_secs(&p["created_at"]));
                    println!("  Usage/quota: {}", quota_line(&p["quotas"], &p["usage"]));
                }
                Err(e) => err(e),
            },
            None => usage(u),
        },
        "create" => {
            let pos = positional(&args[1..], &["--description"]);
            let Some(name) = pos.first() else { return usage(u) };
            let body = json!({ "name": name, "description": flag_value(args, "--description").unwrap_or("") });
            match client.request_json::<Value>(Method::POST, "/projects", Some(body)).await {
                Ok(p) => println!("{} {} ({})", "Project created:".green(), s(&p["name"]).yellow(), s(&p["id"])),
                Err(e) => err(e),
            }
        }
        "delete" | "rm" => match args.get(1) {
            Some(p) => {
                if prompt(&format!("Delete project {} (it must be empty; its service-account tokens are revoked)? [y/N]: ", p)).to_lowercase() != "y" {
                    return println!("Cancelled");
                }
                match client.request_json::<()>(Method::DELETE, &format!("/projects/{}", enc(p)), None).await {
                    Ok(()) => println!("{} {}", "Project deleted:".green(), p),
                    Err(e) => err(e),
                }
            }
            None => usage(u),
        },
        "quota" | "quotas" => {
            let Some(p) = args.get(1) else { return usage(u) };
            let path = format!("/projects/{}", enc(p));
            let current: Value = match client.request_json(Method::GET, &path, None).await {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            if args.len() < 3 {
                return println!("  {}", quota_line(&current["quotas"], &current["usage"]));
            }
            // PATCH replaces the whole set, so start from the current one.
            let mut q = current["quotas"].clone();
            if let Err(e) = apply_quota_edits(&mut q, &args[2..]) {
                return err(e);
            }
            match client.request_json::<Value>(Method::PATCH, &path, Some(json!({ "quotas": q }))).await {
                Ok(p) => println!("{} {}: {}", "Quotas updated:".green(), s(&p["name"]), quota_line(&p["quotas"], &current["usage"])),
                Err(e) => err(e),
            }
        }
        "use" => match args.get(1) {
            Some(p) => {
                let body = json!({ "default_project": if *p == "-" || *p == "none" { "" } else { *p } });
                match client.request::<Value>(Method::PATCH, "/users/me", Some(body)).await {
                    Ok(u) => {
                        client.set_project(None);
                        match u["default_project"].as_str() {
                            Some(id) => println!("{} default project is now {} ({})", "OK:".green(), p.yellow(), id),
                            None => println!("{} no default project", "OK:".green()),
                        }
                    }
                    Err(e) if e.status == Some(StatusCode::BAD_REQUEST) && e.code.as_deref() == Some("invalid") => {
                        // Service accounts and auth-disabled servers have no profile.
                        client.set_project(Some(p.to_string()));
                        println!("{} {}; using project {} for this session only", "Note:".yellow(), e.message, p);
                    }
                    Err(e) => err(e),
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

fn print_links(links: &[Value]) {
    if links.is_empty() {
        return println!("No bindings.");
    }
    for l in links {
        println!(
            "  {:<24} {:<18} {:<46} by {} {}",
            s(&l["id"]).cyan(),
            s(&l["template"]),
            format_entity(&l["principal"]),
            s(&l["created_by"]),
            time_secs(&l["created_at"]).dimmed()
        );
    }
}

pub async fn binding(client: &ApiClient, args: &[&str]) {
    let u = "Usage: binding list <project> | add <project> <role> user:<id>|team:<id>|token:<id> | remove <project> <binding-id>";
    match (args.first().copied().unwrap_or("list"), args.get(1)) {
        ("list" | "ls", Some(p)) => match client.request_json::<Vec<Value>>(Method::GET, &format!("/projects/{}/bindings", enc(p)), None).await {
            Ok(l) => print_links(&l),
            Err(e) => err(e),
        },
        ("add", Some(p)) => {
            let (Some(role), Some(who)) = (args.get(2), args.get(3)) else { return usage(u) };
            let principal = match parse_principal(who) {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            let body = json!({ "role": role_name(role), "principal": principal });
            match client.request_json::<Value>(Method::POST, &format!("/projects/{}/bindings", enc(p)), Some(body)).await {
                Ok(l) => println!("{} {} {} → {}", "Added:".green(), s(&l["id"]), s(&l["template"]), format_entity(&l["principal"])),
                Err(e) => err(e),
            }
        }
        ("remove" | "rm" | "delete", Some(p)) => match args.get(2) {
            Some(link) => match client.request_json::<()>(Method::DELETE, &format!("/projects/{}/bindings/{}", enc(p), enc(link)), None).await {
                Ok(()) => println!("{} {}", "Removed:".green(), link),
                Err(e) => err(e),
            },
            None => usage(u),
        },
        _ => usage(u),
    }
}

pub async fn system_binding(client: &ApiClient, args: &[&str]) {
    let u = "Usage: system-binding list | add <role> user:<id>|team:<id>|token:<id> | remove <binding-id>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match client.request_json::<Vec<Value>>(Method::GET, "/system/bindings", None).await {
            Ok(l) => print_links(&l),
            Err(e) => err(e),
        },
        "add" => {
            let (Some(role), Some(who)) = (args.get(1), args.get(2)) else { return usage(u) };
            let principal = match parse_principal(who) {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            let body = json!({ "role": role_name(role), "principal": principal });
            match client.request_json::<Value>(Method::POST, "/system/bindings", Some(body)).await {
                Ok(l) => println!("{} {} {} → {}", "Added:".green(), s(&l["id"]), s(&l["template"]), format_entity(&l["principal"])),
                Err(e) => err(e),
            }
        }
        "remove" | "rm" | "delete" => match args.get(1) {
            Some(link) => match client.request_json::<()>(Method::DELETE, &format!("/system/bindings/{}", enc(link)), None).await {
                Ok(()) => println!("{} {}", "Removed:".green(), link),
                Err(e) => err(e),
            },
            None => usage(u),
        },
        _ => usage(u),
    }
}

// ---- users and teams -------------------------------------------------------

pub async fn user(client: &ApiClient, args: &[&str]) {
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match client.request_json::<Vec<Value>>(Method::GET, "/users", None).await {
            Ok(us) => {
                for u in us {
                    let ids: Vec<String> = u["identities"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|i| format!("{}:{}", s(&i["provider"]), s(&i["subject"])))
                        .collect();
                    println!(
                        "  {:<38} {:<20} {}{}",
                        s(&u["id"]).cyan(),
                        s(&u["display_name"]),
                        ids.join(", "),
                        if u["disabled"] == true { " (disabled)".red().to_string() } else { String::new() }
                    );
                }
            }
            Err(e) => err(e),
        },
        _ => usage("Usage: user list"),
    }
}

/// A team id from an id or a name.
async fn resolve_team(client: &ApiClient, key: &str) -> Result<String, String> {
    let teams: Vec<Value> = client.request_json(Method::GET, "/teams", None).await?;
    teams
        .iter()
        .find(|t| t["id"] == key)
        .or_else(|| teams.iter().find(|t| t["name"] == key))
        .and_then(|t| t["id"].as_str().map(str::to_string))
        .ok_or_else(|| format!("team '{}' not found", key))
}

pub async fn team(client: &ApiClient, args: &[&str]) {
    let u = "Usage: team list | create <name> | delete <team> | add-member <team> <user-id> | remove-member <team> <user-id>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match client.request_json::<Vec<Value>>(Method::GET, "/teams", None).await {
            Ok(ts) if ts.is_empty() => println!("No teams."),
            Ok(ts) => {
                for t in ts {
                    let members: Vec<String> = t["members"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|m| format!("{}({})", s(&m["user_id"]), s(&m["source"])))
                        .collect();
                    println!("  {:<20} {}  members: {}", s(&t["name"]).cyan(), s(&t["id"]).dimmed(), if members.is_empty() { "-".into() } else { members.join(", ") });
                }
            }
            Err(e) => err(e),
        },
        "create" => match args.get(1) {
            Some(name) => match client.request_json::<Value>(Method::POST, "/teams", Some(json!({ "name": name }))).await {
                Ok(t) => println!("{} {} ({})", "Team created:".green(), s(&t["name"]).yellow(), s(&t["id"])),
                Err(e) => err(e),
            },
            None => usage(u),
        },
        "delete" | "rm" => match args.get(1) {
            Some(t) => {
                let id = match resolve_team(client, t).await {
                    Ok(id) => id,
                    Err(e) => return err(e),
                };
                match client.request_json::<()>(Method::DELETE, &format!("/teams/{}", enc(&id)), None).await {
                    Ok(()) => println!("{} {}", "Team deleted:".green(), t),
                    Err(e) => err(e),
                }
            }
            None => usage(u),
        },
        sub @ ("add-member" | "remove-member") => {
            let (Some(t), Some(user)) = (args.get(1), args.get(2)) else { return usage(u) };
            let id = match resolve_team(client, t).await {
                Ok(id) => id,
                Err(e) => return err(e),
            };
            let method = if sub == "add-member" { Method::PUT } else { Method::DELETE };
            match client.request_json::<Value>(method, &format!("/teams/{}/members/{}", enc(&id), enc(user)), None).await {
                Ok(t) => println!("{} {} now has {} member(s)", "OK:".green(), s(&t["name"]), t["members"].as_array().map(|m| m.len()).unwrap_or(0)),
                Err(e) => err(e),
            }
        }
        _ => usage(u),
    }
}

// ---- site policies ---------------------------------------------------------

/// The current version of site policy `id`: 0 when it doesn't exist.
async fn policy_version(client: &ApiClient, id: &str) -> Result<(u64, Option<Value>), String> {
    match client.request::<Value>(Method::GET, &format!("/authz/policies/{}", enc(id)), None).await {
        Ok(v) if v["source"] == "site" => Ok((v["policy"]["version"].as_u64().unwrap_or(0), Some(v["policy"].clone()))),
        Ok(v) => Err(format!("{} is a {} policy and can't be changed through the API", id, s(&v["source"]))),
        Err(e) if e.status == Some(StatusCode::NOT_FOUND) => Ok((0, None)),
        Err(e) => Err(e.message),
    }
}

fn read_policy_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path, e))
}

pub async fn policy(client: &ApiClient, args: &[&str]) {
    let u = "Usage: policy list | show <id> | put <id> <file> [--disable] [--description TEXT] | delete <id> | validate <id> <file> | history <id> | reload";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match client.request_json::<Value>(Method::GET, "/authz/policies", None).await {
            Ok(v) => {
                let site = v["site"].as_array().cloned().unwrap_or_default();
                for p in v["policies"].as_array().into_iter().flatten() {
                    let id = s(&p["id"]);
                    let extra = match site.iter().find(|x| x["id"] == id) {
                        Some(sp) => format!("v{} {}", sp["version"], s(&sp["description"])),
                        None if p["template"] == true => "template".into(),
                        None => String::new(),
                    };
                    println!("  {:<36} {:<6} {}", id.cyan(), s(&p["source"]), extra);
                }
                for sp in site.iter().filter(|x| x["enabled"] == false) {
                    println!("  {:<36} {:<6} v{} {} {}", s(&sp["id"]).cyan(), "site", sp["version"], "(disabled)".yellow(), s(&sp["description"]));
                }
            }
            Err(e) => err(e),
        },
        "show" | "get" => match args.get(1) {
            Some(id) => match client.request_json::<Value>(Method::GET, &format!("/authz/policies/{}", enc(id)), None).await {
                Ok(v) => {
                    let p = &v["policy"];
                    print!("{} {} ({}", "Policy".bold(), s(&p["id"]).yellow(), s(&v["source"]));
                    if let Some(ver) = p["version"].as_u64() {
                        print!(", version {}, {}", ver, if p["enabled"] == false { "disabled" } else { "enabled" });
                    }
                    println!(")");
                    if !s(&p["description"]).is_empty() {
                        println!("{}", s(&p["description"]).dimmed());
                    }
                    println!("{}", s(&p["text"]));
                }
                Err(e) => err(e),
            },
            None => usage(u),
        },
        "put" => {
            let pos = positional(&args[1..], &["--description"]);
            let (Some(id), Some(file)) = (pos.first(), pos.get(1)) else { return usage(u) };
            let text = match read_policy_file(file) {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            let (version, current) = match policy_version(client, id).await {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            let description = flag_value(args, "--description")
                .map(str::to_string)
                .or_else(|| current.as_ref().map(|c| s(&c["description"]).to_string()))
                .unwrap_or_default();
            let body = json!({ "text": text, "description": description, "enabled": !has_flag(args, "--disable"), "version": version });
            match client.request_json::<Value>(Method::PUT, &format!("/authz/policies/{}", enc(id)), Some(body)).await {
                Ok(p) => println!("{} {} version {}{}", if version == 0 { "Created:" } else { "Updated:" }.green(), id, p["version"], if p["enabled"] == false { " (disabled)" } else { "" }),
                Err(e) => err(e),
            }
        }
        "delete" | "rm" => match args.get(1) {
            Some(id) => {
                let version = match policy_version(client, id).await {
                    Ok((0, _)) => return err(format!("site policy {} not found", id)),
                    Ok((v, _)) => v,
                    Err(e) => return err(e),
                };
                if prompt(&format!("Delete site policy {}? [y/N]: ", id)).to_lowercase() != "y" {
                    return println!("Cancelled");
                }
                match client.request_json::<()>(Method::DELETE, &client::add_query(&format!("/authz/policies/{}", enc(id)), "version", &version.to_string()), None).await {
                    Ok(()) => println!("{} {}", "Deleted:".green(), id),
                    Err(e) => err(e),
                }
            }
            None => usage(u),
        },
        "validate" | "check" => {
            let (Some(id), Some(file)) = (args.get(1), args.get(2)) else { return usage(u) };
            let text = match read_policy_file(file) {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            match client.request_json::<Value>(Method::POST, "/authz/validate", Some(json!({ "id": id, "text": text }))).await {
                Ok(v) if v["valid"] == true => println!("{} {} is valid", "OK:".green(), id),
                Ok(v) => {
                    println!("{} {} is not valid:", "Invalid:".red(), id);
                    for e in v["errors"].as_array().into_iter().flatten() {
                        println!("  {}", e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string()));
                    }
                }
                Err(e) => err(e),
            }
        }
        "history" | "versions" => match args.get(1) {
            Some(id) => match client.request_json::<Vec<Value>>(Method::GET, &format!("/authz/policies/{}/versions", enc(id)), None).await {
                Ok(vs) if vs.is_empty() => println!("No history for {}.", id),
                Ok(vs) => {
                    for v in vs {
                        println!(
                            "  v{:<4} {} by {}{}",
                            v["version"],
                            time_secs(&v["time"]),
                            s(&v["author"]),
                            if v["deleted"] == true { " (deleted)".red().to_string() } else if v["enabled"] == false { " (disabled)".yellow().to_string() } else { String::new() }
                        );
                    }
                }
                Err(e) => err(e),
            },
            None => usage(u),
        },
        "reload" => match client.request_json::<()>(Method::POST, "/authz/reload", None).await {
            Ok(()) => println!("{} policies reloaded", "OK:".green()),
            Err(e) => err(e),
        },
        _ => usage(u),
    }
}

// ---- audit -----------------------------------------------------------------

pub async fn audit(client: &ApiClient, args: &[&str]) {
    let mut path = "/audit".to_string();
    if let Some(p) = flag_value(args, "--project").map(str::to_string).or_else(|| client.project()) {
        path = client::add_query(&path, "project", &p);
    }
    for (flag, key) in [("--since", "since"), ("--limit", "limit"), ("--user", "user")] {
        if let Some(v) = flag_value(args, flag) {
            if key != "user" && v.parse::<u64>().is_err() {
                return err(format!("{} must be a number{}", flag, if key == "since" { " (Unix milliseconds)" } else { "" }));
            }
            path = client::add_query(&path, key, v);
        }
    }
    match client.request_json::<Vec<Value>>(Method::GET, &path, None).await {
        Ok(es) if es.is_empty() => println!("No audit entries."),
        Ok(es) => {
            for e in es {
                let who = e["principal"]["name"]
                    .as_str()
                    .or(e["principal"]["user"].as_str())
                    .map(str::to_string)
                    .or_else(|| e["principal"]["token"].as_str().map(|t| format!("token:{}", t)))
                    .unwrap_or_else(|| "-".into());
                let result = s(&e["result"]);
                let result = if result == "ok" { result.green().to_string() } else { result.red().to_string() };
                println!(
                    "  {} {:<16} {:<22} {:<10} {} {}",
                    e["time"].as_u64().map(|ms| format_unix_time(ms / 1000)).unwrap_or_default().dimmed(),
                    who,
                    s(&e["action"]),
                    result,
                    s(&e["target"]),
                    e["details"]["http"].as_str().unwrap_or("").dimmed()
                );
            }
        }
        Err(e) => err(e),
    }
}

/// `GET /usage` path from `usage` arguments (spec/metering.md §12).
fn usage_path(args: &[&str], project: Option<String>) -> String {
    let mut path = "/usage".to_string();
    if let Some(p) = flag_value(args, "--project").map(str::to_string).or(project) {
        path = client::add_query(&path, "project", &p);
    }
    let granularity = flag_value(args, "--granularity").unwrap_or("month");
    path = client::add_query(&path, "granularity", granularity);
    for (flag, key) in [("--from", "from"), ("--to", "to"), ("--by", "group_by"), ("--meters", "meters"), ("--tz", "tz")] {
        if let Some(v) = flag_value(args, flag) {
            path = client::add_query(&path, key, v);
        }
    }
    if args.contains(&"--csv") {
        path = client::add_query(&path, "format", "csv");
    }
    path
}

/// `usage [--from D] [--to D] [--by project|vm|disk|nic|network] [--granularity hour|day|month] [--meters m,…] [--tz Z] [--csv]`
pub async fn resource_usage(client: &ApiClient, args: &[&str]) {
    let path = usage_path(args, client.project());
    if args.contains(&"--csv") {
        match client.request_bytes(Method::GET, &path, None).await {
            Ok(r) => print!("{}", String::from_utf8_lossy(&r.body)),
            Err(e) => err(e.message),
        }
        return;
    }
    let v: Value = match client.request_json(Method::GET, &path, None).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    println!(
        "{} {} → {} ({}), complete through {}",
        "Usage".bold(),
        s(&v["from"]),
        s(&v["to"]),
        s(&v["timezone"]),
        s(&v["complete_through"])
    );
    let rows = v["rows"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("  No usage recorded.");
        return;
    }
    let keys = v["group_by"].as_array().cloned().unwrap_or_default();
    for r in rows {
        let who: Vec<String> = keys
            .iter()
            .filter_map(|k| {
                let k = k.as_str()?;
                let n = &r[k];
                n.is_object().then(|| format!("{}={}", k, n["name"].as_str().unwrap_or("?")))
            })
            .collect();
        let flags: Vec<&str> = r["flags"].as_array().map(|f| f.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        println!(
            "\n  {} {} {}",
            s(&r["start"]).dimmed(),
            who.join(" ").cyan(),
            if flags.is_empty() { String::new() } else { format!("[{}]", flags.join(", ")).yellow().to_string() }
        );
        if let Some(meters) = r["meters"].as_object() {
            for (m, x) in meters {
                println!("    {:<26} {:>16} {}", m, format!("{}", x["value"]), s(&x["unit"]));
            }
        }
    }
}

// ---- project networks and sharing ------------------------------------------

pub async fn network(client: &ApiClient, args: &[&str]) {
    let u = "Usage: network list | create <name> [--project P] [--subnet CIDR] ... | rm <name> | share <net> <project-id> | unshare <net> <project-id> | shares <project> | accept <project> <net> | leave <project> <net>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => crate::list_networks(client).await,
        "create" | "add" => {
            let project = flag_value(args, "--project");
            match project {
                // Project networks: NAT only, the bridge is generated.
                Some(p) => {
                    let pos = positional(&args[1..], &["--project", "--subnet", "--mtu"]);
                    let Some(name) = pos.first() else { return usage(u) };
                    let mut body = json!({ "name": name, "mode": "nat" });
                    if let Some(s) = flag_value(args, "--subnet") {
                        body["subnet"] = json!(s);
                    }
                    if let Some(m) = flag_value(args, "--mtu").and_then(|m| m.parse::<u16>().ok()) {
                        body["mtu"] = json!(m);
                    }
                    if has_flag(args, "--vhost-user") {
                        body["port_type"] = json!("vhost_user");
                    }
                    match client.request_json::<Value>(Method::POST, &format!("/projects/{}/networks", enc(p)), Some(body)).await {
                        Ok(n) => println!("{} {} (NAT on {}, project {})", "Network created:".green(), s(&n["name"]).yellow(), s(&n["bridge"]), p),
                        Err(e) => err(e),
                    }
                }
                None => crate::handle_network_add(client, &args[1..]).await,
            }
        }
        "rm" | "delete" => match args.get(1) {
            Some(name) => match client.request_json::<serde_json::Value>(Method::DELETE, &format!("/networks/{}?wait=60", enc(name)), None).await {
                Ok(v) => crate::print_delete_result("Network", name, &v),
                Err(e) => err(e),
            },
            None => usage(u),
        },
        "share" => {
            let (Some(net), Some(project)) = (args.get(1), args.get(2)) else { return usage(u) };
            match client.request_json::<()>(Method::POST, &format!("/networks/{}/shares", enc(net)), Some(json!({ "project": project }))).await {
                Ok(()) => println!(
                    "{} {} offered to project {}. Its owner accepts with 'network accept {} {}' within 7 days.",
                    "Offered:".green(),
                    net,
                    project,
                    project,
                    net
                ),
                Err(e) => err(e),
            }
        }
        "unshare" => {
            let (Some(net), Some(project)) = (args.get(1), args.get(2)) else { return usage(u) };
            match client.request_json::<()>(Method::DELETE, &format!("/networks/{}/shares/{}", enc(net), enc(project)), None).await {
                Ok(()) => println!("{} {} no longer shared with {}", "OK:".green(), net, project),
                Err(e) => err(e),
            }
        }
        "shares" => {
            let Some(project) = args.get(1).map(|s| s.to_string()).or_else(|| client.project()) else { return usage(u) };
            match client.request_json::<Vec<Value>>(Method::GET, &format!("/projects/{}/network-shares", enc(&project)), None).await {
                Ok(v) if v.is_empty() => println!("No network shares for {}.", project),
                Ok(v) => {
                    for sh in v {
                        let status = s(&sh["status"]);
                        println!(
                            "  {:<20} from {:<38} {}{}",
                            s(&sh["network"]).cyan(),
                            sh["owner_project"].as_str().unwrap_or("-"),
                            if status == "accepted" { status.green().to_string() } else { status.yellow().to_string() },
                            sh["expires_at"].as_u64().map(|t| format!(" (until {})", format_unix_time(t))).unwrap_or_default()
                        );
                    }
                }
                Err(e) => err(e),
            }
        }
        "accept" => {
            let (Some(project), Some(net)) = (args.get(1), args.get(2)) else { return usage(u) };
            match client.request_json::<Value>(Method::POST, &format!("/projects/{}/network-shares/{}/accept", enc(project), enc(net)), None).await {
                Ok(_) => println!("{} project {} can now attach VMs to {}", "Accepted:".green(), project, net),
                Err(e) => err(e),
            }
        }
        "leave" => {
            let (Some(project), Some(net)) = (args.get(1), args.get(2)) else { return usage(u) };
            match client.request_json::<()>(Method::DELETE, &format!("/projects/{}/network-shares/{}", enc(project), enc(net)), None).await {
                Ok(()) => println!("{} project {} left {}", "OK:".green(), project, net),
                Err(e) => err(e),
            }
        }
        _ => usage(u),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn usage_query_from_flags() {
        let p = super::usage_path(&["--by", "vm,nic", "--from", "2026-10-01T00:00:00Z", "--csv"], Some("pa".into()));
        assert!(p.starts_with("/usage?project=pa&granularity=month&from=2026-10-01T00%3A00%3A00Z&group_by=vm%2Cnic"), "{p}");
        assert!(p.ends_with("&format=csv"), "{p}");
        assert_eq!(super::usage_path(&["--granularity", "day"], None), "/usage?granularity=day");
    }
    use super::*;

    #[test]
    fn quota_edits_merge_into_the_current_set() {
        let mut q = json!({ "vms": 5, "vcpus": null, "networks": 2 });
        apply_quota_edits(&mut q, &["vms=10", "networks=none", "memory_mib=4096"]).unwrap();
        assert_eq!(q, json!({ "vms": 10, "vcpus": null, "networks": null, "memory_mib": 4096 }));
        assert!(apply_quota_edits(&mut q, &["cpus=1"]).is_err());
        assert!(apply_quota_edits(&mut q, &["vms"]).is_err());
        assert!(apply_quota_edits(&mut q, &["vms=lots"]).is_err());
    }

    #[test]
    fn positional_arguments_skip_flags_and_their_values() {
        assert_eq!(positional(&["ci", "--days", "7", "--service-account", "--role", "viewer"], &["--days", "--role"]), vec!["ci"]);
        assert_eq!(positional(&["--project", "lab", "net1"], &["--project"]), vec!["net1"]);
    }
}

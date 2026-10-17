//! `connector` (spec/tenancy.md §14): the directory's login connectors on
//! `glidex.directory.v1.ConnectorService`, over the Connect protocol.
//! Secrets are read from a hidden prompt, stdin or a file, never from the
//! command line, and are never shown.

use crate::client::ApiClient;
use crate::tenancy::{err, rpc_err, transport, usage};
use crate::{flag_value, flag_values, has_flag, prompt_hidden};
use colored::Colorize;
use glidex_api::proto::glidex::directory::v1::*;
use std::collections::HashMap;
use zeroize::Zeroizing;

type Settings = glidex_api::proto::glidex::directory::v1::__buffa::oneof::connector::Settings;

const USAGE: &str = "Usage: connector list | show <id> \
    | add oidc <id> --issuer URL --client-id ID [--name TEXT] [--scope S]... [--groups-claim C] [--domain D]... [--mfa-acr V]... [COMMON] \
    | add pam <id> [--name TEXT] [--allow-group G]... [COMMON] \
    | update <id> [--name TEXT] [--enable|--disable] [COMMON] \
    | secret set <id> [field] [--from-file PATH] | test <id> | remove <id>\n\
    COMMON: [--jit|--no-jit] [--require-group G]... [--mfa|--no-mfa] [--team GROUP=TEAM]... [--default-tenant T] [--token-max-days N]";

fn kind(c: &Connector) -> &'static str {
    match c.settings {
        Some(Settings::Pam(_)) => "pam",
        Some(Settings::Oidc(_)) => "oidc",
        None => "?",
    }
}

fn yes_no(b: Option<bool>) -> &'static str {
    if b == Some(true) {
        "yes"
    } else {
        "no"
    }
}

fn show(c: &Connector) {
    println!("  Id:             {}", c.id.yellow());
    println!("  Name:           {}", c.display_name);
    println!("  Kind:           {}  ({})", kind(c), c.source);
    println!("  Enabled:        {}", yes_no(c.enabled));
    println!("  Create users:   {}", yes_no(c.jit));
    println!("  Require MFA:    {}", yes_no(c.require_mfa));
    if !c.required_groups.is_empty() {
        println!("  Required groups: {}", c.required_groups.join(", "));
    }
    if let Some(t) = &c.default_tenant {
        println!("  Default tenant: {t}");
    }
    if let Some(d) = c.token_max_days {
        println!("  Token max days: {d}");
    }
    let mut teams: Vec<_> = c.group_teams.iter().collect();
    teams.sort();
    for (g, t) in teams {
        println!("  Team:           {g} → {t}");
    }
    match &c.settings {
        Some(Settings::Oidc(o)) => {
            println!("  Issuer:         {}", o.issuer);
            println!("  Client id:      {}", o.client_id);
            if !o.scopes.is_empty() {
                println!("  Scopes:         {}", o.scopes.join(" "));
            }
            if !o.allowed_domains.is_empty() {
                println!("  Email domains:  {}", o.allowed_domains.join(", "));
            }
            if let Some(r) = &o.redirect_uri {
                println!("  Redirect URI:   {r}");
            }
        }
        Some(Settings::Pam(p)) if !p.allowed_groups.is_empty() => {
            println!("  Unix groups:    {}", p.allowed_groups.join(", "))
        }
        _ => {}
    }
    for s in &c.secrets {
        let state = if s.set == Some(true) {
            "set".green()
        } else {
            "NOT SET".red()
        };
        println!("  Secret:         {} {}", s.field, state);
    }
}

/// The settings every kind shares, from the flags given; returns the
/// update-mask paths they touch.
fn common(args: &[&str], c: &mut Connector) -> Result<Vec<&'static str>, String> {
    let mut paths = Vec::new();
    if let Some(n) = flag_value(args, "--name") {
        c.display_name = n.into();
        paths.push("display_name");
    }
    for (on, off, field, path) in [
        ("--jit", "--no-jit", &mut c.jit, "jit"),
        ("--mfa", "--no-mfa", &mut c.require_mfa, "require_mfa"),
    ] {
        if has_flag(args, on) || has_flag(args, off) {
            *field = Some(has_flag(args, on));
            paths.push(path);
        }
    }
    let groups = flag_values(args, "--require-group");
    if !groups.is_empty() {
        c.required_groups = groups.iter().map(|s| s.to_string()).collect();
        paths.push("required_groups");
    }
    let teams = flag_values(args, "--team");
    if !teams.is_empty() {
        let mut m = HashMap::new();
        for t in teams {
            let (g, team) = t
                .split_once('=')
                .ok_or_else(|| format!("--team takes GROUP=TEAM, not {t}"))?;
            m.insert(g.to_string(), team.to_string());
        }
        c.group_teams = m.into_iter().collect();
        paths.push("group_teams");
    }
    if let Some(t) = flag_value(args, "--default-tenant") {
        c.default_tenant = Some(t.into());
        paths.push("default_tenant");
    }
    if let Some(d) = flag_value(args, "--token-max-days") {
        c.token_max_days = Some(
            d.parse()
                .map_err(|_| format!("--token-max-days takes a number, not {d}"))?,
        );
        paths.push("token_max_days");
    }
    Ok(paths)
}

fn strings(args: &[&str], flag: &str) -> Vec<String> {
    flag_values(args, flag)
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn read_secret(args: &[&str], field: &str) -> Result<Zeroizing<String>, String> {
    let v = match flag_value(args, "--from-file") {
        Some(p) => Zeroizing::new(std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?),
        None => Zeroizing::new(prompt_hidden(&format!("{field} (input hidden): "))),
    };
    let v = Zeroizing::new(v.trim().to_string());
    if v.is_empty() {
        return Err("no value given".into());
    }
    Ok(v)
}

pub async fn connector(client: &ApiClient, args: &[&str]) {
    let Some(t) = transport(client).await else {
        return;
    };
    let svc = ConnectorServiceClient::new(t.clone(), t.config());
    let id = |i: usize| args.get(i).map(|s| s.to_string());
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match svc
            .list_connectors(ListConnectorsRequest::default())
            .await
            .map(|r| r.into_owned())
        {
            Ok(r) => {
                for c in &r.connectors {
                    let off = if c.enabled == Some(true) {
                        String::new()
                    } else {
                        " DISABLED".red().to_string()
                    };
                    let unset = c.secrets.iter().any(|s| s.set != Some(true));
                    let warn = if unset {
                        " (secret not set)".red().to_string()
                    } else {
                        String::new()
                    };
                    println!(
                        "  {:<20} {:<5} {}{}{}  {}",
                        c.id.cyan(),
                        kind(c),
                        c.display_name,
                        off,
                        warn,
                        c.source.dimmed()
                    );
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        "show" | "get" => {
            let Some(id) = id(1) else { return usage(USAGE) };
            match svc
                .get_connector(GetConnectorRequest {
                    id,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => show(&r.connector.as_option().cloned().unwrap_or_default()),
                Err(e) => err(rpc_err(e)),
            }
        }
        "add" | "create" => {
            let (Some(k), Some(cid)) = (args.get(1).copied(), id(2)) else {
                return usage(USAGE);
            };
            let mut c = Connector {
                id: cid,
                ..Default::default()
            };
            if let Err(e) = common(args, &mut c) {
                return err(e);
            }
            c.settings = match k {
                "oidc" => {
                    let (Some(issuer), Some(client_id)) = (
                        flag_value(args, "--issuer"),
                        flag_value(args, "--client-id"),
                    ) else {
                        return usage(USAGE);
                    };
                    Some(Settings::Oidc(Box::new(OidcSettings {
                        issuer: issuer.into(),
                        client_id: client_id.into(),
                        scopes: strings(args, "--scope"),
                        groups_claim: flag_value(args, "--groups-claim").unwrap_or("").into(),
                        allowed_domains: strings(args, "--domain"),
                        mfa_acr_values: strings(args, "--mfa-acr"),
                        ..Default::default()
                    })))
                }
                "pam" => Some(Settings::Pam(Box::new(PamSettings {
                    allowed_groups: strings(args, "--allow-group"),
                    ..Default::default()
                }))),
                _ => return usage(USAGE),
            };
            match svc
                .create_connector(CreateConnectorRequest {
                    connector: c.into(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    let c = r.connector.as_option().cloned().unwrap_or_default();
                    println!("{} {}", "Connector created:".green(), c.id.yellow());
                    for s in c.secrets.iter().filter(|s| s.set != Some(true)) {
                        println!(
                            "  Set its {} with: connector secret set {} {}",
                            s.field, c.id, s.field
                        );
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "update" | "set" => {
            let Some(cid) = id(1) else {
                return usage(USAGE);
            };
            let mut c = Connector::default();
            let mut paths = match common(args, &mut c) {
                Ok(p) => p,
                Err(e) => return err(e),
            };
            if has_flag(args, "--enable") || has_flag(args, "--disable") {
                c.enabled = Some(has_flag(args, "--enable"));
                paths.push("enabled");
            }
            if paths.is_empty() {
                return usage(USAGE);
            }
            let mask = buffa_types::google::protobuf::FieldMask {
                paths: paths.iter().map(|p| p.to_string()).collect(),
                ..Default::default()
            };
            let req = UpdateConnectorRequest {
                id: cid,
                connector: c.into(),
                update_mask: mask.into(),
                ..Default::default()
            };
            match svc.update_connector(req).await.map(|r| r.into_owned()) {
                Ok(r) => show(&r.connector.as_option().cloned().unwrap_or_default()),
                Err(e) => err(rpc_err(e)),
            }
        }
        "secret" | "secrets" => {
            let (Some("set"), Some(cid)) = (args.get(1).copied(), id(2)) else {
                return usage(USAGE);
            };
            let field = args
                .get(3)
                .filter(|a| !a.starts_with("--"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| "client_secret".into());
            let value = match read_secret(args, &field) {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            let req = SetSecretRequest {
                id: cid.clone(),
                field: field.clone(),
                value: value.to_string(),
                ..Default::default()
            };
            match svc.set_secret(req).await {
                Ok(_) => println!(
                    "{} {} of {} (stored encrypted; it can't be read back)",
                    "Secret set:".green(),
                    field,
                    cid.yellow()
                ),
                Err(e) => err(rpc_err(e)),
            }
        }
        "test" => {
            let Some(id) = id(1) else { return usage(USAGE) };
            match svc
                .test_connector(TestConnectorRequest {
                    id,
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    for s in &r.steps {
                        let mark = if s.ok == Some(true) {
                            "ok  ".green()
                        } else {
                            "FAIL".red()
                        };
                        println!("  {} {:<14} {}", mark, s.name, s.message);
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "remove" | "rm" | "delete" => {
            let Some(id) = id(1) else { return usage(USAGE) };
            match svc
                .delete_connector(DeleteConnectorRequest {
                    id: id.clone(),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => println!("{} {}", "Connector removed:".green(), id),
                Err(e) => err(rpc_err(e)),
            }
        }
        _ => usage(USAGE),
    }
}

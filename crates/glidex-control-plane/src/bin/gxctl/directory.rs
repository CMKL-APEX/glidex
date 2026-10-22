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
    | add google <id> --client-id ID [--name TEXT] (--domain D... [--trust-2sv] | --consumer [--allow-email E]...) [--device-client-id ID] [COMMON] \
    | add jumpcloud <id> --client-id ID [--name TEXT] [--groups-claim C] [--allow-email E]... [COMMON] \
    | update <id> [--name TEXT] [--enable|--disable] [COMMON] \
    | secret set <id> [field] [--from-file PATH] | test <id> | remove <id> [--detach-users] \
    | scim-token list <id> | scim-token create <id> [--name TEXT] [--days N] | scim-token revoke <id> <token-id> \
    | sync <id> [--confirm]\n\
    COMMON: [--jit|--no-jit] [--require-group G]... [--mfa|--no-mfa] [--team GROUP=TEAM]... [--default-tenant T] [--token-max-days N] [--provisioning none|scim|pull] [--group-mode mapped|auto] \
    [--sync-admin EMAIL --sync-group GROUP... [--sync-interval MIN] [--sync-max-disable PCT] [--ssh-keys-field SCHEMA.FIELD]]";

fn kind(c: &Connector) -> &'static str {
    match c.settings {
        Some(Settings::Pam(_)) => "pam",
        Some(Settings::Oidc(_)) => "oidc",
        Some(Settings::Google(_)) => "google",
        Some(Settings::Jumpcloud(_)) => "jumpcloud",
        None => "?",
    }
}

fn print_sync(st: &SyncStatus) {
    let ts = |t: Option<&buffa_types::google::protobuf::Timestamp>| {
        t.map(|t| crate::format_unix_time(t.seconds.max(0) as u64))
            .unwrap_or_else(|| "-".into())
    };
    let state = match st.state.as_known() {
        Some(SyncState::SYNC_STATE_OK) => "ok".green(),
        Some(SyncState::SYNC_STATE_FAILED) => "FAILED".red(),
        Some(SyncState::SYNC_STATE_BLOCKED) => "BLOCKED".red(),
        _ => "?".normal(),
    };
    println!(
        "  Last pull:      {state} at {}",
        ts(st.finished_at.as_option())
    );
    if let Some(ch) = st.changes.as_option() {
        println!(
            "  Changes:        {} created, {} adopted, {} updated, {} disabled, {} enabled",
            ch.created, ch.adopted, ch.updated, ch.disabled, ch.enabled
        );
    }
    if let Some(e) = &st.error {
        println!("  Error:          {e}");
    }
    if !st.would_disable.is_empty() {
        println!(
            "  It would disable {} users: {}; apply it with `connector sync <id> --confirm`",
            st.would_disable.len(),
            st.would_disable.join(", ")
        );
    }
}

fn yes_no(b: Option<bool>) -> &'static str {
    if b == Some(true) { "yes" } else { "no" }
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
    if let Some(sy) = c.sync.as_option() {
        println!(
            "  Pull:           {} as {}, every {} min",
            sy.groups.join(", "),
            sy.admin_email,
            sy.interval_minutes.unwrap_or(15)
        );
    }
    if let Some(st) = c.sync_status.as_option() {
        print_sync(st);
    }
    if c.provisioning.as_deref().is_some_and(|p| p != "none") {
        println!(
            "  Provisioning:   {} (groups {})",
            c.provisioning.as_deref().unwrap_or_default(),
            c.group_mode.as_deref().unwrap_or("mapped")
        );
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
        Some(Settings::Google(g)) => {
            let consumer = g.accounts.as_known() == Some(GoogleAccounts::GOOGLE_ACCOUNTS_ANY);
            println!(
                "  Accounts:       {}",
                if consumer {
                    "any Google account (invited or listed people)"
                } else {
                    "Google Workspace"
                }
            );
            println!("  Client id:      {}", g.client_id);
            if !g.domains.is_empty() {
                println!("  Workspace:      {}", g.domains.join(", "));
            }
            if !g.allowed_emails.is_empty() {
                println!("  Allowed emails: {}", g.allowed_emails.join(", "));
            }
            println!(
                "  Device client:  {}",
                g.device_client_id
                    .as_deref()
                    .unwrap_or("none (no gxctl device login)")
            );
            if g.trust_2sv == Some(true) {
                println!("  2-step:         trusted (its logins count as MFA)");
            }
        }
        Some(Settings::Jumpcloud(j)) => {
            println!("  Client id:      {}", j.client_id);
            println!(
                "  Groups claim:   {}",
                j.groups_claim.as_deref().unwrap_or("memberOf")
            );
            if !j.allowed_emails.is_empty() {
                println!("  Allowed emails: {}", j.allowed_emails.join(", "));
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
    if let Some(p) = flag_value(args, "--provisioning") {
        c.provisioning = Some(p.into());
        paths.push("provisioning");
    }
    if let Some(m) = flag_value(args, "--group-mode") {
        c.group_mode = Some(m.into());
        paths.push("group_mode");
    }
    // A pull's settings (spec/tenancy.md §8.2): any of them sets them all.
    let sync_flags = [
        "--sync-admin",
        "--sync-group",
        "--sync-interval",
        "--sync-max-disable",
        "--ssh-keys-field",
    ];
    if sync_flags.iter().any(|f| has_flag(args, f)) {
        let num = |f: &str| -> Result<Option<u32>, String> {
            flag_value(args, f)
                .map(|v| {
                    v.parse()
                        .map_err(|_| format!("{f} takes a number, not {v}"))
                })
                .transpose()
        };
        c.sync = SyncSettings {
            interval_minutes: num("--sync-interval")?,
            max_disable: num("--sync-max-disable")?,
            admin_email: flag_value(args, "--sync-admin").unwrap_or("").into(),
            groups: strings(args, "--sync-group"),
            ssh_keys_field: flag_value(args, "--ssh-keys-field").map(String::from),
            ..Default::default()
        }
        .into();
        paths.push("sync");
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
                // Workspace by `hd`, or consumer accounts for invited and
                // listed people (spec/tenancy.md D39).
                "google" => {
                    let Some(client_id) = flag_value(args, "--client-id") else {
                        return usage(USAGE);
                    };
                    Some(Settings::Google(Box::new(GoogleSettings {
                        accounts: if has_flag(args, "--consumer") {
                            GoogleAccounts::GOOGLE_ACCOUNTS_ANY
                        } else {
                            GoogleAccounts::GOOGLE_ACCOUNTS_WORKSPACE
                        }
                        .into(),
                        client_id: client_id.into(),
                        domains: strings(args, "--domain"),
                        allowed_emails: strings(args, "--allow-email"),
                        device_client_id: flag_value(args, "--device-client-id").map(Into::into),
                        trust_2sv: has_flag(args, "--trust-2sv").then_some(true),
                        ..Default::default()
                    })))
                }
                "jumpcloud" => {
                    let Some(client_id) = flag_value(args, "--client-id") else {
                        return usage(USAGE);
                    };
                    Some(Settings::Jumpcloud(Box::new(JumpcloudSettings {
                        client_id: client_id.into(),
                        groups_claim: flag_value(args, "--groups-claim").map(Into::into),
                        allowed_emails: strings(args, "--allow-email"),
                        ..Default::default()
                    })))
                }
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
        "sync" => {
            let Some(cid) = id(1) else {
                return usage(USAGE);
            };
            match svc
                .sync_connector(SyncConnectorRequest {
                    id: cid,
                    confirm: has_flag(args, "--confirm").then_some(true),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => print_sync(&r.status.as_option().cloned().unwrap_or_default()),
                Err(e) => err(rpc_err(e)),
            }
        }
        "scim-token" | "scim-tokens" => {
            let ts = |t: Option<&buffa_types::google::protobuf::Timestamp>| {
                t.map(|t| crate::format_unix_time(t.seconds.max(0) as u64))
                    .unwrap_or_else(|| "-".into())
            };
            match (args.get(1).copied(), id(2)) {
                (Some("list" | "ls"), Some(cid)) => match svc
                    .list_scim_tokens(ListScimTokensRequest {
                        id: cid,
                        ..Default::default()
                    })
                    .await
                    .map(|r| r.into_owned())
                {
                    Ok(r) => {
                        println!(
                            "  SCIM endpoint: {} (the web UI also serves it under /api)",
                            r.path
                        );
                        if r.tokens.is_empty() {
                            println!("  No SCIM tokens.");
                        }
                        for t in &r.tokens {
                            println!(
                                "  {}  {:<20} expires {}  by {}",
                                t.id.cyan(),
                                t.name,
                                ts(t.expires_at.as_option()),
                                t.created_by
                            );
                        }
                    }
                    Err(e) => err(rpc_err(e)),
                },
                (Some("create" | "add"), Some(cid)) => {
                    let days = match flag_value(args, "--days").map(str::parse::<u32>) {
                        Some(Ok(d)) => Some(d),
                        Some(Err(_)) => return err("--days takes a number".to_string()),
                        None => None,
                    };
                    match svc
                        .create_scim_token(CreateScimTokenRequest {
                            id: cid,
                            name: flag_value(args, "--name").unwrap_or("").into(),
                            days,
                            ..Default::default()
                        })
                        .await
                        .map(|r| r.into_owned())
                    {
                        Ok(r) => {
                            let t = r.token.as_option().cloned().unwrap_or_default();
                            println!(
                                "{} {} ({}), expires {}",
                                "SCIM token created:".green(),
                                t.id,
                                t.name,
                                ts(t.expires_at.as_option())
                            );
                            println!(
                                "  Endpoint: {} (the web UI also serves it under /api)",
                                r.path
                            );
                            println!(
                                "  Token (shown once; give it to the IdP as its bearer token): {}",
                                Zeroizing::new(r.secret).as_str()
                            );
                        }
                        Err(e) => err(rpc_err(e)),
                    }
                }
                (Some("revoke" | "rm" | "remove"), Some(cid)) => {
                    let Some(tid) = id(3) else {
                        return usage(USAGE);
                    };
                    match svc
                        .revoke_scim_token(RevokeScimTokenRequest {
                            id: cid,
                            token: tid.clone(),
                            ..Default::default()
                        })
                        .await
                    {
                        Ok(_) => println!("{} {}", "SCIM token revoked:".green(), tid),
                        Err(e) => err(rpc_err(e)),
                    }
                }
                _ => usage(USAGE),
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
                    users: has_flag(args, "--detach-users").then(|| "detach".into()),
                    ..Default::default()
                })
                .await
            {
                Ok(r) => {
                    let r = r.into_owned();
                    println!("{} {}", "Connector removed:".green(), id);
                    if r.users > 0 || r.teams > 0 {
                        let what = if has_flag(args, "--detach-users") {
                            "detached, still enabled"
                        } else {
                            "disabled"
                        };
                        println!(
                            "  {} provisioned users {what}; {} teams detached",
                            r.users, r.teams
                        );
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        _ => usage(USAGE),
    }
}

const INVITE_USAGE: &str = "Usage: invite list <tenant> \
    | create <tenant> <email> --connector C [--role member|role.tenant-admin|role.tenant-auditor] [--days N] \
    | revoke <id>";

/// `invite list|create|revoke` (spec/tenancy.md §6.1): an invitation is a
/// link, shown once, that lets one email address in through one connector.
pub async fn invite(client: &ApiClient, args: &[&str]) {
    let Some(t) = transport(client).await else {
        return;
    };
    let svc = InvitationServiceClient::new(t.clone(), t.config());
    match args.first().copied().unwrap_or("") {
        "list" | "ls" => {
            let Some(tenant) = args.get(1) else {
                return usage(INVITE_USAGE);
            };
            let req = ListInvitationsRequest {
                tenant: tenant.to_string(),
                ..Default::default()
            };
            match svc.list_invitations(req).await.map(|r| r.into_owned()) {
                Ok(r) if r.invitations.is_empty() => println!("No invitations"),
                Ok(r) => {
                    for i in &r.invitations {
                        let state = match i.state.as_known() {
                            Some(InvitationState::INVITATION_STATE_OPEN) => "open".green(),
                            Some(InvitationState::INVITATION_STATE_TAKEN) => "taken".normal(),
                            _ => "expired".dimmed(),
                        };
                        println!(
                            "  {}  {}  {}  via {}  {}",
                            i.id.yellow(),
                            i.email,
                            i.role,
                            i.connector_id,
                            state
                        );
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "create" | "add" => {
            let (Some(tenant), Some(email), Some(connector)) = (
                args.get(1).filter(|a| !a.starts_with("--")),
                args.get(2).filter(|a| !a.starts_with("--")),
                flag_value(args, "--connector"),
            ) else {
                return usage(INVITE_USAGE);
            };
            let req = CreateInvitationRequest {
                tenant: tenant.to_string(),
                email: email.to_string(),
                role: flag_value(args, "--role").unwrap_or("member").into(),
                connector: connector.into(),
                days: flag_value(args, "--days").and_then(|d| d.parse().ok()),
                ..Default::default()
            };
            match svc.create_invitation(req).await.map(|r| r.into_owned()) {
                Ok(r) => {
                    let i = r.invitation.as_option().cloned().unwrap_or_default();
                    println!(
                        "{} {} for {}",
                        "Invitation created:".green(),
                        i.id.yellow(),
                        i.email
                    );
                    println!("  Send this link to them; it is shown once:");
                    println!("  {}", r.link.cyan());
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "revoke" | "rm" => {
            let Some(id) = args.get(1) else {
                return usage(INVITE_USAGE);
            };
            let req = RevokeInvitationRequest {
                id: id.to_string(),
                ..Default::default()
            };
            match svc.revoke_invitation(req).await {
                Ok(_) => println!("{} {}", "Invitation revoked:".green(), id.yellow()),
                Err(e) => err(rpc_err(e)),
            }
        }
        _ => usage(INVITE_USAGE),
    }
}

// ---- workload federations (spec/tenancy.md §9) ----------------------------

const FEDERATION_USAGE: &str = "Usage: federation list <tenant> | show <tenant> <id> \
    | add <tenant> <id> --issuer URL --audience AUD [--name TEXT] \
      (--match CLAIM=VALUE... --service-account TOKEN-ID [--ttl SECS] | --rules-file FILE) \
    | enable <tenant> <id> | disable <tenant> <id> | remove <tenant> <id>\n\
    A rule matches exact claim values and names repository, project_path or sub; \
    --rules-file holds a JSON list of {match, service_account, ttl_secs}.";

/// Service accounts the caller can see: token id → `name (project)`. A
/// rule names its service account by its public token id; this shows
/// which one that is.
async fn service_accounts(t: &crate::client::RpcTransport) -> HashMap<String, String> {
    use glidex_api::proto::glidex::access::v1::{ListTokensRequest, TokenServiceClient};
    let svc = TokenServiceClient::new(t.clone(), t.config());
    match svc.list_tokens(ListTokensRequest::default()).await {
        Ok(r) => r
            .into_owned()
            .tokens
            .into_iter()
            .map(|k| {
                let label = match k.project.as_deref() {
                    Some(p) if !p.is_empty() => format!("{} (project {p})", k.name),
                    _ => k.name.clone(),
                };
                (k.id, label)
            })
            .collect(),
        Err(_) => HashMap::new(),
    }
}

fn print_federation(f: &Federation, accounts: &HashMap<String, String>) {
    let state = if f.enabled == Some(true) {
        "enabled".green()
    } else {
        "disabled".red()
    };
    println!("  {} {}  {}", f.id.cyan(), f.display_name, state);
    println!("    issuer {}  audience {}", f.issuer, f.audience);
    for (i, r) in f.rules.iter().enumerate() {
        let mut claims: Vec<String> = r.r#match.iter().map(|(k, v)| format!("{k}={v}")).collect();
        claims.sort();
        let who = accounts
            .get(&r.service_account)
            .map(String::as_str)
            .unwrap_or("a service account you can't list");
        println!(
            "    rule {}: {} -> {} ({} s)",
            i + 1,
            claims.join(", "),
            who,
            r.ttl_secs.unwrap_or(900)
        );
    }
}

fn rules_from(args: &[&str]) -> Result<Vec<FederationRule>, String> {
    if let Some(path) = flag_value(args, "--rules-file") {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let v: Vec<serde_json::Value> =
            serde_json::from_str(&text).map_err(|e| format!("{path}: not a JSON list: {e}"))?;
        return v
            .into_iter()
            .map(|r| {
                let claims = r
                    .get("match")
                    .and_then(|m| m.as_object())
                    .ok_or_else(|| format!("{path}: a rule without match"))?;
                Ok(FederationRule {
                    r#match: claims
                        .iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect(),
                    service_account: r
                        .get("service_account")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .into(),
                    ttl_secs: r.get("ttl_secs").and_then(|t| t.as_u64()).map(|t| t as u32),
                    ..Default::default()
                })
            })
            .collect();
    }
    let matches = flag_values(args, "--match");
    if matches.is_empty() {
        return Ok(Vec::new());
    }
    let mut m = HashMap::new();
    for kv in matches {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("--match takes CLAIM=VALUE, not {kv}"))?;
        m.insert(k.to_string(), v.to_string());
    }
    let sa =
        flag_value(args, "--service-account").ok_or("--match needs --service-account TOKEN-ID")?;
    let ttl = match flag_value(args, "--ttl") {
        Some(t) => Some(
            t.parse()
                .map_err(|_| format!("--ttl takes seconds, not {t}"))?,
        ),
        None => None,
    };
    Ok(vec![FederationRule {
        r#match: m.into_iter().collect(),
        service_account: sa.into(),
        ttl_secs: ttl,
        ..Default::default()
    }])
}

pub async fn federation(client: &ApiClient, args: &[&str]) {
    let Some(t) = transport(client).await else {
        return;
    };
    let svc = FederationServiceClient::new(t.clone(), t.config());
    let (Some(sub), Some(tenant)) = (args.first().copied(), args.get(1).map(|s| s.to_string()))
    else {
        return usage(FEDERATION_USAGE);
    };
    let id = args.get(2).map(|s| s.to_string());
    match (sub, id) {
        ("list" | "ls", _) => match svc
            .list_federations(ListFederationsRequest {
                tenant,
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned())
        {
            Ok(r) if r.federations.is_empty() => println!("No federations"),
            Ok(r) => {
                let accounts = service_accounts(&t).await;
                r.federations
                    .iter()
                    .for_each(|f| print_federation(f, &accounts));
            }
            Err(e) => err(rpc_err(e)),
        },
        ("show", Some(id)) => match svc
            .get_federation(GetFederationRequest {
                tenant,
                federation: id,
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned())
        {
            Ok(r) => {
                let accounts = service_accounts(&t).await;
                print_federation(
                    &r.federation.as_option().cloned().unwrap_or_default(),
                    &accounts,
                )
            }
            Err(e) => err(rpc_err(e)),
        },
        ("add" | "create", Some(id)) => {
            let (Some(issuer), Some(audience)) =
                (flag_value(args, "--issuer"), flag_value(args, "--audience"))
            else {
                return usage(FEDERATION_USAGE);
            };
            let rules = match rules_from(args) {
                Ok(r) => r,
                Err(e) => return err(e),
            };
            let f = Federation {
                id,
                display_name: flag_value(args, "--name").unwrap_or("").into(),
                issuer: issuer.into(),
                audience: audience.into(),
                enabled: Some(true),
                rules,
                ..Default::default()
            };
            match svc
                .create_federation(CreateFederationRequest {
                    tenant,
                    federation: f.into(),
                    ..Default::default()
                })
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    println!("{}", "Federation added:".green());
                    let accounts = service_accounts(&t).await;
                    print_federation(
                        &r.federation.as_option().cloned().unwrap_or_default(),
                        &accounts,
                    );
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        (on @ ("enable" | "disable"), Some(id)) => {
            let req = UpdateFederationRequest {
                tenant,
                federation: id.clone(),
                changes: Federation {
                    enabled: Some(on == "enable"),
                    ..Default::default()
                }
                .into(),
                update_mask: buffa_types::google::protobuf::FieldMask {
                    paths: vec!["enabled".into()],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            };
            match svc.update_federation(req).await {
                Ok(_) => println!("{} {id} {on}d", "Federation".green()),
                Err(e) => err(rpc_err(e)),
            }
        }
        ("remove" | "rm" | "delete", Some(id)) => match svc
            .delete_federation(DeleteFederationRequest {
                tenant,
                federation: id.clone(),
                ..Default::default()
            })
            .await
        {
            Ok(_) => println!("{} {id}", "Federation removed:".green()),
            Err(e) => err(rpc_err(e)),
        },
        _ => usage(FEDERATION_USAGE),
    }
}

fn print_key(k: &SigningKey) {
    let ts = |t: Option<&buffa_types::google::protobuf::Timestamp>| {
        t.map(|t| crate::format_unix_time(t.seconds.max(0) as u64))
            .unwrap_or_else(|| "-".into())
    };
    let state = if k.active {
        "active".green().to_string()
    } else {
        format!(
            "retired {}, published until {}",
            ts(k.retired_at.as_option()),
            ts(k.published_until.as_option())
        )
    };
    println!(
        "  {}  {}  made {}  {state}",
        k.kid.cyan(),
        k.alg,
        ts(k.created_at.as_option())
    );
}

/// `directory keys [rotate]` (spec/tenancy.md D52, D53): the directory's
/// issuer URL and token signing keys, on `SigningKeyService`.
pub async fn directory(client: &ApiClient, args: &[&str]) {
    let Some(t) = transport(client).await else {
        return;
    };
    let svc = SigningKeyServiceClient::new(t.clone(), t.config());
    match (args.first().copied(), args.get(1).copied()) {
        (Some("keys" | "key"), None | Some("list" | "ls")) => match svc
            .list_signing_keys(ListSigningKeysRequest::default())
            .await
            .map(|r| r.into_owned())
        {
            Ok(r) => {
                let fixed = if r.issuer_fixed {
                    ""
                } else {
                    " (fixed when it first issues a token)"
                };
                match r.issuer.as_str() {
                    "" => println!("  Issuer: none yet; set auth.issuer_base"),
                    i => println!("  Issuer: {i}{fixed}"),
                }
                if r.keys.is_empty() {
                    println!("  No signing keys yet: the leader makes the first within minutes.");
                }
                for k in &r.keys {
                    print_key(k);
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        (Some("keys" | "key"), Some("rotate")) => match svc
            .rotate_signing_key(RotateSigningKeyRequest::default())
            .await
            .map(|r| r.into_owned())
        {
            Ok(r) => {
                if let Some(k) = r.key.as_option() {
                    println!(
                        "  New signing key; the one before it stays published until its tokens expire."
                    );
                    print_key(k);
                }
            }
            Err(e) => err(rpc_err(e)),
        },
        (Some("list" | "ls") | None, _) => {
            let svc = glidex_api::proto::glidex::tenancy::v1::DirectoryServiceClient::new(
                t.clone(),
                t.config(),
            );
            match svc
                .list_directories(
                    glidex_api::proto::glidex::tenancy::v1::ListDirectoriesRequest::default(),
                )
                .await
                .map(|r| r.into_owned())
            {
                Ok(r) => {
                    for d in &r.directories {
                        println!(
                            "  {:<20} {:<24} {}  {}",
                            d.id.cyan(),
                            d.name,
                            d.issuer,
                            d.trust_domain.dimmed()
                        );
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        (Some("create" | "add"), Some(id)) => {
            let Some(email) = flag_value(args, "--admin-email") else {
                return usage(
                    "directory create <id> --admin-email EMAIL [--name TEXT] [--owner SLUG] [--days N] [--trust-domain NAME]",
                );
            };
            let svc = glidex_api::proto::glidex::tenancy::v1::DirectoryServiceClient::new(
                t.clone(),
                t.config(),
            );
            let req = glidex_api::proto::glidex::tenancy::v1::CreateDirectoryRequest {
                id: id.to_string(),
                name: flag_value(args, "--name").unwrap_or("").into(),
                owner: flag_value(args, "--owner")
                    .map(String::from)
                    .unwrap_or_else(|| format!("{id}-admin")),
                admin_email: email.into(),
                days: flag_value(args, "--days").and_then(|d| d.parse().ok()),
                trust_domain: flag_value(args, "--trust-domain").map(String::from),
                ..Default::default()
            };
            match svc.create_directory(req).await.map(|r| r.into_owned()) {
                Ok(r) => {
                    if let Some(d) = r.directory.as_option() {
                        println!(
                            "  Directory {} created; its issuer is {}, its trust domain {}",
                            d.id.cyan(),
                            d.issuer,
                            d.trust_domain
                        );
                    }
                    println!(
                        "  Send this invitation to {email} (shown once): they sign in with a passkey"
                    );
                    println!("    {}", r.invitation_link.yellow());
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        (Some("delete" | "remove" | "rm"), Some(id)) => {
            let svc = glidex_api::proto::glidex::tenancy::v1::DirectoryServiceClient::new(
                t.clone(),
                t.config(),
            );
            match svc
                .delete_directory(
                    glidex_api::proto::glidex::tenancy::v1::DeleteDirectoryRequest {
                        id: id.to_string(),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => println!("  Directory {id} deleted"),
                Err(e) => err(rpc_err(e)),
            }
        }
        _ => usage(
            "directory list | create <id> --admin-email EMAIL [--name TEXT] [--owner SLUG] [--days N] [--trust-domain NAME] | delete <id> | keys [rotate]",
        ),
    }
}

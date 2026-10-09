//! Identity, tenancy and policy commands (spec/security.md §5, §6, §7;
//! spec/cli.md "Access control").

use crate::client::{self, enc, format_entity, parse_principal, parse_role_ref, role_name, ApiClient};
use crate::config;
use crate::{flag_value, flag_values, format_unix_time, has_flag, project_names, prompt, prompt_hidden};
use colored::Colorize;
use hyper::{Method, StatusCode};
use serde_json::{json, Value};
use std::io::IsTerminal;
use std::path::PathBuf;
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
    device_login_named(client, "gxctl", None, None).await
}

/// The same flow for the login family, with the token named for its
/// profile and the `device`/`client` metadata stamped so `auth token list`
/// and the audit trail say which machine and which gxctl asked (A8).
async fn device_login_named(client: &ApiClient, token_name: &str, device: Option<&str>, client_id: Option<&str>) -> Option<Zeroizing<String>> {
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
    let mut body = json!({ "device_code": code, "token_name": token_name });
    if let Some(d) = device {
        body["device"] = json!(d);
    }
    if let Some(c) = client_id {
        body["client"] = json!(c);
    }
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

// ---- auth: login profiles (spec/gxctl-auth.md §6) ---------------------------

/// `auth login|logout|status|use|profiles|token|trust|whoami` — the
/// profile generator and manager of gxctl-auth.md §6. The legacy
/// `login`/`logout`/`whoami`/`token` commands above stay as aliases for
/// what they were; this family adds the file of named profiles.
pub async fn auth(client: &ApiClient, args: &[&str]) {
    match args.first().copied().unwrap_or("") {
        "login" => auth_login(client, &args[1..]).await,
        "logout" => auth_logout(client, &args[1..]).await,
        "status" => auth_status(client, &args[1..]).await,
        "use" => auth_use(&args[1..]),
        "profiles" => auth_profiles(&args[1..]),
        "trust" => auth_trust(&args[1..]).await,
        "token" | "tokens" => token(client, &args[1..]).await,
        "whoami" => whoami(client).await,
        _ => usage("Usage: auth login [--url U]… [--profile P] [--oidc | --token | --pam-user U] [--pin sha256/…] [--insecure] [--rebind] [--use] [--name N] [--days N] | logout [--revoke] [--keep-token] | status | use P | profiles [--all] | trust list|fetch|remove [--all] | token … | whoami"),
    }
}

/// `auth trust fetch` asks an unverified bootstrap connection what the
/// server says its own fingerprint is, so the TOFU prompt can show whether
/// the certificate we were handed agrees with what the server claims
/// (§5.2): a proxy terminating TLS in front presents its own certificate,
/// the ladder rejects it, and the two numbers disagree. The probe carries
/// no credential — server-info is public information, the answer to be
/// compared before any token exists on that connection.
async fn tofu_verdict(urls: &[String]) -> String {
    let trust = config::Tls { insecure: true, ..Default::default() };
    match ApiClient::tcp_multi(urls, None, Some(&trust), None, None) {
        Ok(c) => match c.server_info().await {
            Ok(v) => match v["fingerprint"].as_str() {
                Some(claim) => {
                    let seen = c.accepted_fingerprint();
                    let same = match (glidex_tls::Trust::normalize_pin(claim), seen.as_deref()) {
                        (Some(c), Some(s)) => glidex_tls::Trust::normalize_pin(s).is_some_and(|n| n.as_str() == c.as_str()),
                        _ => false,
                    };
                    if same {
                        format!("it matches what we are being handed ({})", claim)
                    } else {
                        format!("IT DOES NOT MATCH: the server says {claim}, the certificate in front of it is {}", seen.unwrap_or_else(|| "?".into()))
                    }
                }
                None => "the server did not say (an older control plane?)".into(),
            },
            Err(_) => "the server did not answer the unverified probe".into(),
        },
        Err(_) => "the unverified probe could not connect".into(),
    }
}

async fn auth_login(session: &ApiClient, args: &[&str]) {
    let mut urls: Vec<String> = flag_values(args, "--url").into_iter().chain(flag_values(args, "--server").into_iter()).map(|u| (*u).to_string()).collect();
    let mut name = flag_value(args, "--profile").map(|s| s.to_string());
    let rebind = has_flag(args, "--rebind");
    let non_interactive = has_flag(args, "--non-interactive") || !std::io::stdin().is_terminal();
    let pin_arg = flag_value(args, "--pin").map(|s| s.to_string());
    let pam_user = flag_value(args, "--pam-user").map(|s| s.to_string());

    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let known = || cfg.as_ref().map(|c| c.list()).unwrap_or_else(|| "no profiles yet".into());

    // §6.1.1 — target and profile: a named profile, the `--url` target, or
    // the live current one. With nothing to go on, ask which server, never
    // guess.
    if name.is_none() && urls.is_empty() {
        match config::profile_of(&cfg, None) {
            Some((n, p)) => {
                name = Some(n.clone());
                urls = p.url.clone();
            }
            None => return usage(&format!("which server? give --url https://host:8841 or --profile P; the file knows: {}", known())),
        }
    }
    let pair: Option<(&String, &config::Profile)> = if let Some(n) = &name {
        match config::profile_of(&cfg, Some(n)) {
            Some(p) => Some(p),
            None if !urls.is_empty() => None, // a new profile, named, dialling the given target
            None => return err(format!("profile '{n}' does not exist yet and no --url was given; there is nothing to log in to")),
        }
    } else if !urls.is_empty() {
        // `--url` without `--profile`: refresh the profile that already
        // dials exactly this host; anything else is a new profile and the
        // operator must say so with --save (no defaults are guessed,
        // §6.1.1).
        match cfg.as_ref().and_then(|c| c.profiles.iter().find(|(_, p)| urls.iter().all(|u| p.url.iter().any(|pu| config::url_host(pu) == config::url_host(u))))) {
            Some(p) => Some(p),
            None => {
                if !has_flag(args, "--save") {
                    return usage("which profile? this target is new to the file: pass --profile NAME, or --save to record it as a new profile named for its host");
                }
                let host = config::url_host(&urls[0]);
                let mut n: String = host.chars().take_while(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-')).collect();
                n = n.trim_matches('-').to_string();
                if n.is_empty() {
                    n = host;
                }
                name = Some(n);
                None
            }
        }
    } else {
        config::profile_of(&cfg, None)
    };
    let existing = pair.map(|(k, p)| (k.clone(), p.clone()));
    let name = match &name {
        Some(n) => n.clone(),
        None => return err("no profile in effect; pass --profile"),
    };
    if urls.is_empty() {
        urls = match pair {
            Some((_, p)) => p.url.clone(),
            None => return err(format!("profile '{name}' is new and lists no urls; give --url https://host:8841")),
        };
    }

    // Trust carries over a refresh untouched (§6.2: profile, pins and CA
    // stay); --rebind wipes the pins because a rotating-ca or re-init
    // legitimately changes them, and the TOFU path below re-establishes.
    let mut tls = existing.as_ref().map(|(_, p)| p.tls.clone()).unwrap_or_default();
    if rebind {
        tls.pins.clear();
        tls.insecure = false;
        tls.i_understand = None;
    }
    if let Some(p) = &pin_arg {
        if glidex_tls::Trust::normalize_pin(p).is_none() {
            return err(format!("{p} is not a fingerprint; gxctl prints them as sha256/AB:CD:… — paste that line"));
        }
        tls.pins = vec![p.clone()];
    }
    let host = config::url_host(&urls[0]);
    if has_flag(args, "--insecure") {
        // The bypass exists only as its typed confirmation (§5.4) — and a
        // confirmation cannot be typed into a pipe: CI passes --pin, or the
        // one-run GLIDEX_TLS_INSECURE=1 for throwaway rigs, never this.
        if non_interactive {
            return err("--insecure needs a typed confirmation and stdin is not a terminal; trust the exact certificate instead: --pin sha256/<fingerprint>");
        }
        println!("{}", "The connection will verify NO certificate: anyone on the path can read your token and everything you send. This is for hardware labs whose switches cannot present a certificate at all.".red().bold());
        let typed = prompt(&format!("type '{host}' to accept unverified TLS for this profile: "));
        if typed.trim() != host {
            return err("confirmation not typed — nothing is saved and nothing is unverified");
        }
        tls.insecure = true;
        tls.i_understand = Some(host.clone());
    }

    // §6.1.2 connect, through the ladder; nothing is authenticated yet, so
    // the token cannot leak on a rejected certificate — there is no token
    // on this connection at all.
    let mut asked = false;
    let mut reached: Option<(ApiClient, Value)> = None;
    // Bounded: the TOFU prompt runs at most once; a second rejection means
    // the fresh pin does not fit and there is nothing left to ask.
    for _ in 0..2 {
        let c = match ApiClient::tcp_multi(&urls, None, Some(&tls), None, None) {
            Ok(c) => c,
            Err(e) => return err(e),
        };
        match c.server_info().await {
            Ok(v) => {
                reached = Some((c, v));
                break;
            }
            Err(e) => match c.rejected_fingerprint() {
                // The server refused us for some reason that is not the
                // certificate (down, refused, timed out): say it plainly.
                None => return err(e),
                Some(fp) => {
                    if !tls.pins.is_empty() && !asked {
                        // §5.3: a pinned certificate that changed is a hard
                        // error — never a re-prompt.
                        return err(format!(
                            "the certificate of {host} is SHA-256 {fp}, which is not among profile '{name}'s pins ({}).\n  the control plane regenerates its certificate only in narrow cases ('cluster rotate-ca', a re-init): confirm the change through a second channel, then\n  gxctl auth login --profile {name} --pin sha256/<new fingerprint> --rebind",
                            tls.pins.join(", ")
                        ));
                    }
                    if asked || pin_arg.is_some() {
                        return err(format!("the pin did not rescue the connection: {e}"));
                    }
                    if non_interactive {
                        // A TTY-less login never silently pins (§5.2): the
                        // operator must have seen the fingerprint somewhere.
                        return err(format!(
                            "{e}\n  if that is the control plane's certificate, trust this exact one:\n  gxctl auth login --profile {name} --url '{}' --pin sha256/{}",
                            urls[0],
                            fp.to_lowercase()
                        ));
                    }
                    let verdict = tofu_verdict(&urls).await;
                    println!("the certificate of {host} is not trusted by this system:");
                    println!("  SHA-256 {fp}   (the control plane prints this line at startup)");
                    println!("  reported by the server as its own: {verdict}");
                    if !verdict.starts_with("it matches") {
                        println!("{}", "  the two do not agree — check with the operator of the other channel before trusting this".red());
                    }
                    if !prompt(&format!("Trust this exact certificate for profile '{name}'? [y/N]: ")).to_lowercase().starts_with('y') {
                        return err("the certificate was not trusted; nothing was saved and no token was requested");
                    }
                    tls.pins = vec![glidex_tls::Trust::normalize_pin(&fp).unwrap_or(fp.to_lowercase())];
                    asked = true;
                }
            },
        }
    }
    let (cl, info) = match reached {
        Some(x) => x,
        None => return err(format!("{host} never answered over a trusted connection")),
    };

    // §6.1.3 the cluster binding: the profile answers for one cluster,
    // and this says whether the machine in front does too.
    let claimed = info["cluster_id"].as_str().map(str::to_string);
    let expect = existing.as_ref().and_then(|(_, p)| p.cluster_id.clone());
    if let (Some(want), Some(got)) = (&expect, &claimed) {
        if want != got {
            if !rebind {
                return err(format!(
                    "refusing to log in to a machine that is not the cluster this profile knows: profile '{name}' is bound to cluster {want}, but {} answers for {got}.\n  a machine was repurposed, or the profile is stale.\n  if you mean it: re-run with --rebind",
                    cl.describe()
                ));
            }
            println!("{} rebinding profile '{name}':", "Warning:".yellow());
            println!("   cluster was {want}, now {got}");
            println!("   pins were {} (the server now reports {})", if tls.pins.is_empty() { "(none left by --rebind)".into() } else { tls.pins.join(", ") }, info["fingerprint"].as_str().unwrap_or("?"));
        }
    } else if expect.is_some() && claimed.is_none() && !rebind {
        println!("{} the server does not report a cluster id (pre-cluster or older build); nothing new is bound", "Note:".dimmed());
    }
    if has_flag(args, "--oidc") && info["methods"]["oidc"] == json!(false) {
        return err("this control plane has OIDC logins off: set auth.oidc in control-plane.json, or log in with --pam-user / a piped --token");
    }

    // §6.1.4 prove the credential. The token name says which profile minted
    // it, so `auth token list` on the server reads as an audit trail.
    let token_name = flag_value(args, "--name").map(|s| s.to_string()).unwrap_or_else(|| format!("gxctl@{name}"));
    let device_tag = glidex_tls::hostname().unwrap_or_else(|| "device".to_string());
    let client_tag = format!("gxctl/{}", env!("CARGO_PKG_VERSION"));
    let (credential, method_hint) = if has_flag(args, "--oidc") {
        match device_login_named(&cl, &token_name, Some(&device_tag), Some(&client_tag)).await {
            Some(t) => (t, "oidc"),
            None => return,
        }
    } else if let Some(user) = &pam_user {
        // The password goes to the prompt, never to argv or the environment;
        // a pipe gets no password prompt at all (§6.1.7) — CI pipes tokens,
        // not passwords.
        if non_interactive {
            return err("--pam-user needs a terminal for the hidden password (CI: pipe an existing token to 'auth login --token' on a trusted rig)");
        }
        let pw = prompt_hidden(&format!("Password for {user}@{host}: "));
        if pw.trim().is_empty() {
            return err("no password given");
        }
        let mut body = json!({ "username": user, "password": pw.trim().to_string(), "token_name": token_name, "device": device_tag, "client": client_tag });
        if let Some(d) = flag_value(args, "--days").and_then(|d| d.parse::<u64>().ok()) {
            body["days"] = json!(d);
        }
        match cl.send(Method::POST, "/auth/token", Some(&body)).await {
            Ok(r) if r.status.is_success() => {
                let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                match v["token"].as_str() {
                    Some(t) => (Zeroizing::new(t.to_string()), "pam"),
                    None => {
                        err("the control plane returned no token");
                        return;
                    }
                }
            }
            Ok(r) => {
                err(client::render_error(r.status, &r.body, true));
                return;
            }
            Err(e) => {
                err(e);
                return;
            }
        }
    } else if has_flag(args, "--token") {
        // One read from stdin, TTY or not (§6.1.7): scripted logins pipe the
        // token, interactive ones type it hidden.
        let raw = if std::io::stdin().is_terminal() {
            prompt_hidden("Access token (input hidden): ")
        } else {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            line
        };
        let t = raw.trim();
        if t.is_empty() {
            return err("no token given");
        }
        (Zeroizing::new(t.to_string()), "token")
    } else if let Some((_, p)) = &existing {
        // Silent reuse (§6.1.4): the profile already had a credential and
        // the server just confirmed it answers for the same cluster — a
        // re-login is not a reason to mint a second token.
        match p.credential() {
            Ok(Some(t)) => (t, p.identity.as_ref().and_then(|i| i.method.as_deref()).unwrap_or("token")),
            // §3.5: logging in to the implicit localhost keeps working —
            // the legacy token file is that profile's credential, folded in
            // as a copy the moment the profile is written.
            Ok(None) if urls.iter().all(|u| client::is_loopback_host(&config::url_host(u))) => match config::legacy_token() {
                Ok(Some(t)) => (t, "token"),
                Ok(None) => return usage(&format!("profile '{name}' has no credential: --oidc, --token (piped or prompted) or --pam-user USER")),
                Err(e) => return err(e),
            },
            Ok(None) => return usage(&format!("profile '{name}' has no credential: --oidc, --token (piped or prompted) or --pam-user USER")),
            Err(e) => return err(e),
        }
    } else {
        return usage("give a credential: --oidc, --token (piped or prompted), or --pam-user USER");
    };
    let _ = method_hint;

    // §6.1.5 check it before anything is written — whoami on the answered,
    // verified connection, with the candidate as its only secret.
    cl.set_token(Some(credential.clone()));
    let w = match cl.request_json::<Value>(Method::GET, "/auth/whoami", None).await {
        Ok(w) => w,
        Err(e) => return err(format!("the credential was not accepted: {e}")),
    };
    if let (Some(claim), Some(seen)) = (info["fingerprint"].as_str(), cl.accepted_fingerprint()) {
        let agrees = glidex_tls::Trust::normalize_pin(claim).is_some_and(|c| glidex_tls::Trust::normalize_pin(&seen).is_some_and(|s| s == c));
        if !agrees {
            return err(format!("the answer came over a connection presenting a different certificate than the server claims as its own (we saw {seen}, the server says {claim}) — something is terminating TLS in front of {}", cl.describe()));
        }
    }
    // Display-only metadata, copied from the checked answer (§2.2): the
    // server stamped and sanitized it at creation; gxctl never parses it.
    let identity = config::Identity {
        method: w["method"].as_str().map(str::to_string),
        user: w["user"]["display_name"].as_str().map(str::to_string),
        token_id: w["token"]["id"].as_str().map(str::to_string),
        token_name: w["token"]["name"].as_str().map(str::to_string),
        logged_in_at: Some(client::now_secs()),
    };
    let who = w["user"]["display_name"].as_str().unwrap_or("service account").to_string();

    // §6.1.6 merge-write the file (never clobber what the callback does
    // not touch); the token goes out of here into the 0600 file only.
    let is_new = existing.is_none();
    let use_now = has_flag(args, "--use");
    let claimed_c = claimed.clone();
    let server_name = info["cluster_name"].as_str().map(str::to_string);
    let urls_w = urls.clone();
    let tls_w = tls.clone();
    let identity_w = identity.clone();
    let secret = credential.as_str().to_string();
    let name_w = name.clone();
    let mut made_current = false;
    if let Err(e) = config::edit(cfg, |c| {
        let mut p = c.profiles.get(&name_w).cloned().unwrap_or_default();
        p.url = urls_w;
        p.cluster_id = claimed_c;
        p.cluster_name = server_name;
        p.tls = tls_w;
        p.token = Some(secret);
        // token_command is left exactly as it was: a site-wide helper
        // outranks this stored copy at request time (§2.2), and logging in
        // with a flag does not delete the site's answer.
        p.identity = Some(identity_w);
        if is_new {
            p.added_at = Some(client::now_secs());
        }
        if use_now || c.current.is_none() {
            // --use, or being the file's first profile: something must be
            // current for the next bare invocation to find it (§3.2).
            c.current = Some(name_w.clone());
            made_current = true;
        }
        c.profiles.insert(name_w.clone(), p);
    }) {
        return err(e);
    }
    let path = config::file().unwrap_or_else(|| PathBuf::from("(nowhere)"));
    println!("{} logged in as {} to {}", "OK:".green(), who.yellow(), cl.describe());
    println!("{} token saved to {} (mode 0600) — profile '{name}', anyone who can read that file can act as {who}", "Note:".yellow(), path.display());
    if made_current {
        println!("{} profile '{name}' is now current", "Info:".dimmed());
    }
    if session.profile_name().is_some_and(|n| n == name) {
        println!("{} this session keeps the credential it started with; start gxctl again to pick up the saved one", "Note:".dimmed());
    }
}

async fn auth_logout(client: &ApiClient, args: &[&str]) {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let want = flag_value(args, "--profile");
    let name = match config::profile_of(&cfg, want) {
        Some((n, _)) => n.clone(),
        None => {
            println!("{}", if want.is_some() { format!("no profile '{}' in the file", want.unwrap_or("")) } else { "no profile is in effect here (log in first: gxctl auth login --url … --profile P --save)".into() });
            return;
        }
    };
    let token_id = config::profile_of(&cfg, want).and_then(|(_, p)| p.identity.as_ref()).and_then(|i| i.token_id.clone());
    if has_flag(args, "--revoke") {
        let id = match &token_id {
            Some(id) => id.clone(),
            None => return err(format!("profile '{name}' has no server-side token id on record — nothing to revoke; the credential can still be struck from the server's side ('token revoke', or the admin)")),
        };
        // The delete is signed as the token itself; the server allows a
        // principal to strike its own token. A failure here aborts the
        // logout: clearing the file while the token lives would hide a
        // working credential, not remove one.
        if let Err(e) = client.request_json::<()>(Method::DELETE, &format!("/tokens/{}", enc(id.as_str())), None).await {
            return err(format!("the server did not revoke the token: {e}"));
        }
        println!("{} token {id} revoked on {}", "OK:".green(), client.describe());
    }
    if has_flag(args, "--keep-token") {
        println!("the profile keeps its credential (--keep-token); this session ends with the process");
        return;
    }
    if let Err(e) = config::edit(cfg, |c| {
        if let Some(p) = c.profiles.get_mut(&name) {
            p.token = None;
        }
    }) {
        return err(e);
    }
    println!("{} profile '{name}': credential removed (the target, pins and CA stay)", "OK:".green());
}

async fn auth_status(client: &ApiClient, args: &[&str]) {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    // The profile this session runs under if one does; else the one asked
    // for; else the file's current (§6.2).
    let want = flag_value(args, "--profile");
    let pair = config::profile_of(&cfg, want).or_else(|| match client.profile_name() { Some(n) => config::profile_of(&cfg, Some(&n)), None => None });
    let (name, p) = match pair {
        Some(x) => x,
        None => return println!("no profiles yet — log in: gxctl auth login --url https://host:8841 --profile P --save"),
    };
    let current = cfg.as_ref().and_then(|c| c.current.as_deref()).is_some_and(|c| c == name);
    println!("{} profile '{name}'{}", "Profile:".dimmed(), if current { " (current)" } else { "" });
    println!("   dials:  {}", p.url.join(", "));
    println!("   cluster: {}", match (&p.cluster_name, &p.cluster_id) {
        (Some(n), Some(id)) => format!("{n} ({id})"),
        (None, Some(id)) => id.clone(),
        (Some(n), None) => format!("{n} (never verified)"),
        (None, None) => "never verified".into(),
    });
    let mut trust = p.trust_mode();
    if p.tls.insecure {
        trust.push_str("  — this profile verifies NOTHING (typed confirmation)");
    }
    println!("   trust:  {trust}");
    match &p.identity {
        Some(i) => println!(
            "   token:  {} ({}){}",
            i.token_name.as_deref().unwrap_or("?"),
            i.user.as_deref().or(i.method.as_deref()).unwrap_or("?"),
            i.logged_in_at.map(|t| format!(", since {}", format_unix_time(t))).unwrap_or_default()
        ),
        None => println!("   token:  (none — run 'auth login --profile {name}')"),
    }
    if let Some(lu) = &p.last_used {
        println!("   used:   {}{}", lu.url.as_deref().unwrap_or("?"), lu.at.map(|t| format!(" at {}", format_unix_time(t))).unwrap_or_default());
    }
    if !client.is_unix() && client.profile_name().is_some() {
        match client.server_info().await {
            Ok(v) => println!(
                "   server: {} {} reachable (pam {}, oidc {})",
                v["cluster_name"].as_str().unwrap_or("?"),
                v["version"].as_str().unwrap_or("?"),
                if v["methods"]["pam"] == json!(true) { "on" } else { "off" },
                if v["methods"]["oidc"] == json!(true) { "on" } else { "off" }
            ),
            Err(e) => println!("   server: not reachable now ({e})"),
        }
    }
}

fn auth_use(args: &[&str]) {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    if cfg.as_ref().is_none_or(|c| !c.has_any()) {
        return println!("there is no config file yet (log in first: gxctl auth login --url … --profile P --save)");
    }
    let Some(target) = args.first() else {
        return usage("Usage: auth use <profile|->   ('-' clears the current profile for this user)");
    };
    let target = target.to_string();
    if target == "-" {
        if let Err(e) = config::edit(cfg, |c| c.current = None) {
            return err(e);
        }
        return println!("no profile is current now; gxctl asks for --profile when it needs a target");
    }
    if config::profile_of(&cfg, Some(&target)).is_none() {
        let list = cfg.as_ref().map(|c| c.list()).unwrap_or_else(|| "none".into());
        return err(format!("unknown profile '{target}'; the file knows: {list}"));
    }
    if let Err(e) = config::edit(cfg, |c| c.current = Some(target.clone())) {
        return err(e);
    }
    println!("{} profile '{target}' is current; bare invocations use its target, trust and token unless --profile says otherwise", "OK:".green());
}

fn auth_profiles(args: &[&str]) {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let Some(c) = cfg.as_ref() else {
        return println!("no config file yet (log in first: gxctl auth login --url … --profile P --save)");
    };
    if c.profiles.is_empty() {
        return println!("the file exists but holds no profiles (log in: gxctl auth login --url … --profile P --save)");
    }
    let all = has_flag(args, "--all");
    let current = c.current.as_deref();
    println!("{:<2} {:<16} {:<38} {:<22} TOKEN", "", "PROFILE", "TARGET", "TRUST");
    for (n, p) in &c.profiles {
        let is_cur = current.is_some_and(|x| *x == *n);
        if !all && !is_cur {
            continue;
        }
        let mut target = p.url.first().cloned().unwrap_or_else(|| "(no url)".into());
        if p.url.len() > 1 {
            target.push_str(&format!(" (+{})", p.url.len() - 1));
        }
        println!(
            "{:<2} {:<16} {:<38} {:<22} {}",
            if is_cur { "*" } else { "" },
            n,
            target,
            p.trust_mode(),
            p.identity.as_ref().and_then(|i| i.token_name.as_deref()).unwrap_or("-")
        );
    }
    if !all && c.profiles.len() > 1 {
        println!("{}", "run 'auth profiles --all' to see every profile in the file".dimmed());
    }
}

async fn auth_trust(args: &[&str]) {
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let want = flag_value(args, "--profile");
    let Some((name, p)) = config::profile_of(&cfg, want) else {
        return err("no profile to manage: name one with --profile or log one in");
    };
    let name = name.clone();
    let p = p.clone();
    match args.get(1).copied().unwrap_or("list") {
        "list" => {
            if p.tls.pins.is_empty() {
                println!("profile '{name}': no pins; the system store and the profile's CA decide");
            }
            for pin in &p.tls.pins {
                println!("  pin {pin}");
            }
            if let Some(f) = &p.tls.ca_file {
                println!("  ca file {}", f.display());
            }
            if p.tls.ca_pem.is_some() {
                println!("  ca pem (embedded, {} bytes)", p.tls.ca_pem.as_ref().map(|s| s.len()).unwrap_or(0));
            }
            if p.tls.insecure {
                println!("{}", format!("  UNVERIFIED — this profile checks nothing, by typed confirmation: {}", p.tls.i_understand.as_deref().unwrap_or("?")).red());
            }
        }
        "fetch" => {
            // Re-read the published fingerprint without connecting anything
            // else (§5.3): the profile's own trust decides whether the
            // ladder is content, and the server's claim is checked against
            // what we actually saw.
            let cl = match ApiClient::tcp_multi(&p.url, None, Some(&p.tls), None, None) {
                Ok(c) => c,
                Err(e) => return err(e),
            };
            match cl.server_info().await {
                Ok(v) => {
                    let claim = v["fingerprint"].as_str().unwrap_or("(the server does not say)");
                    let seen = cl.accepted_fingerprint();
                    let agrees = match (&seen, glidex_tls::Trust::normalize_pin(claim)) {
                        (Some(s), Some(c)) => glidex_tls::Trust::normalize_pin(s).is_some_and(|n| n.as_str() == c.as_str()),
                        _ => false,
                    };
                    println!("{} {claim}", "server claims:".dimmed());
                    println!("{} {seen:?}", "we were handed:".dimmed());
                    if agrees {
                        let which = glidex_tls::Trust::normalize_pin(claim).and_then(|c| p.tls.pins.iter().position(|pin| glidex_tls::Trust::normalize_pin(pin).is_some_and(|n| n.as_str() == c.as_str()))).map(|i| format!("pin #{} of the profile matches", i + 1));
                        println!("{}", which.unwrap_or_else(|| "the certificate is trusted (through the system store or the profile's CA)".into()).green());
                    } else {
                        println!("{}", "MISMATCH: what the server claims and what the connection presented are different certificates — a proxy may be terminating TLS here. The control plane regenerates its certificate only in narrow cases (rotate-ca, re-init): confirm with its operator through another channel before trusting this".red().bold());
                    }
                }
                Err(e) => err(format!("the profile's trust refused or could not reach the server: {e}")),
            }
        }
        "remove" => {
            if has_flag(args, "--all") {
                let mut what = format!("{} pin(s)", p.tls.pins.len());
                if p.tls.ca_file.is_some() || p.tls.ca_pem.is_some() {
                    what.push_str(" and the CA additions");
                }
                if p.tls.insecure {
                    what.push_str(" and the unverified bypass");
                }
                if !std::io::stdin().is_terminal() && !has_flag(args, "--yes") {
                    return err("--all clears everything the profile adds to trust; confirm interactively or pass --yes");
                }
                if std::io::stdin().is_terminal() && !prompt(&format!("Remove {what} from profile '{name}'? [y/N]: ")).to_lowercase().starts_with('y') {
                    return;
                }
                if let Err(e) = config::edit(cfg, |c| {
                    if let Some(pf) = c.profiles.get_mut(&name) {
                        pf.tls.pins.clear();
                        pf.tls.ca_file = None;
                        pf.tls.ca_pem = None;
                        pf.tls.insecure = false;
                        pf.tls.i_understand = None;
                    }
                }) {
                    return err(e);
                }
                println!("{} profile '{name}': trust is the system store alone again", "OK:".green());
            } else if let Some(pin) = args.get(2) {
                let want = glidex_tls::Trust::normalize_pin(pin);
                let Some(want) = want else {
                    return err(format!("{pin} is not a fingerprint (sha256/<64 hex>, as gxctl prints it)"));
                };
                let mut gone = false;
                if let Err(e) = config::edit(cfg, |c| {
                    if let Some(pf) = c.profiles.get_mut(&name) {
                        let before = pf.tls.pins.len();
                        pf.tls.pins.retain(|x| glidex_tls::Trust::normalize_pin(x).is_none_or(|n| n.as_str() != want.as_str()));
                        gone = pf.tls.pins.len() != before;
                    }
                }) {
                    return err(e);
                }
                println!("{}", if gone { format!("{} pin removed from profile '{name}'", "OK:".green()) } else { format!("profile '{name}' had no such pin") });
            } else {
                return usage("Usage: auth trust remove <sha256/pin> | auth trust remove --all [--yes]");
            }
        }
        _ => usage("Usage: auth trust list | fetch | remove <sha256/pin> | remove --all [--yes]"),
    }
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
    match args.first() {
        Some(&"bandwidth") => return rate_report(client, "/usage/bandwidth", &args[1..]).await,
        Some(&"disk-io") => return rate_report(client, "/usage/disk-io", &args[1..]).await,
        Some(&"compute") => return rate_report(client, "/usage/compute", &args[1..]).await,
        _ => {}
    }
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

/// `usage bandwidth|disk-io|compute [--month YYYY-MM] [--by k,…] [--project P] [--csv]` (§9.3).
async fn rate_report(client: &ApiClient, base: &str, args: &[&str]) {
    let mut path = base.to_string();
    if let Some(p) = flag_value(args, "--project").map(str::to_string).or_else(|| client.project()) {
        path = client::add_query(&path, "project", &p);
    }
    for (flag, key) in [("--month", "month"), ("--by", "group_by"), ("--from", "from"), ("--to", "to")] {
        if let Some(v) = flag_value(args, flag) {
            path = client::add_query(&path, key, v);
        }
    }
    if args.contains(&"--csv") {
        match client.request_bytes(Method::GET, &client::add_query(&path, "format", "csv"), None).await {
            Ok(r) => print!("{}", String::from_utf8_lossy(&r.body)),
            Err(e) => err(e.message),
        }
        return;
    }
    let v: Value = match client.request_json(Method::GET, &path, None).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let period = v["month"].as_str().map(str::to_string).unwrap_or_else(|| format!("{} → {}", s(&v["from"]), s(&v["to"])));
    println!(
        "{} {} ({}){}",
        match base.rsplit('/').next() {
            Some("disk-io") => "Disk I/O",
            Some("compute") => "CPU and memory",
            _ => "Bandwidth",
        }
        .bold(),
        period,
        s(&v["timezone"]),
        if v["final"].as_bool() == Some(true) { "" } else { ", month to date" }
    );
    let rows = v["rows"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("  Nothing recorded.");
        return;
    }
    let keys = v["group_by"].as_array().cloned().unwrap_or_default();
    for r in rows {
        let who: Vec<String> = keys
            .iter()
            .filter_map(|k| {
                let k = k.as_str()?;
                r[k].is_object().then(|| format!("{}={}", k, r[k]["name"].as_str().unwrap_or("?")))
            })
            .collect();
        println!("\n  {}  {}", who.join(" ").cyan(), format!("({} slots of 5 min)", r["slots"]["counted"]).dimmed());
        for part in ["avg", "peak", "p95"] {
            if let Some(o) = r[part].as_object() {
                let vals: Vec<String> = o
                    .iter()
                    .filter(|(k, v)| !v.is_null() && *k != "resolution_secs")
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                let label = match part {
                    "peak" => "30-second peak",
                    "p95" => "95th percentile",
                    _ => "average",
                };
                println!("    {:<16} {}", label, vals.join("  "));
            }
        }
        if r["latency_source"] == "none" {
            println!("    {}", "latency not available (Cloud Hypervisor)".dimmed());
        }
    }
}

/// `stats <vm>`: the latest rates (§9.4).
pub async fn stats(client: &ApiClient, args: &[&str]) {
    let Some(vm) = args.first() else { return err("Usage: stats <vm>") };
    let id = match client.resolve_vm(vm).await {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let v: Value = match client.request_json(Method::GET, &format!("/vms/{}/stats", enc(&id)), None).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    if v["sampled_at"].is_null() {
        println!("No recent sample (is the VM running?).");
        return;
    }
    println!("{} sampled at {} (over {} s)", vm.bold(), s(&v["sampled_at"]), v["resolution_secs"]);
    let line = |label: &str, o: &Value| {
        if let Some(o) = o.as_object() {
            let vals: Vec<String> = o.iter().map(|(k, x)| format!("{k}={x}")).collect();
            println!("  {:<24} {}", label, vals.join("  "));
        }
    };
    line("vm", &v["vm"]);
    for kind in ["nics", "disks"] {
        for x in v[kind].as_array().cloned().unwrap_or_default() {
            line(x["name"].as_str().unwrap_or("?"), &x["values"]);
        }
    }
}

/// `bandwidth <vm> | --network <net>`, `io <vm> | --disk <disk>` and
/// `compute <vm>`: the 5-minute series with its p95 (§9.3).
pub async fn series(client: &ApiClient, what: &str, args: &[&str]) {
    let path = match (what, flag_value(args, "--network"), flag_value(args, "--disk"), args.first()) {
        ("bandwidth", Some(n), _, _) => format!("/networks/{}/bandwidth", enc(n)),
        ("io", _, Some(d), _) => format!("/disks/{}/io", enc(d)),
        (_, None, None, Some(vm)) if !vm.starts_with("--") => match client.resolve_vm(vm).await {
            Ok(id) => format!("/vms/{}/{what}", enc(&id)),
            Err(e) => return err(e),
        },
        _ => {
            let alt = match what {
                "io" => " | --disk <disk>",
                "bandwidth" => " | --network <net>",
                _ => "",
            };
            return err(format!("Usage: {what} <vm>{alt} [--from D] [--to D]"));
        }
    };
    let mut path = client::add_query(&path, "p95", "true");
    for (flag, key) in [("--from", "from"), ("--to", "to")] {
        if let Some(v) = flag_value(args, flag) {
            path = client::add_query(&path, key, v);
        }
    }
    let v: Value = match client.request_json(Method::GET, &path, None).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let points = v["points"].as_array().cloned().unwrap_or_default();
    println!("{} → {}  ({} points of 5 min)", s(&v["from"]), s(&v["to"]), points.len());
    for p in &points {
        let vals: Vec<String> = p.as_object().into_iter().flatten().filter(|(k, _)| *k != "slot").map(|(k, x)| format!("{k}={x}")).collect();
        println!("  {}  {}", s(&p["slot"]).dimmed(), vals.join("  "));
    }
    if let Some(o) = v["p95"].as_object() {
        let vals: Vec<String> = o.iter().filter(|(k, x)| !x.is_null() && *k != "slots").map(|(k, x)| format!("{k}={x}")).collect();
        println!("  {} {}", "95th percentile:".bold(), vals.join("  "));
    }
}

// ---- project networks and sharing ------------------------------------------

pub async fn network(client: &ApiClient, args: &[&str]) {
    let u = "Usage: network list | create <name> [--project P [--isolated] [--vhost-user]] [--subnet CIDR] ... | rm <name> | share <net> <project-id> | unshare <net> <project-id> | shares <project> | accept <project> <net> | leave <project> <net>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => crate::list_networks(client).await,
        "create" | "add" => {
            let project = flag_value(args, "--project");
            match project {
                // Project networks: NAT or isolated, the bridge is generated.
                Some(p) => {
                    let pos = positional(&args[1..], &["--project", "--subnet", "--mtu"]);
                    let Some(name) = pos.first() else { return usage(u) };
                    let mode = if has_flag(args, "--isolated") { "isolated" } else { "nat" };
                    let mut body = json!({ "name": name, "mode": mode });
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
                        Ok(n) => println!(
                            "{} {} ({} on {}, project {})",
                            "Network created:".green(),
                            s(&n["name"]).yellow(),
                            if mode == "nat" { "NAT" } else { "isolated" },
                            s(&n["bridge"]),
                            p
                        ),
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

// ---- cluster (spec/clustering.md §5) ---------------------------------------

/// A token read from a 0600 file or stdin, never from argv.
fn read_token(args: &[&str]) -> Result<Zeroizing<String>, String> {
    use std::io::Read;
    let raw = match flag_value(args, "--token-file") {
        Some(p) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(p).map_err(|e| format!("{p}: {e}"))?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(format!("{p} is readable by others; chmod 600 it"));
            }
            std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?
        }
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
            s
        }
    };
    let t = raw.trim().to_string();
    if t.is_empty() {
        return Err("no token given (pass --token-file <file> or pipe it on stdin)".into());
    }
    Ok(Zeroizing::new(t))
}

pub async fn cluster(client: &ApiClient, args: &[&str]) {
    match args.first().copied() {
        Some("init") => {
            if !has_flag(args, "--force") {
                return usage("Usage: cluster init [--advertise <ip:port>] [--tunnel-ip <ip>] --force\n  Turns this host into a cluster of one and re-keys its records. It keeps a backup but can't be undone.");
            }
            let body = json!({ "advertise": flag_value(args, "--advertise"), "tunnel_ip": flag_value(args, "--tunnel-ip"), "force": true });
            match client.request_json::<Value>(Method::POST, "/cluster/init", Some(body)).await {
                Ok(v) => {
                    println!("{} cluster {} (this node: {})", "Created".green(), s(&v["cluster_id"]), s(&v["node_id"]));
                    println!("  Backup:      {}", s(&v["backup"]));
                    println!("  CA SHA-256:  {}", s(&v["ca_fingerprint"]));
                }
                Err(e) => err(e),
            }
        }
        Some("join") => {
            let Some(server) = flag_value(args, "--server") else {
                return usage("Usage: cluster join --server <ip:8842> [--role server|agent] [--advertise <ip:port>] [--name <name>] [--token-file <file>]\n  The token is read from the file or stdin, never from the command line.");
            };
            let token = match read_token(args) {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            if has_flag(args, "--import") {
                let body = json!({ "server": server, "token": token.as_str(), "advertise": flag_value(args, "--advertise"), "tunnel_ip": flag_value(args, "--tunnel-ip"), "name": flag_value(args, "--name") });
                match client.request_json::<Value>(Method::POST, "/cluster/import", Some(body)).await {
                    Ok(v) => println!("{} {}", "Waiting:".green(), s(&v["message"])),
                    Err(e) => err(e),
                }
                return;
            }
            let body = json!({ "server": server, "token": token.as_str(), "role": flag_value(args, "--role").unwrap_or("server"),
                "advertise": flag_value(args, "--advertise"), "tunnel_ip": flag_value(args, "--tunnel-ip"), "name": flag_value(args, "--name") });
            match client.request_json::<Value>(Method::POST, "/cluster/join", Some(body)).await {
                Ok(v) => println!("{} cluster {} as node {}", "Joined".green(), s(&v["cluster_id"]), s(&v["node_id"])),
                Err(e) => err(e),
            }
        }
        Some("join-token") => {
            let body = json!({ "role": flag_value(args, "--role").unwrap_or("agent"),
                "ttl_secs": flag_value(args, "--ttl").and_then(|t| t.trim_end_matches('h').parse::<u64>().ok().map(|h| h * 3600)),
                "allow_import": has_flag(args, "--allow-import") });
            match client.request_json::<Value>(Method::POST, "/cluster/join-tokens", Some(body)).await {
                Ok(v) => {
                    println!("{}", s(&v["token"]));
                    eprintln!("Shown once. Valid {} s; for a {} node. Servers: {}", v["ttl_secs"], s(&v["role"]), v["servers"].as_array().map(|a| a.iter().map(|x| s(x)).collect::<Vec<_>>().join(", ")).unwrap_or_default());
                }
                Err(e) => err(e),
            }
        }
        Some("status") | None => match client.request_json::<Value>(Method::GET, "/cluster/status?ports=true", None).await {
            Ok(v) => {
                if v["clustered"] == false {
                    println!("This host is standalone (not in a cluster).");
                    return;
                }
                println!("Cluster {}  node {} ({}, {})", s(&v["cluster_id"]), s(&v["name"]), s(&v["role"]), s(&v["advertise"]));
                let r = &v["raft"];
                if !r.is_null() {
                    println!("  Raft: {} term {}  leader: {}  applied {}  last log {}", s(&r["state"]), r["term"], if r["leader"].is_null() { "none".into() } else { format!("{} ({})", s(&r["leader"]["name"]), s(&r["leader"]["address"])) }, r["last_applied"], r["last_log_index"]);
                    for (kind, key) in [("voter", "voters"), ("learner", "learners")] {
                        for m in r[key].as_array().into_iter().flatten() {
                            println!("    {kind:<8} {}  {}", s(&m["name"]), s(&m["address"]));
                        }
                    }
                }
                for n in v["nodes"].as_array().into_iter().flatten() {
                    println!("  node {:<16} {:<7} {:<10} {}  v{} level {}", s(&n["name"]), s(&n["role"]), s(&n["phase"]), s(&n["advertise"]), s(&n["version"]), n["feature_level"]);
                }
                println!("  Feature level: {}", v["feature_level"]);
                if !v["ca"].is_null() {
                    println!("  CA rotation: signing {}…, {} old CA(s) still trusted", s(&v["ca"]["signing"]).chars().take(12).collect::<String>(), v["ca"]["retiring"].as_array().map_or(0, |a| a.len()));
                }
                let gaps: Vec<&Value> = v["ports"].as_array().into_iter().flatten().filter(|p| p["reachable"] == false).collect();
                for p in &gaps {
                    println!("  {} port {}/{} on {} is not reachable from {}", "!".yellow(), p["port"], s(&p["proto"]), s(&p["node"]), s(&v["name"]));
                }
                if gaps.is_empty() && v["ports"].is_array() {
                    println!("  Ports: every checked port answers");
                }
            }
            Err(e) => err(e),
        },
        Some("promote") => {
            let nodes: Vec<&str> = positional(&args[1..], &[]);
            if nodes.is_empty() {
                return usage("Usage: cluster promote <node>… [--force]");
            }
            match client.request_json::<Value>(Method::POST, "/cluster/promote", Some(json!({ "nodes": nodes, "force": has_flag(args, "--force") }))).await {
                Ok(v) => println!("{} {} voters", "OK".green(), v["voters"]),
                Err(e) => err(e),
            }
        }
        Some("rejoin") => {
            let Some(server) = flag_value(args, "--server") else {
                return usage("Usage: cluster rejoin --server <ip:8842> [--node-id <id>] [--advertise <ip:port>] [--token-file <file>]\n  Comes back as the same node. The token is from `gxctl node rejoin-token` and is read from the file or stdin.");
            };
            let token = match read_token(args) {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            let body = json!({ "server": server, "token": token.as_str(), "node_id": flag_value(args, "--node-id"), "advertise": flag_value(args, "--advertise"), "tunnel_ip": flag_value(args, "--tunnel-ip") });
            match client.request_json::<Value>(Method::POST, "/cluster/rejoin", Some(body)).await {
                Ok(v) => println!("{} node {} (Raft state {})", "Rejoined".green(), s(&v["node_id"]), if v["raft_fresh"] == true { "started afresh" } else { "kept" }),
                Err(e) => err(e),
            }
        }
        Some("dissolve") => {
            if !has_flag(args, "--force") {
                return usage("Usage: cluster dissolve --force\n  The last node ends the cluster and becomes a standalone host with everything it held. Other nodes must have left. A final snapshot is kept.");
            }
            match client.request_json::<Value>(Method::POST, "/cluster/dissolve", Some(json!({ "force": true }))).await {
                Ok(v) => println!("{} {} VM(s), {} disk(s) kept; snapshot {}. Restart the control plane to continue standalone.", "Dissolved:".green(), v["vms"], v["disks"], s(&v["snapshot"])),
                Err(e) => err(e),
            }
        }
        Some("leave") => match client.request_json::<Value>(Method::POST, "/cluster/leave", Some(json!({}))).await {
            Ok(v) => println!("{} {}: {}", "Left".green(), s(&v["left"]), s(&v["restart"])),
            Err(e) => err(e),
        },
        Some("rotate-ca") => {
            let body = json!({ "grace_secs": flag_value(args, "--grace").and_then(|g| g.parse::<u64>().ok()) });
            match client.request_json::<Value>(Method::POST, "/cluster/rotate-ca", Some(body)).await {
                Ok(v) => println!("{} new CA {}; {} old CA(s) retire when every node renewed (or at {})", "Rotating".green(), s(&v["signing_ca"]), v["retiring"], s(&v["retire_at"])),
                Err(e) => err(e),
            }
        }
        Some("snapshot") => {
            let Some(file) = args.get(1) else { return usage("Usage: cluster snapshot <file>") };
            match client.request_bytes(Method::GET, "/cluster/snapshot", None).await {
                Ok(r) => {
                    use std::os::unix::fs::OpenOptionsExt;
                    use std::io::Write;
                    match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(file).and_then(|mut f| f.write_all(&r.body)) {
                        Ok(()) => println!("{} {} ({} bytes, 0600: it holds credential and token hashes; store it like the database)", "Wrote".green(), file, r.body.len()),
                        Err(e) => err(format!("{file}: {e}")),
                    }
                }
                Err(e) => err(e.message),
            }
        }
        Some(other) => usage(&format!("Unknown cluster command '{other}'. Try: init, join, join-token, status, promote, snapshot, rotate-ca, rejoin, leave, dissolve")),
    }
}

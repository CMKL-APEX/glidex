//! The metadata endpoint from inside a guest (spec/29-workload-identity.md W3,
//! W6, §5, §7), on this host's glidex: it must be installed from this
//! workspace (`glidex-metad`, netd's rules) and serve a NAT network.
//!
//! 1. A service account (a project viewer) and a VM on the NAT network
//!    with it, reached over SSH.
//! 2. In the guest, with `curl`: `/v1/instance` names the VM; an identity
//!    token says who it is; a request without `Metadata-Flavor: Glidex`, or
//!    forwarded, is refused.
//! 3. The guest's glidex token acts as the service account (used from this
//!    host: NAT guests don't reach the host's API).
//! 4. An identity token for this cluster is exchanged at `/auth/exchange`
//!    (`federation=<tenant>/vm`) for a glidex token, once.
//!
//! Ignored by default: it needs a glidex on this host whose API socket the
//! user can use, KVM, the `ubuntu-26.04` image and `cloudhv-edk2`, and a
//! few minutes:
//!
//! ```text
//! cargo test -p glidex-e2e --test metadata -- --ignored --nocapture
//! ```
//!
//! `GLIDEX_E2E_KEEP=1` leaves the VM; `GLIDEX_E2E_PROJECT` (default
//! `default`), `GLIDEX_E2E_NETWORK` (a NAT network, default `default`),
//! `GLIDEX_E2E_IMAGE` (default ubuntu-26.04) and `GLIDEX_E2E_API_URL` (this
//! host's API over HTTPS, default `https://127.0.0.1:8841`) override them.

use glidex_e2e::*;
use serde_json::{Value, json};
use std::process::Command;
use std::time::Duration;

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn step(what: &str) {
    eprintln!("\n==> {what}");
}

/// What this test made, removed at the end unless kept.
struct Made {
    api: Api<'static>,
    vm: Option<String>,
    credential: String,
    token: Option<String>,
    keydir: std::path::PathBuf,
    keep: bool,
}

impl Drop for Made {
    fn drop(&mut self) {
        if self.keep {
            eprintln!("GLIDEX_E2E_KEEP set: leaving VM {:?}", self.vm);
            return;
        }
        if let Some(id) = &self.vm {
            let _ = self
                .api
                .call("POST", &format!("/vms/{id}/stop?wait=60"), Some(&json!({})));
            let _ = self
                .api
                .call("DELETE", &format!("/vms/{id}?wait=120"), None);
        }
        if let Some(t) = &self.token {
            let _ = self.api.call("DELETE", &format!("/tokens/{t}"), None);
        }
        let _ = self.api.call(
            "DELETE",
            &format!(
                "/credentials/{}?project={}",
                self.credential,
                env("GLIDEX_E2E_PROJECT", "default")
            ),
            None,
        );
        let _ = std::fs::remove_dir_all(&self.keydir);
    }
}

/// The claims of a JWT, unverified (signatures are the unit tests' job).
fn claims(jwt: &str) -> Value {
    use base64::Engine as _;
    let payload = jwt.split('.').nth(1).expect("a JWT");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .expect("base64url");
    serde_json::from_slice(&bytes).expect("JWT claims are JSON")
}

/// `(status, body)` of a request to this host's HTTPS API with `token`.
fn https(method: &str, path: &str, token: Option<&str>, form: Option<&str>) -> (u16, Value) {
    let url = format!(
        "{}{path}",
        env("GLIDEX_E2E_API_URL", "https://127.0.0.1:8841")
    );
    let mut c = Command::new("curl");
    c.args(["-sk", "-X", method, "-w", "\n%{http_code}", &url]);
    if let Some(t) = token {
        c.args(["-H", &format!("Authorization: Bearer {t}")]);
    }
    if let Some(f) = form {
        c.args([
            "-H",
            "Content-Type: application/x-www-form-urlencoded",
            "--data",
            f,
        ]);
    }
    let out = run(&mut c);
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    (
        code.trim().parse().unwrap_or(0),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

#[test]
#[ignore = "needs this host's glidex installed from this workspace, KVM and a NAT network"]
fn a_guest_gets_its_identity_from_the_metadata_endpoint() {
    let api = Api::Local(API_SOCKET.into());
    let project = env("GLIDEX_E2E_PROJECT", "default");
    let tag = uuid::Uuid::new_v4().simple().to_string()[..6].to_string();
    let keydir = std::env::temp_dir().join(format!("gxe2e-md-{tag}"));
    std::fs::create_dir_all(&keydir).unwrap();
    let (key, public) = ssh_keypair(&keydir);
    let credential = format!("gxmd{tag}");
    let mut made = Made {
        api: Api::Local(API_SOCKET.into()),
        vm: None,
        credential: credential.clone(),
        token: None,
        keydir,
        keep: std::env::var("GLIDEX_E2E_KEEP").is_ok(),
    };

    step("a credential, a service account (viewer) and a VM with it");
    api.ok(
        "POST",
        "/credentials",
        Some(
            &json!({ "username": credential, "ssh_authorized_keys": [public], "project": project }),
        ),
    );
    let sa = api.ok(
        "POST",
        "/tokens",
        Some(&json!({ "name": format!("gxmd-{tag}"), "kind": "TOKEN_KIND_SERVICE_ACCOUNT", "project": project,
            "roles": [{ "role": "role.viewer", "project": project }], "expires_in_days": 1 })),
    );
    let sa_id = sa["record"]["id"]
        .as_str()
        .expect("a service account")
        .to_string();
    made.token = Some(sa_id.clone());
    let (id, ssh) = make_vm(
        &api,
        &format!("gxmd-{tag}"),
        &credential,
        &project,
        &sa_id,
        &key,
    );
    made.vm = Some(id.clone());

    let md_on = |ssh: &Ssh, path: &str, headers: &str| {
        ssh.check(&format!(
            "curl -s -w '\\n%{{http_code}}' {headers} 'http://169.254.169.254{path}'"
        ))
    };
    let md = |path: &str, headers: &str| md_on(&ssh, path, headers);
    let split = |out: String| -> (u16, String) {
        let (body, code) = out.trim_end().rsplit_once('\n').unwrap_or(("", "0"));
        (code.trim().parse().unwrap_or(0), body.to_string())
    };
    let flavor = "-H 'Metadata-Flavor: Glidex'";

    step("in the guest: /v1/instance names this VM");
    let (code, body) = split(md("/v1/instance", flavor));
    assert_eq!(code, 200, "{body}");
    let inst: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(inst["vm_id"], id.as_str(), "{inst}");

    step("in the guest: refused without the header, or forwarded");
    assert_eq!(split(md("/v1/instance", "")).0, 403);
    assert_eq!(
        split(md(
            "/v1/instance",
            &format!("{flavor} -H 'X-Forwarded-For: 10.0.0.1'")
        ))
        .0,
        403
    );

    step("in the guest: an identity token says who it is");
    let (code, jwt) = split(md("/v1/identity?audience=gx-e2e", flavor));
    assert_eq!(code, 200, "{jwt}");
    let c = claims(&jwt);
    assert_eq!(c["aud"], "gx-e2e", "{c}");
    assert!(
        c["sub"].as_str().unwrap().ends_with(&format!("/vm/{id}")),
        "{c}"
    );
    assert_eq!(c["glidex"]["service_account"], sa_id.as_str(), "{c}");
    assert_eq!(c["exp"].as_u64().unwrap() - c["iat"].as_u64().unwrap(), 900);
    let cluster = c["glidex"]["cluster"].as_str().unwrap().to_string();
    let tenant = c["glidex"]["tenant"].as_str().unwrap().to_string();

    step("in the guest: a glidex token acts as the service account");
    let (code, body) = split(md("/v1/glidex/token", flavor));
    assert_eq!(code, 200, "{body}");
    let t: Value = serde_json::from_str(&body).unwrap();
    let access = t["access_token"].as_str().unwrap();
    assert!(access.starts_with("gxt_"));
    let (code, vms) = https("GET", "/vms", Some(access), None);
    assert_eq!(code, 200, "{vms}");
    assert!(vms.to_string().contains(&id), "a viewer sees the VM: {vms}");
    let (code, _) = https("DELETE", &format!("/vms/{id}"), Some(access), None);
    assert_eq!(code, 403, "only a viewer");

    step("the network opens this host's API to its guests, then closes it");
    // A node NAT network keeps its guests from the host but for DHCP, DNS,
    // ping and metadata; `host_api` opens the API (spec §15).
    let net = env("GLIDEX_E2E_NETWORK", "default");
    let from_guest = |token: &str| {
        let out = ssh.run(&format!(
            "gw=$(ip route | awk '/^default/ {{print $3; exit}}'); curl -sk --max-time 5 -o /dev/null -w '%{{http_code}}' -H 'Authorization: Bearer {token}' https://$gw:8841/vms"
        ));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(from_guest(access), "000", "closed by default");
    api.ok(
        "PUT",
        &format!("/networks/{net}/host_api"),
        Some(&json!({ "host_api": true })),
    );
    wait_for("the API from the guest", Duration::from_secs(60), || {
        (from_guest(access) == "200").then_some(())
    });
    api.ok(
        "PUT",
        &format!("/networks/{net}/host_api"),
        Some(&json!({ "host_api": false })),
    );
    wait_for("the API closed again", Duration::from_secs(60), || {
        (from_guest(access) == "000").then_some(())
    });

    step("an identity token for this cluster, exchanged once");
    let (code, mine) = split(md(&format!("/v1/identity?audience={cluster}"), flavor));
    assert_eq!(code, 200, "{mine}");
    let form = format!(
        "grant_type=urn:ietf:params:oauth:grant-type:token-exchange&subject_token_type=urn:ietf:params:oauth:token-type:jwt&subject_token={mine}&federation={tenant}/vm"
    );
    let (code, ex) = https("POST", "/auth/exchange", None, Some(&form));
    assert_eq!(code, 200, "{ex}");
    let (code, _) = https("GET", "/vms", ex["access_token"].as_str(), None);
    assert_eq!(code, 200);
    let (code, again) = https("POST", "/auth/exchange", None, Some(&form));
    assert_eq!(
        (code, again["error"].as_str()),
        (400, Some("invalid_grant")),
        "only once"
    );

    step("a new VM given the same port index gets its own identity");
    // netd hands the freed index to the next VM: nothing given to the old
    // one may reach it (the control plane's answers are kept by VM).
    api.call("POST", &format!("/vms/{id}/stop?wait=60"), Some(&json!({})));
    api.ok("DELETE", &format!("/vms/{id}?wait=120"), None);
    made.vm = None;
    let (id2, ssh2) = make_vm(
        &api,
        &format!("gxmd-{tag}-2"),
        &credential,
        &project,
        &sa_id,
        &key,
    );
    made.vm = Some(id2.clone());
    let (code, jwt) = split(md_on(&ssh2, "/v1/identity?audience=gx-e2e", flavor));
    assert_eq!(code, 200, "{jwt}");
    let c = claims(&jwt);
    assert!(
        c["sub"].as_str().unwrap().ends_with(&format!("/vm/{id2}")),
        "the old VM's token reached the new one: {c}"
    );
    drop(made);
}

/// Make and start a VM with `sa` on the NAT network, and reach it.
fn make_vm(
    api: &Api,
    name: &str,
    credential: &str,
    project: &str,
    sa: &str,
    key: &std::path::Path,
) -> (String, Ssh) {
    let vm = api.ok(
        "POST",
        "/vms",
        Some(&json!({
            "name": name, "vcpu_count": 1, "mem_size_mib": 1024,
            "image": env("GLIDEX_E2E_IMAGE", "ubuntu-26.04"), "firmware": "cloudhv-edk2",
            "root_disk_size_gib": 10, "credential": credential, "project": project,
            "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR", "service_account": sa,
            "networks": [{ "network": env("GLIDEX_E2E_NETWORK", "default") }],
            "kernel_image_path": "", "rootfs_path": "",
        })),
    );
    let id = vm["vm"]["id"].as_str().unwrap().to_string();
    api.ok(
        "POST",
        &format!("/vms/{id}/start?wait=120"),
        Some(&json!({})),
    );
    let ip = wait_for("the VM's address", Duration::from_secs(120), || {
        let v = api.ok("GET", &format!("/vms/{id}"), None);
        v["vm"]["nics"][0]["ipv4"].as_str().map(String::from)
    });
    let ssh = Ssh {
        host: ip,
        user: credential.to_string(),
        key: key.to_path_buf(),
    };
    wait_for("SSH to the VM", Duration::from_secs(300), || {
        ssh.reachable().then_some(())
    });
    (id, ssh)
}

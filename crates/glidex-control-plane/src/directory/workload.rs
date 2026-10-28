//! Workload identity tokens for VMs (spec/workload-identity.md §4.2).
//!
//! A VM's identity token is a JWT signed with its directory's keys
//! (tenancy.md D53): `sub` is a SPIFFE ID in the directory's trust domain
//! (W9, W13), `aud` what the guest asked for (W7), glidex's own claims under
//! `glidex`. It grants nothing in glidex by itself (W4): it is typed `JWT`,
//! never `at+jwt`, so no API accepts it as a bearer token (`api::op`).

use serde_json::{Value, json};

/// The JOSE `typ` of an identity token (JWT-SVIDs allow `JWT`).
pub const TYP: &str = "JWT";
/// A VM's glidex tokens are federated tokens (tenancy.md D48) whose
/// `federation` is `vm:<tenant id>`: federation ids are `[a-z0-9-]`, so no
/// federation is ever named so (W6).
pub const VM_ORIGIN: &str = "vm:";
/// The federation name `POST /auth/exchange` takes for a VM's own identity
/// token, `<tenant>/vm` (§5); no federation may be called this.
pub const VM_FEDERATION: &str = "vm";
/// A VM's glidex token's lifetime (W6, W8).
pub const GLIDEX_TOKEN_TTL_SECS: u64 = 900;
/// Lifetimes (W8, §4.4).
pub const DEFAULT_TTL_SECS: u64 = 900;
pub const MIN_TTL_SECS: u64 = 600;
pub const MAX_TTL_SECS: u64 = 3_600;

/// The lifetime for a request: the default, or what was asked for if it is
/// within bounds.
pub fn ttl(requested: Option<u64>) -> Result<u64, String> {
    match requested {
        None => Ok(DEFAULT_TTL_SECS),
        Some(t) if (MIN_TTL_SECS..=MAX_TTL_SECS).contains(&t) => Ok(t),
        Some(t) => Err(format!(
            "ttl {t} s is outside {MIN_TTL_SECS}..={MAX_TTL_SECS}"
        )),
    }
}

/// A SPIFFE trust domain name: lowercase letters, digits, `.`, `-` and `_`,
/// at most 255 characters, no path (SPIFFE ID §2.1).
pub fn valid_trust_domain(td: &str) -> bool {
    !td.is_empty()
        && td.len() <= 255
        && !td.starts_with('.')
        && !td.ends_with('.')
        && td
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
}

/// A directory's trust domain unless one was chosen (W13):
/// `<directory>.<issuer host>`. `None` when the issuer has no host.
pub fn default_trust_domain(issuer: &str, directory: &str) -> Option<String> {
    let host = url::Url::parse(issuer)
        .ok()?
        .host_str()?
        .to_ascii_lowercase();
    let td = format!("{directory}.{}", host.trim_end_matches('.'));
    valid_trust_domain(&td).then_some(td)
}

/// Who a token is about. Tenant slugs and project names are `[a-z0-9-]`
/// and never change, so they are safe as SPIFFE path segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmIdentity {
    /// The tenant's slug.
    pub tenant: String,
    /// The project's name.
    pub project: String,
    pub vm_id: String,
    pub vm_name: String,
    /// The cluster running the VM.
    pub cluster: String,
    /// The VM's service account, once VMs have one (W5).
    pub service_account: Option<String>,
}

/// `spiffe://<trust domain>/tenant/<t>/project/<p>/vm/<id>` (W9).
pub fn spiffe_id(trust_domain: &str, id: &VmIdentity) -> String {
    format!(
        "spiffe://{trust_domain}/tenant/{}/project/{}/vm/{}",
        id.tenant, id.project, id.vm_id
    )
}

/// The claims of an identity token, without `iss` (the signer sets it).
pub fn identity_claims(
    trust_domain: &str,
    id: &VmIdentity,
    audience: &str,
    now: u64,
    ttl_secs: u64,
    jti: &str,
) -> Value {
    let mut glidex = json!({
        "tenant": id.tenant,
        "project": id.project,
        "vm": { "id": id.vm_id, "name": id.vm_name },
        "cluster": id.cluster,
    });
    if let Some(sa) = &id.service_account {
        glidex["service_account"] = sa.clone().into();
    }
    json!({
        "sub": spiffe_id(trust_domain, id),
        "aud": audience,
        "iat": now,
        "nbf": now,
        "exp": now + ttl_secs,
        "jti": jti,
        "glidex": glidex,
    })
}

/// What an audience may be: non-empty printable text, at most 512 bytes.
pub fn valid_audience(aud: &str) -> bool {
    !aud.is_empty() && aud.len() <= 512 && !aud.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm() -> VmIdentity {
        VmIdentity {
            tenant: "acme".into(),
            project: "web".into(),
            vm_id: "6f1c0000-0000-4000-8000-000000000001".into(),
            vm_name: "web-1".into(),
            cluster: "standalone-x".into(),
            service_account: None,
        }
    }

    #[test]
    fn lifetimes() {
        assert_eq!(ttl(None), Ok(900));
        assert_eq!(ttl(Some(600)), Ok(600));
        assert_eq!(ttl(Some(3_600)), Ok(3_600));
        assert!(ttl(Some(599)).is_err());
        assert!(ttl(Some(3_601)).is_err());
    }

    #[test]
    fn trust_domains() {
        assert_eq!(
            default_trust_domain("https://glidex.example.org/d/local", "local").as_deref(),
            Some("local.glidex.example.org")
        );
        assert_eq!(
            default_trust_domain("https://Host.Example:8443/d/acme", "acme").as_deref(),
            Some("acme.host.example")
        );
        assert_eq!(default_trust_domain("not a url", "local"), None);
        for ok in ["acme.com", "glidex.acme.internal", "a_b-c.d"] {
            assert!(valid_trust_domain(ok), "{ok}");
        }
        for bad in [
            "",
            "Acme.com",
            "acme.com/x",
            ".acme",
            "acme.",
            "a b",
            "spiffe://x",
        ] {
            assert!(!valid_trust_domain(bad), "{bad}");
        }
    }

    #[test]
    fn claims() {
        let c = identity_claims(
            "local.g.example",
            &vm(),
            "sts.amazonaws.com",
            1_000,
            900,
            "j1",
        );
        assert_eq!(
            c["sub"],
            "spiffe://local.g.example/tenant/acme/project/web/vm/6f1c0000-0000-4000-8000-000000000001"
        );
        assert_eq!(c["aud"], "sts.amazonaws.com");
        assert_eq!(c["exp"], 1_900);
        assert_eq!(c["nbf"], 1_000);
        assert_eq!(c["glidex"]["vm"]["name"], "web-1");
        assert_eq!(c["glidex"]["cluster"], "standalone-x");
        assert!(c["glidex"].get("service_account").is_none());
        assert!(c.get("iss").is_none(), "the signer sets iss");
        let mut with_sa = vm();
        with_sa.service_account = Some("tok-1".into());
        let c = identity_claims("t", &with_sa, "a", 0, 600, "j");
        assert_eq!(c["glidex"]["service_account"], "tok-1");
    }

    #[test]
    fn audiences() {
        assert!(valid_audience("sts.amazonaws.com"));
        assert!(valid_audience("https://vault.example/v1/auth/jwt"));
        assert!(!valid_audience(""));
        assert!(!valid_audience("a\nb"));
        assert!(!valid_audience(&"a".repeat(513)));
    }
}

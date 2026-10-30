//! The `google` preset (spec/19-tenancy.md D39, §5.1): Google Workspace,
//! allowed by the ID token's `hd` claim, and consumer Google accounts for
//! people the directory already knows.
//!
//! Google ID tokens carry no groups and no `amr`. Anyone can open a
//! consumer Google account under any email address, so the email's domain
//! proves nothing: only `hd` says the account belongs to a Workspace
//! organization.

use serde::{Deserialize, Serialize};

pub const ISSUER: &str = "https://accounts.google.com";
/// Google's tokens may carry the issuer without the scheme.
const ISSUER_ALIAS: &str = "accounts.google.com";
pub const SCOPES: [&str; 3] = ["openid", "email", "profile"];
/// Mail domains anyone can have an address at.
pub const PUBLIC_MAIL_DOMAINS: &[&str] = &["gmail.com", "googlemail.com"];

/// Which Google accounts a connector serves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accounts {
    /// Accounts of the Workspace domains in `domains`.
    #[default]
    Workspace,
    /// Any Google account, consumer ones included: gated (D39).
    Any,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleSettings {
    #[serde(default)]
    pub accounts: Accounts,
    pub client_id: String,
    /// Workspace domains (`hd`). Required for `workspace`.
    #[serde(default)]
    pub domains: Vec<String>,
    /// With `jit`, only these verified addresses get a new account.
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    /// The *TVs and Limited Input devices* client for gxctl's device
    /// grant; without one, device logins are refused.
    #[serde(default)]
    pub device_client_id: Option<String>,
    /// The operator says the Workspace domain enforces 2-step
    /// verification: its logins count as multi-factor.
    #[serde(default)]
    pub trust_2sv: bool,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// A local mock IdP instead of Google, for tests: a loopback URL only.
    #[serde(default)]
    pub issuer: Option<String>,
}

impl GoogleSettings {
    pub fn issuer(&self) -> &str {
        self.issuer.as_deref().unwrap_or(ISSUER)
    }

    /// `jit` and `guarded` as the connector has them (D22).
    pub fn validate(&self, jit: bool, guarded: bool) -> Result<(), String> {
        if let Some(i) = &self.issuer
            && !(i.starts_with("http://127.0.0.1") || i.starts_with("http://localhost"))
        {
            return Err(
                "a google connector's issuer is Google's; issuer is for a local mock".into(),
            );
        }
        if self.client_id.is_empty() {
            return Err("client_id is required".into());
        }
        if self.device_client_id.as_deref() == Some("") {
            return Err("device_client_id is empty".into());
        }
        for d in &self.domains {
            if d.is_empty() || d.contains('@') || d.contains('/') || d.contains(char::is_whitespace)
            {
                return Err(format!("{d:?} is not a domain"));
            }
            if super::is_public_mail_domain(d) {
                return Err(format!("{d} is a public mail domain: no Workspace has it"));
            }
        }
        for e in &self.allowed_emails {
            if !e.contains('@') {
                return Err(format!("{e:?} is not an email address"));
            }
        }
        match self.accounts {
            Accounts::Workspace if self.domains.is_empty() => {
                return Err("accounts: workspace needs domains (the Workspace's hd)".into());
            }
            Accounts::Any if jit && self.allowed_emails.is_empty() => {
                return Err(
                    "accounts: any with jit would let in anyone with a Google account: set allowed_emails, or turn jit off and invite".into(),
                );
            }
            _ => {}
        }
        if self.trust_2sv && self.accounts == Accounts::Any {
            return Err(
                "trust_2sv is for Workspace: consumer accounts have no 2-step policy to trust"
                    .into(),
            );
        }
        // D22: Google sends no amr, so the owner's and operators' tenants
        // take a Workspace whose 2-step verification the operator vouches for.
        if guarded && (self.accounts != Accounts::Workspace || !self.trust_2sv) {
            return Err(
                "new users of this connector join an owner or operator tenant: use accounts: workspace with trust_2sv".into(),
            );
        }
        Ok(())
    }

    pub fn rule(&self) -> Rule {
        Rule {
            accounts: self.accounts,
            domains: self
                .domains
                .iter()
                .map(|d| d.to_ascii_lowercase())
                .collect(),
            trust_2sv: self.trust_2sv,
        }
    }

    pub fn preset(&self) -> super::Preset {
        let mut auth_params = Vec::new();
        // A hint for Google's account chooser only; `hd` is checked in the token.
        if self.accounts == Accounts::Workspace {
            let hd = match self.domains.as_slice() {
                [one] => one.clone(),
                _ => "*".to_string(),
            };
            auth_params.push(("hd".to_string(), hd));
        }
        super::Preset {
            issuer_aliases: if self.issuer.is_none() {
                vec![ISSUER_ALIAS.to_string()]
            } else {
                Vec::new()
            },
            auth_params,
            rule: super::ClaimRule::Google(self.rule()),
        }
    }
}

/// The checks a `google` connector adds to the OIDC ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub accounts: Accounts,
    /// Lower case.
    pub domains: Vec<String>,
    pub trust_2sv: bool,
}

impl Rule {
    /// Whether the login counts as multi-factor, or why it isn't allowed.
    pub fn check(
        &self,
        claims: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bool, String> {
        // Google has sent it as a string too.
        let verified = match claims.get("email_verified") {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s == "true",
            _ => false,
        };
        if !verified {
            return Err("the Google account's email is not verified".into());
        }
        let hd = claims
            .get("hd")
            .and_then(|h| h.as_str())
            .map(|h| h.to_ascii_lowercase());
        match self.accounts {
            Accounts::Workspace => {
                if !hd.as_ref().is_some_and(|h| self.domains.contains(h)) {
                    return Err("not an account of the connector's Workspace domains".into());
                }
                Ok(self.trust_2sv)
            }
            // A consumer login proves no second factor.
            Accounts::Any => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claims(v: serde_json::Value) -> std::collections::HashMap<String, serde_json::Value> {
        serde_json::from_value(v).unwrap()
    }

    fn workspace() -> GoogleSettings {
        GoogleSettings {
            client_id: "web".into(),
            domains: vec!["Example.org".into()],
            ..Default::default()
        }
    }

    #[test]
    fn workspace_logins_are_allowed_by_hd_not_by_the_email() {
        let r = workspace().rule();
        let ok =
            claims(json!({"email": "a@example.org", "email_verified": true, "hd": "example.org"}));
        assert_eq!(r.check(&ok), Ok(false));
        // A consumer account under the company's address has no hd.
        let forged = claims(json!({"email": "a@example.org", "email_verified": true}));
        assert!(r.check(&forged).is_err());
        let other =
            claims(json!({"email": "a@other.org", "email_verified": true, "hd": "other.org"}));
        assert!(r.check(&other).is_err());
        let unverified =
            claims(json!({"email": "a@example.org", "email_verified": false, "hd": "example.org"}));
        assert!(r.check(&unverified).is_err());
        let as_string = claims(
            json!({"email": "a@example.org", "email_verified": "true", "hd": "example.org"}),
        );
        assert_eq!(r.check(&as_string), Ok(false));
    }

    #[test]
    fn trust_2sv_decides_mfa_and_consumer_logins_have_none() {
        let mut s = workspace();
        s.trust_2sv = true;
        let ok = claims(json!({"email_verified": true, "hd": "example.org", "amr": ["pwd"]}));
        assert_eq!(s.rule().check(&ok), Ok(true));
        let any = GoogleSettings {
            accounts: Accounts::Any,
            client_id: "web".into(),
            ..Default::default()
        };
        let consumer = claims(json!({"email_verified": true, "amr": ["mfa"]}));
        assert_eq!(any.rule().check(&consumer), Ok(false), "amr isn't Google's");
    }

    #[test]
    fn settings_are_checked() {
        assert!(workspace().validate(true, false).is_ok());
        let mut s = workspace();
        s.domains.clear();
        assert!(s.validate(true, false).is_err(), "workspace needs domains");
        let mut s = workspace();
        s.domains = vec!["gmail.com".into()];
        assert!(s.validate(true, false).is_err(), "public mail domain");
        let mut s = workspace();
        s.issuer = Some("https://evil.example".into());
        assert!(s.validate(true, false).is_err(), "only a local mock");
        let any = GoogleSettings {
            accounts: Accounts::Any,
            client_id: "web".into(),
            ..Default::default()
        };
        assert!(any.validate(true, false).is_err(), "anyone with jit");
        assert!(any.validate(false, false).is_ok(), "invitations only");
        let mut listed = any.clone();
        listed.allowed_emails = vec!["bob@gmail.com".into()];
        assert!(listed.validate(true, false).is_ok());
        // D22.
        assert!(
            listed.validate(false, true).is_err(),
            "consumer accounts for the owner"
        );
        assert!(
            workspace().validate(false, true).is_err(),
            "without trust_2sv"
        );
        let mut s = workspace();
        s.trust_2sv = true;
        assert!(s.validate(false, true).is_ok());
    }

    #[test]
    fn the_hd_hint_is_one_domain_or_any_workspace() {
        let p = workspace().preset();
        assert_eq!(
            p.auth_params,
            vec![("hd".to_string(), "Example.org".to_string())]
        );
        assert_eq!(p.issuer_aliases, vec!["accounts.google.com".to_string()]);
        let mut s = workspace();
        s.domains.push("example.net".into());
        assert_eq!(s.preset().auth_params[0].1, "*");
    }
}

//! The `jumpcloud` preset (spec/tenancy.md §5.1): JumpCloud SSO over OIDC.
//! The issuer and scopes are JumpCloud's; groups come in the claim the
//! application's *Include group attribute* names.
//!
//! A JumpCloud user's email is set by the organization's administrators,
//! not by the user, so a login whose token says nothing about it counts
//! its email as verified (for `allowed_emails` and invitations); a token
//! that says `email_verified: false` is believed.

use serde::{Deserialize, Serialize};

pub const ISSUER: &str = "https://oauth.id.jumpcloud.com/";
pub const SCOPES: [&str; 3] = ["openid", "email", "profile"];
pub const GROUPS_CLAIM: &str = "memberOf";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JumpcloudSettings {
    pub client_id: String,
    /// The claim the application's *Include group attribute* names.
    #[serde(default)]
    pub groups_claim: Option<String>,
    /// With `jit`, only these addresses get a new account.
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// A local mock IdP instead of JumpCloud, for tests: a loopback URL only.
    #[serde(default)]
    pub issuer: Option<String>,
}

impl JumpcloudSettings {
    pub fn issuer(&self) -> &str {
        self.issuer.as_deref().unwrap_or(ISSUER)
    }

    pub fn groups_claim(&self) -> &str {
        self.groups_claim.as_deref().unwrap_or(GROUPS_CLAIM)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(i) = &self.issuer
            && !(i.starts_with("http://127.0.0.1") || i.starts_with("http://localhost"))
        {
            return Err(
                "a jumpcloud connector's issuer is JumpCloud's; issuer is for a local mock".into(),
            );
        }
        if self.client_id.is_empty() {
            return Err("client_id is required".into());
        }
        if self.groups_claim.as_deref() == Some("") {
            return Err("groups_claim is empty".into());
        }
        for e in &self.allowed_emails {
            if !e.contains('@') {
                return Err(format!("{e:?} is not an email address"));
            }
        }
        Ok(())
    }

    pub fn preset(&self) -> super::Preset {
        super::Preset {
            rule: super::ClaimRule::Jumpcloud,
            ..Default::default()
        }
    }
}

/// The email counts as verified unless the token says otherwise; MFA is
/// the generic `amr`/`acr` reading.
pub fn check(claims: &std::collections::HashMap<String, serde_json::Value>) -> super::Checked {
    let said = match claims.get("email_verified") {
        Some(serde_json::Value::Bool(b)) => Some(*b),
        Some(serde_json::Value::String(s)) => Some(s == "true"),
        _ => None,
    };
    super::Checked {
        mfa: None,
        email_verified: Some(said.unwrap_or(true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claims(v: serde_json::Value) -> std::collections::HashMap<String, serde_json::Value> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn its_emails_are_the_organizations() {
        assert_eq!(
            check(&claims(json!({"email": "a@corp.example"}))).email_verified,
            Some(true)
        );
        assert_eq!(
            check(&claims(
                json!({"email": "a@corp.example", "email_verified": false})
            ))
            .email_verified,
            Some(false)
        );
        assert_eq!(check(&claims(json!({}))).mfa, None, "MFA is amr/acr's");
    }

    #[test]
    fn settings_are_checked() {
        let s = JumpcloudSettings {
            client_id: "web".into(),
            ..Default::default()
        };
        assert!(s.validate().is_ok());
        assert_eq!(s.issuer(), ISSUER);
        assert_eq!(s.groups_claim(), "memberOf");
        assert!(
            JumpcloudSettings::default().validate().is_err(),
            "client_id"
        );
        let mut bad = s.clone();
        bad.issuer = Some("https://evil.example".into());
        assert!(bad.validate().is_err());
        let mut bad = s.clone();
        bad.allowed_emails = vec!["nope".into()];
        assert!(bad.validate().is_err());
    }
}

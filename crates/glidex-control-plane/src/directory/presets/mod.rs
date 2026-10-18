//! Presets (spec/tenancy.md §5.1): fixed configurations of the OIDC code
//! for one IdP. A preset fixes what the IdP fixes (issuer, scopes) and
//! checks the claims it adds to the generic ones, after the ID token is
//! verified.

pub mod google;

/// What a preset checks in a verified ID token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ClaimRule {
    /// Only the generic OIDC checks.
    #[default]
    None,
    Google(google::Rule),
}

impl ClaimRule {
    /// `Err` with why the login isn't allowed; `Ok(Some(mfa))` when the
    /// preset decides whether the login counts as multi-factor.
    pub fn check(
        &self,
        claims: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<Option<bool>, String> {
        match self {
            ClaimRule::None => Ok(None),
            ClaimRule::Google(r) => r.check(claims).map(Some),
        }
    }
}

/// What an OIDC client takes from its preset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preset {
    /// Other spellings of the issuer that the IdP's tokens carry.
    pub issuer_aliases: Vec<String>,
    /// Added to the authorization request.
    pub auth_params: Vec<(String, String)>,
    pub rule: ClaimRule,
}

/// Whether `domain` is a public mail domain, which no tenant or
/// Workspace connector may claim (D39).
pub fn is_public_mail_domain(domain: &str) -> bool {
    google::PUBLIC_MAIL_DOMAINS
        .iter()
        .any(|d| d.eq_ignore_ascii_case(domain))
}

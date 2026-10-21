//! Trust between a cluster and the directories of other organizations
//! (spec/tenancy.md §12.1, D63–D66): the offer token a cluster's operator
//! sends, and (task 5b) redeeming it and ending trust.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// An offer token's prefix.
pub const OFFER_PREFIX: &str = "gxo_";
/// An offer's default and longest life, in days.
pub const OFFER_DAYS: u64 = 7;
pub const OFFER_MAX_DAYS: u64 = 30;

/// What an offer token carries (D64): enough for the directory to reach
/// the cluster, pin it, and redeem the offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferToken {
    pub cluster_id: String,
    pub cluster_name: String,
    /// The cluster's API, `https://host[:port]`.
    pub api: String,
    /// Its certificate's SHA-256, 64 hex digits.
    pub tls_sha256: String,
    /// The tenant's slug and its directory's issuer.
    pub tenant: String,
    pub directory: String,
    pub offer: String,
    pub secret: String,
    pub expires_at: u64,
}

impl OfferToken {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("serializable");
        format!(
            "{OFFER_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    pub fn decode(s: &str) -> Option<OfferToken> {
        let b = s.trim().strip_prefix(OFFER_PREFIX)?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(b)
            .ok()?;
        serde_json::from_slice(&json).ok()
    }
}

/// The issuer URL as offers and trust records compare it: lower-case
/// scheme and host, no trailing `/`.
pub fn normalize_issuer(s: &str) -> Option<String> {
    let u: axum::http::Uri = s.trim().parse().ok()?;
    let a = u.authority()?;
    (u.scheme_str() == Some("https") && u.query().is_none()).then(|| {
        format!(
            "https://{}{}",
            a.as_str().to_ascii_lowercase(),
            u.path().trim_end_matches('/')
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_tokens_round_trip_and_refuse_junk() {
        let t = OfferToken {
            cluster_id: "c1".into(),
            cluster_name: "north".into(),
            api: "https://north.example:8841".into(),
            tls_sha256: "ab".repeat(32),
            tenant: "acme".into(),
            directory: "https://id.acme.example/d/acme".into(),
            offer: "o1".into(),
            secret: "s".into(),
            expires_at: 5,
        };
        let s = t.encode();
        assert!(s.starts_with(OFFER_PREFIX));
        assert_eq!(OfferToken::decode(&s), Some(t));
        assert_eq!(OfferToken::decode("gxo_!!"), None);
        assert_eq!(OfferToken::decode("gxt_abc"), None);
        assert_eq!(
            normalize_issuer("https://ID.Acme.example/d/acme/").as_deref(),
            Some("https://id.acme.example/d/acme")
        );
        assert_eq!(normalize_issuer("http://id.acme.example/d/acme"), None);
    }
}

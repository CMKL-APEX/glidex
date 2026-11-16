//! What a cluster checks when a directory calls it (D65, D66), and how
//! fresh a trusted directory's replica is (D13).

use crate::peer::{self, Replays};
use crate::wire::{
    self, EndAtCluster, FeedState, RedeemRequest, TrustedDirectory, normalize_issuer, trusted_id,
};
use serde_json::json;

/// What a redeem checks of the cluster's offer.
#[derive(Debug, Clone, Copy)]
pub struct Offer<'a> {
    /// Not redeemed, revoked or expired.
    pub open: bool,
    /// The SHA-256 (hex) of its secret.
    pub secret_hash: &'a str,
    /// The issuer and tenant slug it was made for.
    pub directory: &'a str,
    pub tenant: &'a str,
}

/// A redeem that checked out: the directory's issuer and certificate pin,
/// as the trust record keeps them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    pub issuer: String,
    pub pin: String,
}

/// Check directory's redeem `b` of `offer` (`None`: no such offer) at
/// cluster `cluster_id`, carrying `assertion` (D65). `Err` says why, for
/// the audit log; the caller answers every refusal the same way. The
/// caller spends the offer before writing anything else.
pub fn check_redeem(
    assertion: &str,
    b: &RedeemRequest,
    offer: Option<Offer<'_>>,
    cluster_id: &str,
    replays: &Replays,
    now: u64,
) -> Result<Redeemed, String> {
    let Some(offer) = offer else {
        return Err("no such offer".into());
    };
    if !offer.open {
        return Err("the offer was redeemed, or expired".into());
    }
    if !crate::constant_eq(&crate::sha256_hex(b.secret.as_bytes()), offer.secret_hash) {
        return Err("the wrong secret".into());
    }
    let Some(issuer) = normalize_issuer(&b.issuer) else {
        return Err("not an issuer URL".into());
    };
    if issuer != offer.directory {
        return Err("another directory than the offer's".into());
    }
    if b.tenant.slug != offer.tenant || b.tenant.id.is_empty() {
        return Err("another tenant than the offer's".into());
    }
    let Some(pin) = glidex_tls::Trust::normalize_pin(&b.tls_sha256) else {
        return Err("no certificate fingerprint".into());
    };
    peer::verify(
        assertion,
        &b.jwks,
        &issuer,
        cluster_id,
        wire::REDEEM,
        replays,
        now,
    )
    .map_err(|e| e.to_string())?;
    Ok(Redeemed { issuer, pin })
}

/// The trust record after redeem `b` checked out as `r`: `existing` (the
/// one for that issuer, if any) with the directory's address, pin and
/// keys as the redeem brought them, and the tenant added.
pub fn redeemed_directory(
    existing: Option<TrustedDirectory>,
    b: &RedeemRequest,
    r: &Redeemed,
    now: u64,
) -> TrustedDirectory {
    let mut d = existing.unwrap_or_else(|| TrustedDirectory {
        id: trusted_id(&r.issuer),
        issuer: r.issuer.clone(),
        directory: b.directory.clone(),
        api: String::new(),
        tls_sha256: String::new(),
        jwks: json!({ "keys": [] }),
        tenants: Vec::new(),
        created_at: now,
        max_staleness_minutes: None,
    });
    d.directory = b.directory.clone();
    d.api = b.api.clone();
    d.tls_sha256 = r.pin.clone();
    d.jwks = b.jwks.clone();
    d.tenants.retain(|x| x.id != b.tenant.id);
    d.tenants.push(b.tenant.clone());
    d
}

/// Check a trusted directory's `POST /trust/v1/end` (D66): `find` gives
/// the trust record for an issuer. The record, for the caller to drop
/// `b.tenant` from; `Err` says why, for the audit log.
pub fn check_end(
    assertion: &str,
    b: &EndAtCluster,
    find: impl FnOnce(&str) -> Option<TrustedDirectory>,
    cluster_id: &str,
    replays: &Replays,
    now: u64,
) -> Result<TrustedDirectory, String> {
    let Some(issuer) = normalize_issuer(&b.issuer) else {
        return Err("not an issuer URL".into());
    };
    let Some(d) = find(&issuer) else {
        return Err("not a trusted directory".into());
    };
    peer::verify(
        assertion,
        &d.jwks,
        &issuer,
        cluster_id,
        wire::END,
        replays,
        now,
    )
    .map_err(|e| e.to_string())?;
    Ok(d)
}

/// A trusted directory's replica counts as behind past this: three
/// missed pulls of [`FEED_EVERY`].
pub const BEHIND_SECS: u64 = 90;

/// How often a cluster pulls its trusted directories' feeds.
pub const FEED_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// How far behind `d`'s replica is at `now`, in seconds: since the
/// directory last answered a feed (since trust began, before any).
pub fn lag(d: &TrustedDirectory, st: &FeedState, now: u64) -> u64 {
    let last = st.checked_at.max(st.applied_at).max(d.created_at);
    now.saturating_sub(last)
}

/// Whether `d`'s credentials are still good here (D13): `Ok(lag)` in
/// seconds, or `Err(lag)` past its `max_staleness` (its own, else
/// `default_max_minutes`).
pub fn freshness(
    d: &TrustedDirectory,
    st: &FeedState,
    default_max_minutes: u64,
    now: u64,
) -> Result<u64, u64> {
    let lag = lag(d, st, now);
    let max = d.max_staleness_minutes.unwrap_or(default_max_minutes) * 60;
    if lag > max { Err(lag) } else { Ok(lag) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{ClusterKey, jwks};
    use crate::wire::RemoteTenant;

    fn request(dir_key: &ClusterKey) -> RedeemRequest {
        RedeemRequest {
            offer: "o1".into(),
            secret: "s3cret".into(),
            issuer: "https://ID.acme.example/d/acme".into(),
            directory: "acme".into(),
            tenant: RemoteTenant {
                id: "t1".into(),
                slug: "acme".into(),
                display_name: String::new(),
            },
            jwks: jwks(&[dir_key]),
            api: "https://id.acme.example:8841".into(),
            tls_sha256: "ab".repeat(32),
        }
    }

    #[test]
    fn a_redeem_checks_the_offer_then_the_assertion() {
        let k = ClusterKey::generate("acme:1").unwrap();
        let iss = "https://id.acme.example/d/acme";
        let t = crate::now();
        let r = Replays::default();
        let hash = crate::sha256_hex(b"s3cret");
        let offer = Offer {
            open: true,
            secret_hash: &hash,
            directory: iss,
            tenant: "acme",
        };
        let b = request(&k);
        let a = || peer::assertion(&k, iss, "c1", wire::REDEEM, t).unwrap();
        assert_eq!(
            check_redeem(&a(), &b, None, "c1", &r, t),
            Err("no such offer".into())
        );
        let closed = Offer {
            open: false,
            ..offer
        };
        assert!(check_redeem(&a(), &b, Some(closed), "c1", &r, t).is_err());
        let mut wrong = b.clone();
        wrong.secret = "nope".into();
        assert_eq!(
            check_redeem(&a(), &wrong, Some(offer), "c1", &r, t),
            Err("the wrong secret".into())
        );
        let mut wrong = b.clone();
        wrong.tenant.slug = "other".into();
        assert!(check_redeem(&a(), &wrong, Some(offer), "c1", &r, t).is_err());
        // An assertion for another cluster.
        let other = peer::assertion(&k, iss, "c2", wire::REDEEM, t).unwrap();
        assert!(check_redeem(&other, &b, Some(offer), "c1", &r, t).is_err());
        let ok = check_redeem(&a(), &b, Some(offer), "c1", &r, t).unwrap();
        assert_eq!(ok.issuer, iss);
        let d = redeemed_directory(None, &b, &ok, t);
        assert_eq!(d.id, trusted_id(iss));
        assert_eq!(d.tenants.len(), 1);
        // A second tenant of the same directory joins the record.
        let mut b2 = b.clone();
        b2.tenant.id = "t2".into();
        let d = redeemed_directory(Some(d), &b2, &ok, t);
        assert_eq!(d.tenants.len(), 2);
    }

    #[test]
    fn credentials_go_stale_past_max_staleness() {
        let d = TrustedDirectory {
            id: "trusted-x".into(),
            issuer: "https://x.example/d/x".into(),
            directory: "x".into(),
            api: String::new(),
            tls_sha256: String::new(),
            jwks: serde_json::Value::Null,
            tenants: vec![],
            created_at: 100,
            max_staleness_minutes: None,
        };
        let st = FeedState {
            checked_at: 1_000,
            ..Default::default()
        };
        assert_eq!(freshness(&d, &st, 60, 1_000 + 3_600), Ok(3_600));
        assert_eq!(freshness(&d, &st, 60, 1_000 + 3_601), Err(3_601));
        let d = TrustedDirectory {
            max_staleness_minutes: Some(1),
            ..d
        };
        assert_eq!(freshness(&d, &st, 60, 1_061), Err(61));
    }
}

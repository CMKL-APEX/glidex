//! What a cluster checks when a directory calls it (D65, D66, D91–D94),
//! how it records a directory's tenants' opt-ins (D93, D97), and how
//! fresh a trusted directory's replica is (D13).

use crate::peer::{self, Replays};
use crate::wire::{
    self, AdmissionStatus, AdmitRequest, AdmitSetting, EndAtCluster, FeedState, OfferScope,
    RedeemRequest, RemoteTenant, TrustedDirectory, WholeDirectory, normalize_issuer, trusted_id,
};
use serde_json::json;

/// What a redeem checks of the cluster's offer.
#[derive(Debug, Clone, Copy)]
pub struct Offer<'a> {
    /// Not redeemed, revoked or expired.
    pub open: bool,
    /// The SHA-256 (hex) of its secret.
    pub secret_hash: &'a str,
    /// The issuer it was made for.
    pub directory: &'a str,
    /// A tenant offer, for this tenant slug; or a directory offer
    /// (`tenant` empty), with how opt-ins are treated (D91, D92).
    pub scope: OfferScope,
    pub tenant: &'a str,
    pub admit: AdmitSetting,
}

/// A redeem that checked out: the directory's issuer and certificate pin,
/// as the trust record keeps them, and for a directory offer the setting
/// its tenants' opt-ins get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    pub issuer: String,
    pub pin: String,
    pub whole: Option<AdmitSetting>,
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
    match (offer.scope, &b.tenant) {
        (OfferScope::Tenant, Some(t)) if t.slug == offer.tenant && !t.id.is_empty() => {}
        (OfferScope::Tenant, Some(_)) => return Err("another tenant than the offer's".into()),
        (OfferScope::Tenant, None) => return Err("no tenant for a tenant offer".into()),
        (OfferScope::Directory, Some(_)) => {
            return Err("a tenant for a directory offer".into());
        }
        (OfferScope::Directory, None) => {}
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
    let whole = (offer.scope == OfferScope::Directory).then_some(offer.admit);
    Ok(Redeemed { issuer, pin, whole })
}

/// The trust record after redeem `b` checked out as `r`: `existing` (the
/// one for that issuer, if any) with the directory's address, pin and
/// keys as the redeem brought them, and the tenant added; for a directory
/// offer, trusted as a whole from now on, its tenants kept (D92).
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
        whole: None,
    });
    d.directory = b.directory.clone();
    d.api = b.api.clone();
    d.tls_sha256 = r.pin.clone();
    d.jwks = b.jwks.clone();
    if let Some(t) = &b.tenant {
        set_admission(&mut d, t, AdmissionStatus::Admitted);
    }
    if let Some(admit) = r.whole {
        let w = d.whole.get_or_insert_with(|| WholeDirectory {
            since: now,
            ..Default::default()
        });
        w.admit = admit;
    }
    d
}

/// Check a directory's `POST /trust/v1/admit` (D93): `find` gives the
/// trust record for an issuer, which must trust it as a whole. The
/// record, for [`admit`]; `Err` says why, for the audit log.
pub fn check_admit(
    assertion: &str,
    b: &AdmitRequest,
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
    if d.whole.is_none() {
        return Err("a directory not trusted as a whole".into());
    }
    if b.tenant.id.is_empty() || b.tenant.slug.is_empty() {
        return Err("no tenant".into());
    }
    peer::verify(
        assertion,
        &d.jwks,
        &issuer,
        cluster_id,
        wire::ADMIT,
        replays,
        now,
    )
    .map_err(|e| e.to_string())?;
    Ok(d)
}

/// Tenant `t` of `d` opts in (D93): admitted at once under `open`, else
/// pending, one entry per tenant (D97). A tenant admitted already stays
/// so, its names updated; a refused one may ask again. The status, which
/// is the answer.
pub fn admit(d: &mut TrustedDirectory, t: &RemoteTenant) -> AdmissionStatus {
    let status = match (d.admission(&t.id), d.whole.as_ref().map(|w| w.admit)) {
        (Some(AdmissionStatus::Admitted), _) | (_, Some(AdmitSetting::Open)) => {
            AdmissionStatus::Admitted
        }
        _ => AdmissionStatus::Pending,
    };
    set_admission(d, t, status);
    status
}

/// The operator decides pending tenant `id` of `d` (D96): admitted, or
/// refused. Whether it was pending.
pub fn decide(d: &mut TrustedDirectory, id: &str, approve: bool) -> bool {
    let Some(t) = d
        .whole
        .as_ref()
        .and_then(|w| w.pending.iter().find(|t| t.id == id))
        .cloned()
    else {
        return false;
    };
    let status = if approve {
        AdmissionStatus::Admitted
    } else {
        AdmissionStatus::Refused
    };
    set_admission(d, &t, status);
    true
}

/// Put tenant `t` of `d` in one list: `status`'s, out of the others.
fn set_admission(d: &mut TrustedDirectory, t: &RemoteTenant, status: AdmissionStatus) {
    d.tenants.retain(|x| x.id != t.id);
    if let Some(w) = &mut d.whole {
        w.pending.retain(|x| x.id != t.id);
        w.refused.retain(|x| x.id != t.id);
    }
    match (status, &mut d.whole) {
        (AdmissionStatus::Admitted, _) => d.tenants.push(t.clone()),
        (AdmissionStatus::Pending, Some(w)) => w.pending.push(t.clone()),
        (AdmissionStatus::Refused, Some(w)) => w.refused.push(t.clone()),
        // A directory trusted by tenant offers only has no other list.
        (_, None) => {}
    }
}

/// `d` after the end `b` checked out (D66, D94): `None` when the trust
/// ends, for the caller to forget the record and drop the replica. One
/// tenant leaving takes it out of every list; the record stays while
/// tenants are admitted, or while it is trusted as a whole. An end with
/// no tenant ends the whole directory's trust.
pub fn ended(mut d: TrustedDirectory, b: &EndAtCluster) -> Option<TrustedDirectory> {
    if b.tenant.is_empty() {
        return None;
    }
    d.tenants.retain(|x| x.id != b.tenant);
    if let Some(w) = &mut d.whole {
        w.pending.retain(|x| x.id != b.tenant);
        w.refused.retain(|x| x.id != b.tenant);
    }
    (d.whole.is_some() || !d.tenants.is_empty()).then_some(d)
}

/// Check a trusted directory's `POST /trust/v1/end` (D66, D94): `find`
/// gives the trust record for an issuer. The record, for [`ended`]; `Err`
/// says why, for the audit log.
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

    fn tenant(id: &str, slug: &str) -> RemoteTenant {
        RemoteTenant {
            id: id.into(),
            slug: slug.into(),
            display_name: String::new(),
        }
    }

    fn request(dir_key: &ClusterKey) -> RedeemRequest {
        RedeemRequest {
            offer: "o1".into(),
            secret: "s3cret".into(),
            issuer: "https://ID.acme.example/d/acme".into(),
            directory: "acme".into(),
            tenant: Some(tenant("t1", "acme")),
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
            scope: OfferScope::Tenant,
            tenant: "acme",
            admit: AdmitSetting::Open,
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
        wrong.tenant.as_mut().unwrap().slug = "other".into();
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
        b2.tenant.as_mut().unwrap().id = "t2".into();
        let d = redeemed_directory(Some(d), &b2, &ok, t);
        assert_eq!(d.tenants.len(), 2);
        assert!(d.whole.is_none());
        // A tenant offer redeemed with no tenant, and a tenant sent for a
        // directory offer, are refused before the assertion is looked at.
        let mut none = b.clone();
        none.tenant = None;
        assert_eq!(
            check_redeem("x", &none, Some(offer), "c1", &r, t),
            Err("no tenant for a tenant offer".into())
        );
        let whole = Offer {
            scope: OfferScope::Directory,
            tenant: "",
            admit: AdmitSetting::Approval,
            ..offer
        };
        assert_eq!(
            check_redeem("x", &b, Some(whole), "c1", &r, t),
            Err("a tenant for a directory offer".into())
        );
        // A directory offer: the record becomes whole, its tenants kept.
        let ok = check_redeem(&a(), &none, Some(whole), "c1", &r, t).unwrap();
        assert_eq!(ok.whole, Some(AdmitSetting::Approval));
        let d = redeemed_directory(Some(d), &none, &ok, t);
        assert_eq!(d.tenants.len(), 2);
        assert_eq!(d.whole.as_ref().unwrap().admit, AdmitSetting::Approval);
    }

    #[test]
    fn an_admit_needs_a_directory_trusted_as_a_whole_and_its_purpose() {
        let k = ClusterKey::generate("acme:1").unwrap();
        let iss = "https://id.acme.example/d/acme";
        let t = crate::now();
        let r = Replays::default();
        let mut d = TrustedDirectory {
            id: trusted_id(iss),
            issuer: iss.into(),
            directory: "acme".into(),
            api: String::new(),
            tls_sha256: String::new(),
            jwks: jwks(&[&k]),
            tenants: vec![],
            created_at: t,
            max_staleness_minutes: None,
            whole: None,
        };
        let b = AdmitRequest {
            issuer: "https://ID.acme.example/d/acme/".into(),
            tenant: tenant("t1", "physics"),
        };
        let a = |purpose| peer::assertion(&k, iss, "c1", purpose, t).unwrap();
        let find = |d: &TrustedDirectory| {
            let d = d.clone();
            move |i: &str| (i == d.issuer).then_some(d)
        };
        assert_eq!(
            check_admit(&a(wire::ADMIT), &b, find(&d), "c1", &r, t),
            Err("a directory not trusted as a whole".into())
        );
        d.whole = Some(WholeDirectory::default());
        assert_eq!(
            check_admit(&a(wire::ADMIT), &b, |_| None, "c1", &r, t),
            Err("not a trusted directory".into())
        );
        let mut no_tenant = b.clone();
        no_tenant.tenant.id.clear();
        assert!(check_admit(&a(wire::ADMIT), &no_tenant, find(&d), "c1", &r, t).is_err());
        // Another purpose's assertion, another cluster's, a replayed one.
        assert!(check_admit(&a(wire::REDEEM), &b, find(&d), "c1", &r, t).is_err());
        let other = peer::assertion(&k, iss, "c2", wire::ADMIT, t).unwrap();
        assert!(check_admit(&other, &b, find(&d), "c1", &r, t).is_err());
        let once = a(wire::ADMIT);
        assert!(check_admit(&once, &b, find(&d), "c1", &r, t).is_ok());
        assert!(check_admit(&once, &b, find(&d), "c1", &r, t).is_err());
    }

    #[test]
    fn opt_ins_follow_the_setting_and_ending_follows_the_scope() {
        let mut d = TrustedDirectory {
            id: "trusted-x".into(),
            issuer: "https://x.example/d/x".into(),
            directory: "x".into(),
            api: String::new(),
            tls_sha256: String::new(),
            jwks: serde_json::Value::Null,
            tenants: vec![],
            created_at: 1,
            max_staleness_minutes: None,
            whole: Some(WholeDirectory {
                admit: AdmitSetting::Approval,
                ..Default::default()
            }),
        };
        let (a, b) = (tenant("t1", "a"), tenant("t2", "b"));
        assert_eq!(admit(&mut d, &a), AdmissionStatus::Pending);
        assert_eq!(admit(&mut d, &a), AdmissionStatus::Pending);
        assert_eq!(d.whole.as_ref().unwrap().pending.len(), 1);
        assert!(decide(&mut d, "t1", false));
        assert_eq!(d.admission("t1"), Some(AdmissionStatus::Refused));
        assert_eq!(admit(&mut d, &a), AdmissionStatus::Pending);
        assert!(d.whole.as_ref().unwrap().refused.is_empty());
        d.whole.as_mut().unwrap().admit = AdmitSetting::Open;
        assert_eq!(admit(&mut d, &b), AdmissionStatus::Admitted);
        // Pending under approval, then open: asking again admits it.
        assert_eq!(admit(&mut d, &a), AdmissionStatus::Admitted);
        assert_eq!(
            d.admissions()
                .iter()
                .map(|(t, s)| (t.id.as_str(), *s))
                .collect::<Vec<_>>(),
            vec![
                ("t2", AdmissionStatus::Admitted),
                ("t1", AdmissionStatus::Admitted)
            ]
        );
        // Tenants leave; the directory's trust stays until ended whole.
        let end = |tenant: &str| EndAtCluster {
            issuer: d.issuer.clone(),
            tenant: tenant.into(),
        };
        let d2 = ended(d.clone(), &end("t1")).unwrap();
        let d2 = ended(d2, &end("t2")).unwrap();
        assert!(d2.tenants.is_empty());
        assert_eq!(ended(d2, &end("")), None);
        // A directory trusted by tenant offers ends with its last tenant.
        let mut by_offers = d.clone();
        by_offers.whole = None;
        let by_offers = ended(by_offers, &end("t1")).unwrap();
        assert_eq!(ended(by_offers, &end("t2")), None);
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
            whole: None,
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

//! A renewed API certificate, held until the directories that trust this
//! cluster know it (spec/19-tenancy.md D90).
//!
//! A trusted directory pins the certificate its calls to this cluster
//! meet. A node that renews the certificate it serves at the cluster's API
//! address keeps serving the old one and publishes the new one's pin in its
//! node record; the leader announces it in its reports (D88) and records
//! which directories accepted them; the node switches once all have, or
//! when the old certificate is close to expiring.

use crate::api::App;
use crate::trust::DirectoryPins;
use tokio_rustls::rustls::sign::CertifiedKey;

/// The old certificate is let go this long before it expires, whatever
/// the directories know.
pub const HOLD_UNTIL_EXPIRY_SECS: i64 = 7 * 86_400;

/// A certificate's pin as the trust records keep it: 64 lowercase hex.
pub fn pin_of(key: &CertifiedKey) -> String {
    glidex_tls::Trust::normalize_pin(&glidex_tls::fingerprint_certified(key)).unwrap_or_default()
}

/// Whether a node serving the certificate pinned `serving` (expiring at
/// `expires_at`) may switch to its renewal pinned `next` (D90): when no
/// trusted directory meets `serving` at the API address, when every one
/// that does has accepted a report announcing `next`, or when `serving`
/// expires within [`HOLD_UNTIL_EXPIRY_SECS`].
pub fn may_switch(
    serving: &str,
    next: &str,
    expires_at: i64,
    pins: &[(String, DirectoryPins)],
    now: i64,
) -> bool {
    let mut meeting = pins.iter().filter(|(_, p)| p.seen == serving).peekable();
    meeting.peek().is_none()
        || meeting.all(|(_, p)| p.accepted.as_deref() == Some(next))
        || expires_at - now < HOLD_UNTIL_EXPIRY_SECS
}

/// Record that this node serves `key` and holds no renewal; refresh the
/// node record if that changed.
pub fn serving(app: &App, key: &CertifiedKey) {
    if app.manager.set_api_pins(Some(pin_of(key)), None) {
        report(app);
    }
}

/// The gate a node's certificate reload asks before serving renewal `new`
/// over `old` (`glidex_tls::FileCert::watch_gated`): publishes `new` as
/// pending, then `may_switch`.
pub fn gate(app: &App, new: &CertifiedKey, old: &CertifiedKey) -> bool {
    let (serving_pin, next) = (pin_of(old), pin_of(new));
    let directories = app.auth.trust.directories().unwrap_or_default();
    if !directories.is_empty()
        && app
            .manager
            .set_api_pins(Some(serving_pin.clone()), Some(next.clone()))
    {
        tracing::info!(serving = %serving_pin, %next, "holding a renewed API certificate until trusted directories know it");
        report(app);
    }
    let pins: Vec<_> = app
        .auth
        .trust
        .directory_pins()
        .unwrap_or_default()
        .into_iter()
        .filter(|(id, _)| directories.iter().any(|d| d.id == *id))
        .collect();
    let expires_at = glidex_tls::leaf_names(old).map_or(0, |(_, t)| t);
    may_switch(
        &serving_pin,
        &next,
        expires_at,
        &pins,
        crate::tenancy::now() as i64,
    )
}

fn report(app: &App) {
    let m = app.manager.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = m.report_self() {
            tracing::warn!("publishing this node's API certificate pins: {}", e);
        }
    });
}

/// What the leader announces (D90): the pin of the certificate the API
/// address presents, and the renewal the node serving it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub seen: String,
    pub next: Option<String>,
}

/// Find the node serving this cluster's API address by the certificate it
/// presents, and the renewal it holds. `None`, logged, when the address
/// can't be reached or no node serves what it presents.
pub async fn announcement(app: &App) -> Option<Announcement> {
    let api = crate::service::tenancy::api_endpoint(&app.auth.config, &app.manager)?;
    let nodes = app.manager.nodes().list().ok()?;
    let pins: Vec<&str> = nodes
        .iter()
        .filter_map(|n| n.status.api_pin.as_deref())
        .collect();
    if pins.is_empty() {
        return None;
    }
    let seen = match glidex_trust::peer::presented(&api, &pins).await {
        Ok(p) => glidex_tls::Trust::normalize_pin(&p)?,
        Err(e) => {
            tracing::info!(%api, "the API address's certificate: {}", e);
            return None;
        }
    };
    let Some(node) = nodes
        .iter()
        .find(|n| n.status.api_pin.as_deref() == Some(seen.as_str()))
    else {
        tracing::info!(%api, %seen, "no node serves the certificate the API address presents: no renewal announced");
        return None;
    };
    Some(Announcement {
        seen,
        next: node.status.api_pin_pending.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins(v: &[(&str, Option<&str>)]) -> Vec<(String, DirectoryPins)> {
        v.iter()
            .enumerate()
            .map(|(i, (seen, accepted))| {
                (
                    format!("d{i}"),
                    DirectoryPins {
                        seen: seen.to_string(),
                        accepted: accepted.map(String::from),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_renewal_is_held_until_every_directory_meeting_it_knows_it() {
        let far = 100 * 86_400;
        // No directory meets this certificate: switch.
        assert!(may_switch("a", "b", far, &[], 0));
        assert!(may_switch("a", "b", far, &pins(&[("z", None)]), 0));
        // One that does hasn't accepted the new pin: hold.
        assert!(!may_switch("a", "b", far, &pins(&[("a", None)]), 0));
        assert!(!may_switch(
            "a",
            "b",
            far,
            &pins(&[("a", Some("b")), ("a", Some("c"))]),
            0
        ));
        // All of them have: switch.
        assert!(may_switch(
            "a",
            "b",
            far,
            &pins(&[("a", Some("b")), ("a", Some("b")), ("z", None)]),
            0
        ));
        // The old certificate close to expiring: switch anyway.
        let soon = HOLD_UNTIL_EXPIRY_SECS - 1;
        assert!(may_switch("a", "b", soon, &pins(&[("a", None)]), 0));
        assert!(!may_switch(
            "a",
            "b",
            HOLD_UNTIL_EXPIRY_SECS,
            &pins(&[("a", None)]),
            0
        ));
    }
}

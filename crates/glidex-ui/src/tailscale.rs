//! The node's Tailscale certificates (spec/tailscale-tls.md §4.2): with
//! `GLIDEX_UI_TLS_TAILSCALE=on`, the UI serves them by SNI through
//! `glidex_tls::tailscale`, which hyperstor's management service shares.

pub use glidex_tls::tailscale::*;

/// `GLIDEX_UI_TLS_TAILSCALE`: `off` (default) or `on`.
pub fn enabled(v: Option<&str>) -> Result<bool, String> {
    match v {
        None | Some("off") => Ok(false),
        Some("on") => Ok(true),
        Some(o) => Err(format!("GLIDEX_UI_TLS_TAILSCALE: '{o}' is not on or off")),
    }
}

#[cfg(test)]
mod tests {
    use super::enabled;

    #[test]
    fn the_setting() {
        assert!(!enabled(None).unwrap());
        assert!(!enabled(Some("off")).unwrap());
        assert!(enabled(Some("on")).unwrap());
        assert!(enabled(Some("yes")).is_err());
    }
}

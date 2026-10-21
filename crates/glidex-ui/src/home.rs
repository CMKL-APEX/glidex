//! Where this web UI belongs (spec/host-page.md §3, W4, W5).
//!
//! glidex-ui reports its own address to the control plane (`PUT /ui/self`
//! on the UI socket) at start and every 30 seconds. The answer says, on an
//! agent, which server's UI browsers go to (`redirect`) and the agent's own
//! Host page there (`home`); on a server, any extra `Host` values to accept
//! (its `cluster.ui_url`, e.g. a load balancer). The last answer is kept
//! while the control plane can't be reached; a host that never got one
//! serves normally.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How often glidex-ui reports itself.
pub const EVERY: Duration = Duration::from_secs(30);

/// The control plane's answer to `PUT /ui/self`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    /// The server UI an agent sends browsers to.
    pub redirect: Option<String>,
    /// The agent's Host page path there; set only on an agent.
    pub home: Option<String>,
    /// More `Host` values to accept (W4), `host[:port]`.
    pub extra_hosts: Vec<String>,
    /// The servers' node addresses, for the page shown without `redirect`.
    pub servers: Vec<String>,
}

impl Answer {
    pub fn from_json(v: &serde_json::Value) -> Self {
        let strings = |k: &str| {
            v[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        Answer {
            redirect: v["redirect"]
                .as_str()
                .map(|s| s.trim_end_matches('/').to_string()),
            home: v["home"]
                .as_str()
                .filter(|h| h.starts_with('/'))
                .map(String::from),
            extra_hosts: strings("extra_hosts"),
            servers: strings("servers"),
        }
    }

    /// This host is a cluster agent: browsers belong on a server.
    pub fn agent(&self) -> bool {
        self.home.is_some()
    }
}

#[derive(Debug, Default)]
pub struct Home(RwLock<Option<Answer>>);

impl Home {
    pub fn answer(&self) -> Option<Answer> {
        self.0.read().unwrap().clone()
    }

    pub fn set(&self, a: Answer) {
        let mut cur = self.0.write().unwrap();
        if cur.as_ref() != Some(&a) {
            tracing::info!(
                redirect = ?a.redirect,
                extra_hosts = ?a.extra_hosts,
                agent = a.agent(),
                "where this UI belongs"
            );
        }
        *cur = Some(a);
    }

    /// `host` (a `Host` value) is one of the extra names (W4).
    pub fn allows_host(&self, host: &str) -> bool {
        let Some((h, port)) = super::split_host_port(host) else {
            return false;
        };
        self.0.read().unwrap().as_ref().is_some_and(|a| {
            a.extra_hosts.iter().any(|e| {
                super::split_host_port(e)
                    .is_some_and(|(eh, ep)| eh == h && (ep.is_none() || ep == port))
            })
        })
    }
}

/// Where a browser asking for `path_and_query` on an agent goes: the same
/// path on the server's UI, and the agent's Host page for `/`.
pub fn location(base: &str, home: &str, path_and_query: &str) -> String {
    let rest = if path_and_query == "/" || path_and_query.is_empty() {
        home
    } else {
        path_and_query
    };
    format!("{base}{rest}")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The page an agent shows when it knows no server's UI (W5).
fn no_server_page(servers: &[String]) -> Response {
    let list = if servers.is_empty() {
        "<p>It doesn't know the cluster's servers yet.</p>".to_string()
    } else {
        let items: String = servers
            .iter()
            .map(|s| format!("<li><code>{}</code></li>", escape(s)))
            .collect();
        format!("<p>Open the web UI of one of the cluster's servers:</p><ul>{items}</ul>")
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>GlideX agent</title></head>\
         <body style=\"font-family: system-ui, sans-serif; max-width: 40rem; margin: 3rem auto; padding: 0 1rem\">\
         <h1>This host is a cluster agent</h1>\
         <p>Its web UI is served by the cluster's servers, and none has published its address yet.</p>\
         {list}</body></html>"
    );
    let mut r = (StatusCode::SERVICE_UNAVAILABLE, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Layer inside the `Host` check: never proxy a browser to glidex-ui's own
/// `/ui/…` control-plane routes; on an agent, send browsers to a server.
pub async fn route(State(home): State<Arc<Home>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == "/api/ui" || path.starts_with("/api/ui/") {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }
    let Some(a) = home.answer().filter(Answer::agent) else {
        return next.run(req).await;
    };
    // The control plane answers the API itself: `421 use_a_server`.
    let api = path == "/api" || path.starts_with("/api/");
    if api || !matches!(*req.method(), Method::GET | Method::HEAD) {
        return next.run(req).await;
    }
    let Some(base) = &a.redirect else {
        return no_server_page(&a.servers);
    };
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let home_path = a.home.as_deref().unwrap_or("/");
    match HeaderValue::from_str(&location(base, home_path, pq)) {
        Ok(loc) => {
            let mut r = Response::new(Body::from("This host's web UI is on a server.\n"));
            *r.status_mut() = StatusCode::TEMPORARY_REDIRECT;
            r.headers_mut().insert(header::LOCATION, loc);
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn answers_and_locations() {
        let a = Answer::from_json(&json!({
            "redirect": "https://lb.example.com/",
            "home": "/hosts/prism1",
            "servers": ["10.0.0.1:8842"],
        }));
        assert!(a.agent());
        assert_eq!(a.redirect.as_deref(), Some("https://lb.example.com"));
        let base = a.redirect.as_deref().unwrap();
        assert_eq!(
            location(base, "/hosts/prism1", "/"),
            "https://lb.example.com/hosts/prism1"
        );
        assert_eq!(
            location(base, "/hosts/prism1", "/vms/abc?tab=disks"),
            "https://lb.example.com/vms/abc?tab=disks"
        );
        // A server's answer: no home, so not an agent.
        let s = Answer::from_json(&json!({"extra_hosts": ["LB.example.com"], "servers": []}));
        assert!(!s.agent());
        // A home that isn't a path is ignored.
        assert_eq!(Answer::from_json(&json!({"home": "https://x"})).home, None);
    }

    #[test]
    fn extra_hosts_are_allowed() {
        let h = Home::default();
        assert!(!h.allows_host("lb.example.com"));
        h.set(Answer::from_json(
            &json!({"extra_hosts": ["lb.example.com", "alt.example.com:8443"]}),
        ));
        assert!(h.allows_host("LB.example.com"));
        assert!(h.allows_host("lb.example.com:443"));
        assert!(h.allows_host("alt.example.com:8443"));
        assert!(!h.allows_host("alt.example.com:5173"));
        assert!(!h.allows_host("evil.example.com"));
    }

    #[test]
    fn the_no_server_page_escapes() {
        let r = no_server_page(&["<script>".into()]);
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

//! glidex-authd over its real socket, in process, with fake PAM.

mod common;

use common::*;
use glidex_authd::client::{AuthOk, AuthdError};
use glidex_authd::proto::DEFAULT_SERVICE;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn raw(h: &Harness) -> (UnixStream, BufReader<UnixStream>) {
    let s = UnixStream::connect(&h.socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let r = BufReader::new(s.try_clone().unwrap());
    (s, r)
}

fn send(s: &mut UnixStream, r: &mut BufReader<UnixStream>, line: &[u8]) -> serde_json::Value {
    s.write_all(line).unwrap();
    let mut reply = String::new();
    assert!(r.read_line(&mut reply).unwrap() > 0, "connection closed");
    serde_json::from_str(&reply).unwrap()
}

#[test]
fn success_returns_uid_and_groups() {
    let h = start();
    let ok = h.client().authenticate("alice", PASSWORD, DEFAULT_SERVICE).unwrap();
    assert_eq!(
        ok,
        AuthOk {
            uid: 1000,
            groups: vec!["alice".into(), "glidex-users".into(), "staff".into()]
        }
    );
}

#[test]
fn wire_format() {
    let h = start();
    let (mut s, mut r) = raw(&h);
    let v = send(
        &mut s,
        &mut r,
        format!("{{\"id\":7,\"op\":\"authenticate\",\"args\":{{\"user\":\"alice\",\"password\":\"{PASSWORD}\"}}}}\n")
            .as_bytes(),
    );
    assert_eq!(v["id"], 7);
    assert_eq!(v["ok"]["uid"], 1000);
    assert_eq!(v["ok"]["groups"][1], "glidex-users");
    // Same connection, second request; service given explicitly.
    let v = send(
        &mut s,
        &mut r,
        br#"{"id":8,"op":"authenticate","args":{"user":"alice","password":"nope","service":"glidex"}}
"#,
    );
    assert_eq!(v["id"], 8);
    assert_eq!(v["error"]["code"], "denied");
    assert_eq!(v["error"]["message"], "authentication failed");
}

fn denied_body(h: &Harness, user: &str, password: &str) -> String {
    let (mut s, mut r) = raw(h);
    let line = format!(
        "{}\n",
        serde_json::json!({"id": 1, "op": "authenticate", "args": {"user": user, "password": password}})
    );
    let v = send(&mut s, &mut r, line.as_bytes());
    assert!(v.get("ok").is_none(), "{v}");
    v.to_string()
}

#[test]
fn every_denial_looks_the_same() {
    let h = start();
    let wrong_password = denied_body(&h, "alice", "wrong");
    let wrong_group = denied_body(&h, "mallory", PASSWORD);
    let unknown = denied_body(&h, "nobody-here", PASSWORD);
    let invalid_name = denied_body(&h, "Not A User!", PASSWORD);
    assert_eq!(
        wrong_password,
        r#"{"error":{"code":"denied","message":"authentication failed"},"id":1}"#
    );
    assert_eq!(wrong_group, wrong_password);
    assert_eq!(unknown, wrong_password);
    assert_eq!(invalid_name, wrong_password);
    // PAM ran only for alice; the others never reached it.
    assert_eq!(h.pam_calls(), 1);
    assert_eq!(h.client().authenticate("mallory", PASSWORD, DEFAULT_SERVICE), Err(AuthdError::Denied));
}

#[test]
fn other_peer_uid_is_disconnected() {
    let h = start_with(test_config(), Some(my_uid() + 1));
    let e = h.client().authenticate("alice", PASSWORD, DEFAULT_SERVICE).unwrap_err();
    assert!(matches!(e, AuthdError::Unavailable(_)), "{e:?}");
    // A raw connection gets EOF (or a reset) without any reply.
    let mut s = UnixStream::connect(&h.socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = s.write_all(b"{}\n");
    let mut buf = [0u8; 64];
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)));
    assert_eq!(h.pam_calls(), 0);
}

#[test]
fn missing_service_user_rejects_everyone() {
    let h = start_with(test_config(), None);
    assert!(matches!(
        h.client().authenticate("alice", PASSWORD, DEFAULT_SERVICE),
        Err(AuthdError::Unavailable(_))
    ));
}

#[test]
fn root_needs_allow_root() {
    use glidex_authd::server::Authd;
    use std::collections::HashMap;
    use std::sync::Arc;
    let mk = |allow_root| {
        let config = glidex_authd::config::Config {
            allow_root,
            ..test_config()
        };
        Authd::new(config, Some(999), Arc::new(FakePam::default()), Arc::new(FakeAccounts(HashMap::new())))
    };
    assert!(!mk(false).peer_allowed(0));
    assert!(mk(true).peer_allowed(0));
    assert!(mk(false).peer_allowed(999));
    assert!(!mk(true).peer_allowed(1000));
}

#[test]
fn per_user_rate_limit() {
    let h = start();
    let c = h.client();
    for _ in 0..5 {
        assert_eq!(c.authenticate("alice", "wrong", DEFAULT_SERVICE), Err(AuthdError::Denied));
    }
    let calls = h.pam_calls();
    // Even the right password is refused now, without calling PAM.
    assert_eq!(c.authenticate("alice", PASSWORD, DEFAULT_SERVICE), Err(AuthdError::RateLimited));
    assert_eq!(h.pam_calls(), calls);
    // Unknown users are limited the same way, so the limit leaks nothing.
    for _ in 0..5 {
        assert_eq!(c.authenticate("ghost", "x", DEFAULT_SERVICE), Err(AuthdError::Denied));
    }
    assert_eq!(c.authenticate("ghost", "x", DEFAULT_SERVICE), Err(AuthdError::RateLimited));
    // Other users are unaffected.
    assert!(c.authenticate("user0", PASSWORD, DEFAULT_SERVICE).is_ok());
}

#[test]
fn success_resets_the_user_counter() {
    let h = start();
    let c = h.client();
    for _ in 0..4 {
        assert_eq!(c.authenticate("alice", "wrong", DEFAULT_SERVICE), Err(AuthdError::Denied));
    }
    assert!(c.authenticate("alice", PASSWORD, DEFAULT_SERVICE).is_ok());
    for _ in 0..4 {
        assert_eq!(c.authenticate("alice", "wrong", DEFAULT_SERVICE), Err(AuthdError::Denied));
    }
    assert!(c.authenticate("alice", PASSWORD, DEFAULT_SERVICE).is_ok());
}

#[test]
fn global_rate_limit() {
    let h = start();
    let c = h.client();
    // 30 failures spread over users so no single user hits its limit.
    for i in 0..30 {
        let user = format!("user{}", i % 10);
        assert_eq!(c.authenticate(&user, "wrong", DEFAULT_SERVICE), Err(AuthdError::Denied), "attempt {i}");
    }
    let calls = h.pam_calls();
    assert_eq!(c.authenticate("alice", PASSWORD, DEFAULT_SERVICE), Err(AuthdError::RateLimited));
    assert_eq!(h.pam_calls(), calls);
}

#[test]
fn malformed_json_is_a_protocol_error() {
    let h = start();
    let (mut s, mut r) = raw(&h);
    let v = send(&mut s, &mut r, b"{not json\n");
    assert_eq!(v["id"], 0);
    assert_eq!(v["error"]["code"], "protocol_error");
    // The message never quotes the input.
    let v = send(&mut s, &mut r, br#"{"id":"sekrit-input","op":"authenticate"}
"#);
    assert_eq!(v["error"]["code"], "protocol_error");
    assert!(!v.to_string().contains("sekrit"), "{v}");
    // The connection stays usable.
    let v = send(&mut s, &mut r, br#"{"id":2,"op":"frobnicate"}
"#);
    assert_eq!(v["id"], 2);
    assert_eq!(v["error"]["code"], "protocol_error");
    let v = send(&mut s, &mut r, br#"{"id":3,"op":"authenticate"}
"#);
    assert_eq!(v["error"]["message"], "missing args");
    let v = send(
        &mut s,
        &mut r,
        br#"{"id":4,"op":"authenticate","args":{"user":"alice","password":"x","service":"../../etc/x"}}
"#,
    );
    assert_eq!(v["error"]["code"], "protocol_error");
    assert_eq!(h.pam_calls(), 0);
}

#[test]
fn oversize_line_is_rejected_and_closed() {
    let h = start();
    let (mut s, mut r) = raw(&h);
    let mut big = br#"{"id":1,"op":"authenticate","args":{"user":"alice","password":""#.to_vec();
    big.extend(std::iter::repeat_n(b'a', 70 * 1024));
    big.extend_from_slice(b"\"}}\n");
    let v = send(&mut s, &mut r, &big);
    assert_eq!(v["error"]["code"], "protocol_error");
    assert_eq!(v["error"]["message"], "request too large");
    let mut rest = String::new();
    assert_eq!(r.read_line(&mut rest).unwrap_or(0), 0, "connection should be closed");
    assert_eq!(h.pam_calls(), 0);
}

#[test]
fn connection_cap() {
    let config = glidex_authd::config::Config {
        max_connections: 2,
        ..test_config()
    };
    let h = start_with(config, Some(my_uid()));
    // Two idle connections hold both slots.
    let a = UnixStream::connect(&h.socket).unwrap();
    let b = UnixStream::connect(&h.socket).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(
        h.client().authenticate("alice", PASSWORD, DEFAULT_SERVICE),
        Err(AuthdError::RateLimited)
    ));
    drop((a, b));
    let mut ok = false;
    for _ in 0..50 {
        if h.client().authenticate("alice", PASSWORD, DEFAULT_SERVICE).is_ok() {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ok, "slots were not released");
}

#[test]
fn failure_delay_is_applied() {
    let config = glidex_authd::config::Config {
        failure_delay: Duration::from_millis(150),
        ..test_config()
    };
    let h = start_with(config, Some(my_uid()));
    let t = std::time::Instant::now();
    assert!(h.client().authenticate("alice", PASSWORD, DEFAULT_SERVICE).is_ok());
    assert!(t.elapsed() < Duration::from_millis(150));
    let t = std::time::Instant::now();
    assert_eq!(h.client().authenticate("nobody", "x", DEFAULT_SERVICE), Err(AuthdError::Denied));
    assert!(t.elapsed() >= Duration::from_millis(150));
}

#[test]
fn client_reports_missing_socket() {
    let c = glidex_authd::client::AuthdClient::new("/nonexistent/glidex-authd/auth.sock");
    assert!(matches!(c.authenticate("alice", "x", DEFAULT_SERVICE), Err(AuthdError::Unavailable(_))));
}

#[test]
fn socket_mode_is_0660() {
    use std::os::unix::fs::PermissionsExt;
    let h = start();
    let mode = std::fs::metadata(&h.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o660);
}

//! Wire protocol between the control plane and glidex-authd
//! (spec/security.md §5.3).
//!
//! Newline-delimited JSON over a Unix socket, one request per line, one
//! response per line, one request in flight per connection. There is a
//! single op, `authenticate`; no `hello` is needed.
//!
//! ```text
//! {"id":1,"op":"authenticate","args":{"user":"alice","password":"…","service":"glidex"}}
//! {"id":1,"ok":{"uid":1000,"groups":["glidex-users","staff"]}}
//! {"id":1,"error":{"code":"denied","message":"authentication failed"}}
//! ```

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use zeroize::Zeroizing;

pub const MAX_LINE: usize = 64 * 1024;
pub const DEFAULT_RUN_DIR: &str = "/run/glidex-authd";
pub const SOCKET_NAME: &str = "auth.sock";
pub const DEFAULT_SERVICE: &str = "glidex";
pub const OP_AUTHENTICATE: &str = "authenticate";

/// Error codes. `denied` never says why: a wrong password, an unknown user,
/// a user outside `allowed_groups` and an expired account look the same.
pub const CODE_DENIED: &str = "denied";
pub const CODE_RATE_LIMITED: &str = "rate_limited";
pub const CODE_PROTOCOL_ERROR: &str = "protocol_error";
pub const CODE_INTERNAL: &str = "internal";
pub const DENIED_MESSAGE: &str = "authentication failed";

/// A password: zeroized on drop, never shown by `Debug`.
#[derive(Clone, Default)]
pub struct Password(Zeroizing<String>);

impl Password {
    pub fn new(s: String) -> Self {
        Password(Zeroizing::new(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password([REDACTED])")
    }
}

impl Serialize for Password {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Password {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Password;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Password, E> {
                Ok(Password::new(v.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Password, E> {
                Ok(Password::new(v))
            }
        }
        d.deserialize_string(V)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticateArgs {
    pub user: String,
    pub password: Password,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// A request as read off the wire. A plain struct (not an adjacently tagged
/// enum) so serde parses `args` in place instead of buffering a copy of the
/// password.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    pub op: String,
    #[serde(default)]
    pub args: Option<AuthenticateArgs>,
}

/// The client side of [`Request`], borrowing its strings.
#[derive(Serialize)]
pub struct RequestRef<'a> {
    pub id: u64,
    pub op: &'a str,
    pub args: AuthenticateArgsRef<'a>,
}

#[derive(Serialize)]
pub struct AuthenticateArgsRef<'a> {
    pub user: &'a str,
    pub password: &'a str,
    pub service: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthOk {
    pub uid: u32,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }

    pub fn denied() -> Self {
        Self::new(CODE_DENIED, DENIED_MESSAGE)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<AuthOk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(id: u64, ok: AuthOk) -> Self {
        Self { id, ok: Some(ok), error: None }
    }

    pub fn err(id: u64, error: ErrorBody) -> Self {
        Self { id, ok: None, error: Some(error) }
    }
}

/// `[a-z_][a-z0-9_.-]{0,31}`: the portable subset of Unix user names.
pub fn valid_user_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && matches!(b[0], b'a'..=b'z' | b'_')
        && b[1..]
            .iter()
            .all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
}

/// `[a-z0-9_-]{1,32}`: a file name under `/etc/pam.d`.
pub fn valid_service_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b.iter().all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_names() {
        for ok in ["alice", "_svc", "a", "bob.smith", "x-1_y", &"a".repeat(32)] {
            assert!(valid_user_name(ok), "{ok}");
        }
        for bad in ["", "Alice", "1abc", "-x", ".x", "a b", "a/b", "é", &"a".repeat(33), "root\n"] {
            assert!(!valid_user_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn service_names() {
        assert!(valid_service_name("glidex"));
        assert!(valid_service_name("glidex-test_2"));
        for bad in ["", "../etc", "Glidex", "a.b", &"a".repeat(33)] {
            assert!(!valid_service_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn password_is_redacted_in_debug() {
        let req: Request = serde_json::from_str(
            r#"{"id":1,"op":"authenticate","args":{"user":"alice","password":"hunter2-secret"}}"#,
        )
        .unwrap();
        let args = req.args.as_ref().unwrap();
        assert_eq!(args.password.as_str(), "hunter2-secret");
        let dbg = format!("{req:?} {req:#?}");
        assert!(!dbg.contains("hunter2"), "{dbg}");
        assert!(dbg.contains("REDACTED"));
    }

    #[test]
    fn escaped_password_round_trips() {
        let req: Request = serde_json::from_str(
            r#"{"id":1,"op":"authenticate","args":{"user":"alice","password":"a\"b\\cé"}}"#,
        )
        .unwrap();
        assert_eq!(req.args.unwrap().password.as_str(), "a\"b\\cé");
    }
}

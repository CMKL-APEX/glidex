//! Access tokens (glidex spec/11-security.md §5.5): `<prefix>` and base62 of
//! 32 random bytes, of which only the SHA-256 is stored; a required
//! lifetime with a default and a maximum, and a short name. Each
//! application picks its prefix (glidex: `gxt_`) and stores the record.

use crate::secrets::{random_token, sha256_hex};
use thiserror::Error;

/// Random bytes in a token's secret.
pub const SECRET_BYTES: usize = 32;
/// The longest token name.
pub const NAME_MAX: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TokenError {
    #[error("token lifetime must be 1-{0} days")]
    Lifetime(u64),
    #[error("token name must be 1-64 characters")]
    Name,
}

/// A token's lifetime: what it gets when none is asked for, and the most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenRules {
    pub default_days: u64,
    pub max_days: u64,
}

impl TokenRules {
    /// The lifetime in days for a request of `requested` days.
    pub fn days(&self, requested: Option<u64>) -> Result<u64, TokenError> {
        let days = requested.unwrap_or(self.default_days);
        if days == 0 || days > self.max_days {
            return Err(TokenError::Lifetime(self.max_days));
        }
        Ok(days)
    }

    /// When a token made at `now` for `requested` days expires.
    pub fn expiry(&self, requested: Option<u64>, now: u64) -> Result<u64, TokenError> {
        Ok(now + self.days(requested)? * 86400)
    }
}

/// A token's name: 1-64 bytes.
pub fn check_name(name: &str) -> Result<(), TokenError> {
    if name.is_empty() || name.len() > NAME_MAX {
        return Err(TokenError::Name);
    }
    Ok(())
}

/// A new token: its secret, shown once, and what is stored in its place.
#[derive(Debug, Clone)]
pub struct MintedToken {
    pub secret: String,
    /// SHA-256 of `secret`: the token's key in the store.
    pub hash: String,
}

pub fn mint_token(prefix: &str) -> MintedToken {
    let secret = format!("{}{}", prefix, random_token(SECRET_BYTES));
    MintedToken {
        hash: token_hash(&secret),
        secret,
    }
}

/// The store key of token `secret`.
pub fn token_hash(secret: &str) -> String {
    sha256_hex(secret.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_carry_the_prefix_and_store_the_hash() {
        let t = mint_token("hst_");
        let rest = t.secret.strip_prefix("hst_").expect("prefix");
        // 32 bytes are 43 base62 digits, fewer with leading zero bytes.
        // Tokens are never printed, even in tests.
        assert!((40..=43).contains(&rest.len()), "length {}", rest.len());
        assert!(rest.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_eq!(t.hash, sha256_hex(t.secret.as_bytes()));
        assert_eq!(t.hash.len(), 64);
        assert_ne!(mint_token("hst_").secret, t.secret);
    }

    #[test]
    fn lifetimes_default_and_clamp() {
        let r = TokenRules {
            default_days: 90,
            max_days: 365,
        };
        assert_eq!(r.days(None), Ok(90));
        assert_eq!(r.days(Some(365)), Ok(365));
        assert_eq!(r.days(Some(366)), Err(TokenError::Lifetime(365)));
        assert_eq!(r.days(Some(0)), Err(TokenError::Lifetime(365)));
        assert_eq!(r.expiry(Some(2), 10), Ok(10 + 2 * 86400));
        assert_eq!(
            TokenError::Lifetime(365).to_string(),
            "token lifetime must be 1-365 days"
        );
    }

    #[test]
    fn names_are_short() {
        assert!(check_name("ci").is_ok());
        assert!(check_name(&"n".repeat(64)).is_ok());
        assert_eq!(check_name(""), Err(TokenError::Name));
        assert_eq!(check_name(&"n".repeat(65)), Err(TokenError::Name));
    }
}

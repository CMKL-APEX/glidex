//! Random values, hashes and comparisons for secrets (glidex
//! spec/security.md §5.5): session ids, CSRF values, tokens and tickets
//! are base62 of random bytes, and only their SHA-256 is stored.

use sha2::{Digest, Sha256};

pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    d.iter().map(|b| format!("{:02x}", b)).collect()
}

/// `n` random bytes.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system random source");
    b
}

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Base62 of `bytes` (big-endian), for tokens.
pub fn base62(bytes: &[u8]) -> String {
    let mut digits: Vec<u8> = Vec::new();
    let mut num: Vec<u8> = bytes.to_vec();
    while num.iter().any(|&b| b != 0) {
        let mut rem: u32 = 0;
        for b in num.iter_mut() {
            let acc = (rem << 8) | *b as u32;
            *b = (acc / 62) as u8;
            rem = acc % 62;
        }
        digits.push(BASE62[rem as usize]);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    digits.reverse();
    String::from_utf8(digits).expect("ascii")
}

/// URL-safe random string (session ids, CSRF values, tickets).
pub fn random_token(n: usize) -> String {
    base62(&random_bytes(n))
}

/// Compare two strings in time independent of where they differ.
pub fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Clean a client-claimed `device` / `client` string for storage: one
/// trimmed line, ≤ 128 characters, no control characters; `None` on
/// anything else. Display and audit only — never an authorization input
/// (glidex spec/gxctl-auth.md §7.2).
pub fn stamp(s: Option<&str>) -> Option<String> {
    let s = s
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control) && s.len() <= 128)?;
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base62_round_numbers() {
        assert_eq!(base62(&[0]), "0");
        assert_eq!(base62(&[61]), "z");
        assert_eq!(base62(&[62]), "10");
        assert_eq!(base62(&[1, 0]), "48");
        assert!(random_token(32).len() >= 40);
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn constant_eq_compares_whole_strings() {
        assert!(constant_eq("abc", "abc"));
        assert!(!constant_eq("abc", "abd"));
        assert!(!constant_eq("abc", "ab"));
        assert!(constant_eq("", ""));
    }

    #[test]
    fn stamps_are_one_short_clean_line() {
        assert_eq!(stamp(Some("  laptop ")).as_deref(), Some("laptop"));
        assert_eq!(stamp(Some("a\nb")), None);
        assert_eq!(stamp(Some("   ")), None);
        assert_eq!(stamp(Some(&"x".repeat(129))), None);
        assert_eq!(stamp(None), None);
    }
}

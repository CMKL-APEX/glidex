//! WebAuthn, the parts a relying party checks (spec/passkeys.md §2): the
//! client data, the authenticator data, the attested credential's COSE
//! key, and assertion signatures. No attestation statement is trusted
//! (P1). CBOR is decoded here, strictly and only as far as WebAuthn needs:
//! definite lengths, no tags, no floats.

use base64::Engine as _;
use serde_json::Value;
use sha2::Digest as _;

/// COSE algorithms glidex accepts (P1).
pub const ES256: i64 = -7;
pub const EDDSA: i64 = -8;
pub const RS256: i64 = -257;
pub const ALGORITHMS: [i64; 3] = [ES256, EDDSA, RS256];

const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
const FLAG_BE: u8 = 0x08;
const FLAG_BS: u8 = 0x10;
const FLAG_AT: u8 = 0x40;
/// Nesting deep enough for an attestation object and its keys.
const MAX_DEPTH: usize = 8;

pub fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn unb64url(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|_| "not base64url".to_string())
}

fn sha256(b: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(b).into()
}

// ---- CBOR -----------------------------------------------------------------

/// A decoded CBOR value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cbor {
    Int(i64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Bool(bool),
    Null,
}

impl Cbor {
    /// The value under integer key `k` (COSE keys).
    pub fn int_key(&self, k: i64) -> Option<&Cbor> {
        match self {
            Cbor::Map(m) => m
                .iter()
                .find(|(key, _)| *key == Cbor::Int(k))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// The value under text key `k` (the attestation object).
    pub fn text_key(&self, k: &str) -> Option<&Cbor> {
        match self {
            Cbor::Map(m) => m
                .iter()
                .find(|(key, _)| matches!(key, Cbor::Text(t) if t == k))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Cbor::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn int(&self) -> Option<i64> {
        match self {
            Cbor::Int(i) => Some(*i),
            _ => None,
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(n).ok_or("CBOR length overflow")?;
        let s = self.b.get(self.at..end).ok_or("CBOR ends early")?;
        self.at = end;
        Ok(s)
    }

    fn arg(&mut self, info: u8) -> Result<u64, String> {
        Ok(match info {
            0..=23 => info as u64,
            24 => self.take(1)?[0] as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            _ => return Err("indefinite or reserved CBOR length".into()),
        })
    }

    fn value(&mut self, depth: usize) -> Result<Cbor, String> {
        if depth > MAX_DEPTH {
            return Err("CBOR nested too deep".into());
        }
        let first = self.take(1)?[0];
        let (major, info) = (first >> 5, first & 0x1f);
        let len = |n: u64| usize::try_from(n).map_err(|_| "CBOR length too large".to_string());
        Ok(match major {
            0 => Cbor::Int(i64::try_from(self.arg(info)?).map_err(|_| "CBOR integer too large")?),
            1 => {
                let n = i64::try_from(self.arg(info)?).map_err(|_| "CBOR integer too large")?;
                Cbor::Int(-1 - n)
            }
            2 => {
                let n = len(self.arg(info)?)?;
                Cbor::Bytes(self.take(n)?.to_vec())
            }
            3 => {
                let n = len(self.arg(info)?)?;
                Cbor::Text(
                    String::from_utf8(self.take(n)?.to_vec())
                        .map_err(|_| "CBOR text isn't UTF-8")?,
                )
            }
            4 => {
                let n = len(self.arg(info)?)?;
                if n > 1024 {
                    return Err("CBOR array too long".into());
                }
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(self.value(depth + 1)?);
                }
                Cbor::Array(v)
            }
            5 => {
                let n = len(self.arg(info)?)?;
                if n > 256 {
                    return Err("CBOR map too long".into());
                }
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = self.value(depth + 1)?;
                    v.push((k, self.value(depth + 1)?));
                }
                Cbor::Map(v)
            }
            7 => match info {
                20 => Cbor::Bool(false),
                21 => Cbor::Bool(true),
                22 => Cbor::Null,
                _ => return Err("unsupported CBOR simple value".into()),
            },
            _ => return Err("CBOR tags aren't used in WebAuthn".into()),
        })
    }
}

/// Decode one CBOR value from the start of `b`; also how many bytes it took.
pub fn cbor_prefix(b: &[u8]) -> Result<(Cbor, usize), String> {
    let mut r = Reader { b, at: 0 };
    let v = r.value(0)?;
    Ok((v, r.at))
}

/// Decode `b`, which must be exactly one CBOR value.
pub fn cbor(b: &[u8]) -> Result<Cbor, String> {
    let (v, n) = cbor_prefix(b)?;
    if n != b.len() {
        return Err("trailing bytes after CBOR".into());
    }
    Ok(v)
}

// ---- keys -----------------------------------------------------------------

/// A credential's public key, from its COSE encoding.
#[derive(Debug, Clone, PartialEq)]
pub enum PublicKey {
    Es256(p256::ecdsa::VerifyingKey),
    EdDsa(ed25519_dalek::VerifyingKey),
    Rs256(rsa::RsaPublicKey),
}

impl PublicKey {
    pub fn from_cose(k: &Cbor) -> Result<PublicKey, String> {
        let alg = k
            .int_key(3)
            .and_then(Cbor::int)
            .ok_or("COSE key without alg")?;
        let kty = k
            .int_key(1)
            .and_then(Cbor::int)
            .ok_or("COSE key without kty")?;
        let b = |label| k.int_key(label).and_then(Cbor::bytes);
        match (alg, kty) {
            (ES256, 2) => {
                if k.int_key(-1).and_then(Cbor::int) != Some(1) {
                    return Err("ES256 needs curve P-256".into());
                }
                let (x, y) = (b(-2).ok_or("no x")?, b(-3).ok_or("no y")?);
                if x.len() != 32 || y.len() != 32 {
                    return Err("bad P-256 point".into());
                }
                let mut sec1 = vec![4u8];
                sec1.extend_from_slice(x);
                sec1.extend_from_slice(y);
                p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1)
                    .map(PublicKey::Es256)
                    .map_err(|_| "bad P-256 point".into())
            }
            (EDDSA, 1) => {
                if k.int_key(-1).and_then(Cbor::int) != Some(6) {
                    return Err("EdDSA needs Ed25519".into());
                }
                let x: [u8; 32] = b(-2)
                    .ok_or("no x")?
                    .try_into()
                    .map_err(|_| "bad Ed25519 key")?;
                ed25519_dalek::VerifyingKey::from_bytes(&x)
                    .map(PublicKey::EdDsa)
                    .map_err(|_| "bad Ed25519 key".into())
            }
            (RS256, 3) => {
                let n = rsa::BigUint::from_bytes_be(b(-1).ok_or("no n")?);
                let e = rsa::BigUint::from_bytes_be(b(-2).ok_or("no e")?);
                let key = rsa::RsaPublicKey::new(n, e).map_err(|_| "bad RSA key")?;
                if rsa::traits::PublicKeyParts::size(&key) < 256 {
                    return Err("RSA keys are at least 2048 bits".into());
                }
                Ok(PublicKey::Rs256(key))
            }
            _ => Err(format!("algorithm {alg} isn't accepted")),
        }
    }

    pub fn alg(&self) -> i64 {
        match self {
            PublicKey::Es256(_) => ES256,
            PublicKey::EdDsa(_) => EDDSA,
            PublicKey::Rs256(_) => RS256,
        }
    }

    /// Whether `sig` signs `msg`.
    pub fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        match self {
            PublicKey::Es256(k) => {
                use p256::ecdsa::signature::Verifier as _;
                p256::ecdsa::Signature::from_der(sig).is_ok_and(|s| k.verify(msg, &s).is_ok())
            }
            PublicKey::EdDsa(k) => ed25519_dalek::Signature::from_slice(sig)
                .is_ok_and(|s| k.verify_strict(msg, &s).is_ok()),
            PublicKey::Rs256(k) => {
                let digest = sha2_10::Sha256::digest_of(msg);
                k.verify(rsa::Pkcs1v15Sign::new::<sha2_10::Sha256>(), &digest, sig)
                    .is_ok()
            }
        }
    }
}

/// `sha2` 0.10's SHA-256, which `rsa` 0.9 takes.
trait DigestOf {
    fn digest_of(msg: &[u8]) -> Vec<u8>;
}

impl DigestOf for sha2_10::Sha256 {
    fn digest_of(msg: &[u8]) -> Vec<u8> {
        use sha2_10::Digest as _;
        sha2_10::Sha256::digest(msg).to_vec()
    }
}

// ---- authenticator and client data -----------------------------------------

/// The authenticator data (WebAuthn §6.1).
#[derive(Debug, Clone)]
pub struct AuthData {
    pub rp_id_hash: [u8; 32],
    pub flags: u8,
    pub counter: u32,
    /// With registration: the AAGUID, credential id and key.
    pub attested: Option<(Vec<u8>, Vec<u8>, Cbor)>,
}

impl AuthData {
    pub fn parse(b: &[u8]) -> Result<AuthData, String> {
        if b.len() < 37 {
            return Err("authenticator data too short".into());
        }
        let mut rp = [0u8; 32];
        rp.copy_from_slice(&b[..32]);
        let flags = b[32];
        let counter = u32::from_be_bytes(b[33..37].try_into().unwrap());
        let attested = if flags & FLAG_AT != 0 {
            let rest = &b[37..];
            if rest.len() < 18 {
                return Err("attested credential data too short".into());
            }
            let aaguid = rest[..16].to_vec();
            let n = u16::from_be_bytes([rest[16], rest[17]]) as usize;
            if n == 0 || n > 1023 || rest.len() < 18 + n {
                return Err("bad credential id".into());
            }
            let id = rest[18..18 + n].to_vec();
            let (key, _used) = cbor_prefix(&rest[18 + n..])?;
            Some((aaguid, id, key))
        } else {
            None
        };
        Ok(AuthData {
            rp_id_hash: rp,
            flags,
            counter,
            attested,
        })
    }

    pub fn user_present(&self) -> bool {
        self.flags & FLAG_UP != 0
    }

    pub fn user_verified(&self) -> bool {
        self.flags & FLAG_UV != 0
    }

    /// Backup eligible and backed up (a synced passkey).
    pub fn backup(&self) -> (bool, bool) {
        (self.flags & FLAG_BE != 0, self.flags & FLAG_BS != 0)
    }
}

/// What a ceremony expects of the client and authenticator data.
pub struct Expect<'a> {
    /// `webauthn.create` or `webauthn.get`.
    pub kind: &'a str,
    pub challenge: &'a [u8],
    /// Origins accepted (P2).
    pub origins: &'a [String],
    pub rp_id: &'a str,
}

/// Check `clientDataJSON` (WebAuthn §7.1 steps 7–11).
pub fn check_client_data(raw: &[u8], e: &Expect<'_>) -> Result<(), String> {
    let v: Value = serde_json::from_slice(raw).map_err(|_| "client data isn't JSON")?;
    if v.get("type").and_then(Value::as_str) != Some(e.kind) {
        return Err(format!("client data type isn't {}", e.kind));
    }
    let ch = v
        .get("challenge")
        .and_then(Value::as_str)
        .ok_or("no challenge")?;
    if !crate::auth::constant_eq(ch, &b64url(e.challenge)) {
        return Err("wrong challenge".into());
    }
    let origin = v.get("origin").and_then(Value::as_str).ok_or("no origin")?;
    if !e.origins.iter().any(|o| o == origin) {
        return Err(format!("origin {origin} isn't accepted"));
    }
    if v.get("crossOrigin").and_then(Value::as_bool) == Some(true) {
        return Err("cross-origin ceremonies aren't accepted".into());
    }
    Ok(())
}

/// Check the authenticator data against the RP id and P3's flags.
pub fn check_auth_data(a: &AuthData, rp_id: &str) -> Result<(), String> {
    if !crate::auth::constant_eq(&b64url(&a.rp_id_hash), &b64url(&sha256(rp_id.as_bytes()))) {
        return Err("another relying party's credential".into());
    }
    if !a.user_present() {
        return Err("the user wasn't present".into());
    }
    if !a.user_verified() {
        return Err("the user wasn't verified".into());
    }
    Ok(())
}

/// A registration's result: the new credential.
#[derive(Debug, Clone)]
pub struct NewCredential {
    pub id: Vec<u8>,
    pub key: PublicKey,
    /// The COSE key as sent, to store.
    pub cose: Vec<u8>,
    pub aaguid: Vec<u8>,
    pub counter: u32,
    pub backup: (bool, bool),
}

/// Verify a registration (`navigator.credentials.create`).
pub fn verify_registration(
    client_data: &[u8],
    attestation_object: &[u8],
    e: &Expect<'_>,
) -> Result<NewCredential, String> {
    check_client_data(client_data, e)?;
    let att = cbor(attestation_object)?;
    let auth_data = att
        .text_key("authData")
        .and_then(Cbor::bytes)
        .ok_or("attestation object without authData")?;
    let a = AuthData::parse(auth_data)?;
    check_auth_data(&a, e.rp_id)?;
    let (aaguid, id, cose) = a.attested.clone().ok_or("no attested credential")?;
    let key = PublicKey::from_cose(&cose)?;
    Ok(NewCredential {
        id,
        key,
        cose: encode(&cose),
        aaguid,
        counter: a.counter,
        backup: a.backup(),
    })
}

/// Verify an assertion (`navigator.credentials.get`) with `key`; the new
/// counter.
pub fn verify_assertion(
    client_data: &[u8],
    authenticator_data: &[u8],
    signature: &[u8],
    key: &PublicKey,
    e: &Expect<'_>,
) -> Result<u32, String> {
    check_client_data(client_data, e)?;
    let a = AuthData::parse(authenticator_data)?;
    check_auth_data(&a, e.rp_id)?;
    let mut msg = authenticator_data.to_vec();
    msg.extend_from_slice(&sha256(client_data));
    if !key.verify(&msg, signature) {
        return Err("bad signature".into());
    }
    Ok(a.counter)
}

// ---- encoding (to store COSE keys, and for tests) ----------------------------

fn head(major: u8, n: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | n as u8),
        24..=0xff => out.extend([m | 24, n as u8]),
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend((n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(m | 26);
            out.extend((n as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend(n.to_be_bytes());
        }
    }
}

/// CBOR-encode `v` (definite lengths, as decoded).
pub fn encode(v: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    fn go(v: &Cbor, out: &mut Vec<u8>) {
        match v {
            Cbor::Int(i) if *i >= 0 => head(0, *i as u64, out),
            Cbor::Int(i) => head(1, (-1 - *i) as u64, out),
            Cbor::Bytes(b) => {
                head(2, b.len() as u64, out);
                out.extend(b);
            }
            Cbor::Text(t) => {
                head(3, t.len() as u64, out);
                out.extend(t.as_bytes());
            }
            Cbor::Array(a) => {
                head(4, a.len() as u64, out);
                a.iter().for_each(|x| go(x, out));
            }
            Cbor::Map(m) => {
                head(5, m.len() as u64, out);
                for (k, x) in m {
                    go(k, out);
                    go(x, out);
                }
            }
            Cbor::Bool(false) => out.push(0xf4),
            Cbor::Bool(true) => out.push(0xf5),
            Cbor::Null => out.push(0xf6),
        }
    }
    go(v, &mut out);
    out
}

/// A software authenticator for tests (spec/passkeys.md §5).
#[cfg(test)]
pub mod soft {
    use super::*;

    pub struct Authenticator {
        pub key: p256::ecdsa::SigningKey,
        pub id: Vec<u8>,
        pub counter: u32,
        pub flags: u8,
    }

    impl Default for Authenticator {
        fn default() -> Self {
            Authenticator {
                key: p256::ecdsa::SigningKey::random(&mut rand_core_06()),
                id: crate::auth::random_bytes(16),
                counter: 0,
                flags: FLAG_UP | FLAG_UV,
            }
        }
    }

    /// `p256` 0.13 takes `rand_core` 0.6's RNG.
    fn rand_core_06() -> p256::elliptic_curve::rand_core::OsRng {
        p256::elliptic_curve::rand_core::OsRng
    }

    pub fn client_data(kind: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": kind, "challenge": b64url(challenge), "origin": origin, "crossOrigin": false,
        }))
        .unwrap()
    }

    impl Authenticator {
        pub fn cose(&self) -> Cbor {
            let p = self.key.verifying_key().to_encoded_point(false);
            Cbor::Map(vec![
                (Cbor::Int(1), Cbor::Int(2)),
                (Cbor::Int(3), Cbor::Int(ES256)),
                (Cbor::Int(-1), Cbor::Int(1)),
                (Cbor::Int(-2), Cbor::Bytes(p.x().unwrap().to_vec())),
                (Cbor::Int(-3), Cbor::Bytes(p.y().unwrap().to_vec())),
            ])
        }

        fn auth_data(&self, rp_id: &str, attested: bool) -> Vec<u8> {
            let mut a = sha256(rp_id.as_bytes()).to_vec();
            a.push(self.flags | if attested { FLAG_AT } else { 0 });
            a.extend(self.counter.to_be_bytes());
            if attested {
                a.extend([0u8; 16]);
                a.extend((self.id.len() as u16).to_be_bytes());
                a.extend(&self.id);
                a.extend(encode(&self.cose()));
            }
            a
        }

        /// `(clientDataJSON, attestationObject)` for a registration.
        pub fn register(
            &mut self,
            rp_id: &str,
            challenge: &[u8],
            origin: &str,
        ) -> (Vec<u8>, Vec<u8>) {
            let cd = client_data("webauthn.create", challenge, origin);
            let att = Cbor::Map(vec![
                (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
                (Cbor::Text("attStmt".into()), Cbor::Map(vec![])),
                (
                    Cbor::Text("authData".into()),
                    Cbor::Bytes(self.auth_data(rp_id, true)),
                ),
            ]);
            (cd, encode(&att))
        }

        /// `(clientDataJSON, authenticatorData, signature)` for an assertion.
        pub fn assert(
            &mut self,
            rp_id: &str,
            challenge: &[u8],
            origin: &str,
        ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            use p256::ecdsa::signature::Signer as _;
            self.counter += 1;
            let cd = client_data("webauthn.get", challenge, origin);
            let ad = self.auth_data(rp_id, false);
            let mut msg = ad.clone();
            msg.extend(sha256(&cd));
            let sig: p256::ecdsa::Signature = self.key.sign(&msg);
            (cd, ad, sig.to_der().as_bytes().to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RP: &str = "example.com";
    const ORIGIN: &str = "https://glidex.example.com";

    fn expect<'a>(kind: &'a str, ch: &'a [u8], origins: &'a [String]) -> Expect<'a> {
        Expect {
            kind,
            challenge: ch,
            origins,
            rp_id: RP,
        }
    }

    #[test]
    fn cbor_round_trips_and_refuses_what_webauthn_doesnt_use() {
        let v = Cbor::Map(vec![
            (Cbor::Int(-257), Cbor::Bytes(vec![1; 300])),
            (
                Cbor::Text("x".into()),
                Cbor::Array(vec![Cbor::Bool(true), Cbor::Null, Cbor::Int(70000)]),
            ),
        ]);
        assert_eq!(cbor(&encode(&v)).unwrap(), v);
        assert!(cbor(&[0x5f]).is_err(), "indefinite length");
        assert!(cbor(&[0xc0, 0x00]).is_err(), "a tag");
        assert!(cbor(&[0xf9, 0, 0]).is_err(), "a float");
        assert!(cbor(&[0x01, 0x02]).is_err(), "trailing bytes");
        assert!(
            cbor(&[0x5a, 0xff, 0xff, 0xff, 0xff]).is_err(),
            "longer than the input"
        );
        let deep = [0x81u8; 20]
            .iter()
            .copied()
            .chain([0x00])
            .collect::<Vec<_>>();
        assert!(cbor(&deep).is_err(), "too deep");
    }

    #[test]
    fn a_registration_and_assertions_verify_and_bad_ones_dont() {
        let origins = vec![ORIGIN.to_string()];
        let mut a = soft::Authenticator::default();
        let ch = [7u8; 32];
        let (cd, att) = a.register(RP, &ch, ORIGIN);
        let cred =
            verify_registration(&cd, &att, &expect("webauthn.create", &ch, &origins)).unwrap();
        assert_eq!(cred.id, a.id);
        assert_eq!(cred.key.alg(), ES256);
        let stored = PublicKey::from_cose(&cbor(&cred.cose).unwrap()).unwrap();
        // Wrong origin, challenge, RP and type are refused.
        assert!(
            verify_registration(&cd, &att, &expect("webauthn.create", &[8u8; 32], &origins))
                .is_err()
        );
        assert!(
            verify_registration(
                &cd,
                &att,
                &expect("webauthn.create", &ch, &["https://evil.example".into()])
            )
            .is_err()
        );
        assert!(verify_registration(&cd, &att, &expect("webauthn.get", &ch, &origins)).is_err());
        let (cd2, att2) = a.register("other.example", &ch, ORIGIN);
        assert!(
            verify_registration(&cd2, &att2, &expect("webauthn.create", &ch, &origins)).is_err()
        );

        let ch = [9u8; 32];
        let (cd, ad, sig) = a.assert(RP, &ch, ORIGIN);
        let e = expect("webauthn.get", &ch, &origins);
        assert_eq!(verify_assertion(&cd, &ad, &sig, &stored, &e).unwrap(), 1);
        let mut bad = sig.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(
            verify_assertion(&cd, &ad, &bad, &stored, &e).is_err(),
            "a bad signature"
        );
        let other = soft::Authenticator::default();
        let other_key = PublicKey::from_cose(&other.cose()).unwrap();
        assert!(
            verify_assertion(&cd, &ad, &sig, &other_key, &e).is_err(),
            "another key"
        );
        // Without user verification.
        a.flags = FLAG_UP;
        let (cd, ad, sig) = a.assert(RP, &ch, ORIGIN);
        assert!(verify_assertion(&cd, &ad, &sig, &stored, &e).is_err());
    }

    #[test]
    fn cose_keys_of_each_kind() {
        let ed = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let cose = Cbor::Map(vec![
            (Cbor::Int(1), Cbor::Int(1)),
            (Cbor::Int(3), Cbor::Int(EDDSA)),
            (Cbor::Int(-1), Cbor::Int(6)),
            (
                Cbor::Int(-2),
                Cbor::Bytes(ed.verifying_key().to_bytes().to_vec()),
            ),
        ]);
        let k = PublicKey::from_cose(&cose).unwrap();
        use ed25519_dalek::Signer as _;
        assert!(k.verify(b"hi", &ed.sign(b"hi").to_bytes()));
        assert!(!k.verify(b"ho", &ed.sign(b"hi").to_bytes()));
        // An unknown algorithm, and a small RSA key.
        let bad = Cbor::Map(vec![
            (Cbor::Int(1), Cbor::Int(2)),
            (Cbor::Int(3), Cbor::Int(-36)),
        ]);
        assert!(PublicKey::from_cose(&bad).is_err());
        let small = Cbor::Map(vec![
            (Cbor::Int(1), Cbor::Int(3)),
            (Cbor::Int(3), Cbor::Int(RS256)),
            (Cbor::Int(-1), Cbor::Bytes(vec![0xff; 64])),
            (Cbor::Int(-2), Cbor::Bytes(vec![1, 0, 1])),
        ]);
        assert!(PublicKey::from_cose(&small).is_err());
    }
}

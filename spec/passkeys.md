# Passkeys (design)

Status: **implemented** (§7). A user of the local directory signs in with a
**passkey** (WebAuthn, a platform or security-key credential) as one more
of its sign-in methods ([tenancy.md](tenancy.md) §6.2), alongside a local
password or an IdP connector. Passkeys are made and used in the web UI;
gxctl keeps its token, PAM and OIDC device logins.

## 1. Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| P1 | What a passkey is | An identity of provider `passkey` (subject: the credential id, base64url) linked to one user, like any sign-in method (tenancy.md D17), plus its **public key** in `passkeys` (directory scope): the COSE key (ES256, EdDSA or RS256), the signature counter, a name the user gives it, its AAGUID, transports and backup flags, `created_at`, `last_used_at`. No attestation is trusted: glidex checks that the same key signs later, not which maker built it. | A passkey is reached and removed the way every other sign-in method is; one list of sign-in methods per user. Trusting attestation would need a maker allowlist nobody asked for. |
| P2 | Which domain | Passkeys belong to the **relying-party id** `auth.passkeys.rp_id` (a domain, e.g. `example.com` or the tailnet's `<tailnet>.ts.net`). A ceremony is accepted from an `https` origin (or `http://localhost`, which browsers treat as secure) whose host is that domain or under it, and which is the origin the request came from (the web UI's own name, `GLIDEX_UI_HOSTS`, or `auth.allowed_origins`). Without the setting, the RP id is the web UI's host name in each request, and the Account page says passkeys made there work under that name only. `auth.passkeys.enabled: false` turns them off. | Passkeys made on one name of the web UI keep working on its other names and on a cluster's other servers. |
| P3 | Signing in | **Discoverable** credentials: the login page offers "Sign in with a passkey" with no user name; the authenticator picks the passkey and returns the user handle. User verification is **required** (PIN, fingerprint, face) at both registration and sign-in, and a passkey sign-in **counts as multi-factor** (`mfa`, D22, and for step-up). A disabled user is refused; a counter that goes backwards (both non-zero) is refused and audited as a possible clone. | No account names to type or enumerate; with verification, a passkey is possession plus a factor. |
| P4 | Adding and removing | Only the user adds its own passkeys, signed in, as a step-up action (`manageOwnSignIns`, tenancy.md §6.2), at most 10. An administrator may remove a user's passkeys (`manageUsers`), never add one. A user may be left with passkeys only if it has **at least two**: removing a sign-in method that would leave one passkey and nothing else is `409 last_identity`, as is removing a user's last method. Passkeys can be renamed. | Losing one device must not lock its person out; nobody can plant a passkey on another person's account. |
| P5 | Step-up | A passkey can answer the re-authentication a step-up action asks for: the prompt's passkey sign-in is a fresh login, as a password one is. | A user who signs in with a passkey must be able to pass step-up the same way. |
| P6 | Transport | The ceremonies are HTTP-only (`/auth/passkeys/…`, like the OIDC callbacks, tenancy.md D29); listing, renaming and removing are `UserService` operations on both transports. | WebAuthn is a browser protocol; its options and responses are its own JSON. |

## 2. Ceremonies

Challenges are 32 random bytes, single use, valid for 5 minutes, and
bound to the browser by a `Lax` cookie (`glidex_webauthn`), as the OIDC
login state is.

**Registration** (signed in, step-up):

1. `POST /auth/passkeys/register/start {name}` → `PublicKeyCredentialCreationOptions`:
   `rp {id, name}`, `user {id: the user's passkey handle, name, displayName}`,
   the challenge, `pubKeyCredParams` (ES256 −7, EdDSA −8, RS256 −257),
   `authenticatorSelection {residentKey: required, userVerification: required}`,
   `attestation: none`, `excludeCredentials` (the user's passkeys), timeout.
   The handle is 32 random bytes per user, made at its first passkey and
   never the user id.
2. The browser calls `navigator.credentials.create`.
3. `POST /auth/passkeys/register/finish {credential}`: `clientDataJSON`
   (`type: webauthn.create`, the challenge, an accepted origin, P2) and
   `attestationObject` (CBOR): `authData` with the SHA-256 of the RP id,
   flags UP and UV set, attested credential data with an allowed key
   type. The attestation statement is not checked (P1). The credential id
   must be new. `201 {passkey}`.

**Sign-in** (public):

1. `POST /auth/passkeys/login/start` → `PublicKeyCredentialRequestOptions`:
   the challenge, `rpId`, `userVerification: required`, no
   `allowCredentials`.
2. The browser calls `navigator.credentials.get`.
3. `POST /auth/passkeys/login/finish {credential}`: the passkey by its id,
   the user handle the authenticator returned matching its user's, the
   client data (`webauthn.get`, the challenge, an accepted origin), the
   authenticator data (the RP id hash, UP and UV), the signature over
   `authData ‖ SHA-256(clientDataJSON)` with the stored key, and the
   counter (P3). Then a session as at any login (method `passkey`, `mfa`;
   a fresh one also answers step-up, P5), and the page keeps its CSRF
   value from the answer. Any failure is `401 passkey_refused`, the same
   for every reason; the reason goes to the audit log.

Rate limit: 10 sign-in attempts a minute per client address: the TCP
peer, or the browser's address glidex-ui (or a relaying server) passes
in `X-Forwarded-For`. A header a client sends to `:8841` itself is
ignored, so it can't pick its own bucket.

## 3. API

- `GET /auth/methods` says whether passkeys are on, and the RP id.
- `UserService.ListPasskeys` (`GET /users/{id}/passkeys`: one's own with
  `manageOwnSignIns`, anyone's with `listUsers`), `RenamePasskey`
  (`PATCH /users/{id}/passkeys/{passkey}`, one's own), and
  `DeletePasskey` (`DELETE …`, one's own or `manageUsers`; P4). A
  passkey is also listed among the user's identities, labelled
  "Passkey: `<name>`".

## 4. UI

- Login page: **Sign in with a passkey** when they are on.
- Account page: **Passkeys**: the user's passkeys (name, made, last
  used), **Add a passkey** (a name, then the browser's prompt; a fresh
  sign-in first if needed), rename, remove; where the RP id isn't set,
  a note that they work under this name only.
- The re-authentication prompt offers the user's passkey as well as its
  other methods.
- Administrators see a user's passkeys among its sign-in methods and
  can remove them.

## 5. Tests

- Unit: CBOR and COSE parsing, `authData` flags and counters, client
  data checks; every key type.
- Integration with a software authenticator (P-256 and Ed25519):
  register and sign in; a wrong origin, RP id, challenge (and a reused
  one), missing UV, a backwards counter, another user's handle, and a
  disabled user are all refused with the same answer; a passkey sign-in
  counts as MFA and passes step-up; P4's removal rules.
- UI e2e with Chromium's virtual authenticator (`WebAuthn.addVirtualAuthenticator`):
  add a passkey on the Account page, sign out, sign in with it.

## 6. Tasks

1. This spec.
2. The server: `auth/webauthn.rs` (CBOR, COSE keys, client data,
   authenticator data, signature checks), `passkeys` (directory scope),
   the ceremonies, sessions and step-up, `auth.passkeys` config, the
   `UserService` operations and P4's rules; tests with a software
   authenticator.
3. The UI and docs: the login button, the Account page's Passkeys, the
   re-authentication prompt, the labels; the operator guide (`rp_id`);
   UI e2e with a virtual authenticator.

## 7. Implementation status

| Spec | Code |
|---|---|
| WebAuthn checks | `auth/webauthn.rs`: strict CBOR (definite lengths, no tags or floats, bounded nesting), COSE keys (ES256 on P-256, EdDSA on Ed25519, RS256 of 2048 bits or more), client data (type, challenge, origin, not cross-origin), authenticator data (RP id hash, UP, UV, counter, attested credential), signatures; pure Rust, no OpenSSL |
| P1 | `auth/passkeys.rs`: `PasskeyStore` (table `passkeys`, directory scope: credentials and each user's 32-byte handle), identities of provider `passkey` |
| P2 | `auth.passkeys {enabled, rp_id, rp_name}`; `api/passkeys.rs::origin_and_rp`: the request's own `https` origin, under the RP id (or its host) |
| §2 ceremonies | `/auth/passkeys/register/{start,finish}` (signed in, `manageOwnSignIns`, step-up, at most 10) and `/auth/passkeys/login/{start,finish}` (public; 10 attempts a minute per client); challenges single use, 5 minutes, bound by the `glidex_webauthn` cookie (`Ceremonies`) |
| P3 | `Method::Passkey`, a session with `mfa`; one `401 passkey_refused` for every failure (an unknown passkey, another origin, no UV, a counter going backwards, a replay, a disabled user), the reason in the audit log |
| P4 | `UserService.{ListPasskeys, RenamePasskey, DeletePasskey}` (`/users/{id}/passkeys…`); `keeps_a_way_in` for them and `UnlinkIdentity`: never no method, never one passkey alone; deleting a user forgets its passkeys |
| Tests | `webauthn.rs` unit tests; `tests/passkey_tests.rs` with a software authenticator |
| UI (§4) | `components/PasskeyButton.tsx` (the login page and the re-authentication dialog), `components/PasskeysCard.tsx` on the Account page (add, rename, remove, the RP id note), "Passkey" among sign-in methods; `api.ts` converts WebAuthn's binary fields |
| Docs | the operator guide's "Passkeys" (`auth.passkeys.rp_id`) |
| UI e2e | `passkeys.spec.ts`: a passkey added on the Account page with Chromium's virtual authenticator, then a sign-in with it |

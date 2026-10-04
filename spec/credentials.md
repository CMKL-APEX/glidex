# Credential Store

> Credentials belong to a project and are keyed by `(project, username)`;
> a VM only uses credentials of its own project (spec/security.md §6.2).
> Records from before projects moved to the default project on upgrade.

Source: `crates/glidex-control-plane/src/credentials.rs`. Stored guest
logins that the generated cloud-init seed provisions on a firmware-booted
VM's first boot (see [hypervisors.md](hypervisors.md#firmware-boot)).

## Model

```rust
pub struct Credential {
    pub username: String,              // table key
    pub password_hash: Option<String>, // SHA-512-crypt, "$6$rounds=5000$<16>$<86>"
    pub ssh_authorized_keys: Vec<String>,
    pub created_at: u64,               // unix seconds
    pub updated_at: u64,
}
```

Stored as serde-JSON in the `credentials` table of the same ReDB file as
`vms` (`VmStore::database()` hands out the shared `Arc<Database>`).

**Invariant.** A credential always has a password hash, at least one SSH
key, or both. `create` and `update` both enforce it.

## Security rules

- **Hash only.** The plaintext password is hashed on arrival
  (`hash_password`) and dropped; it is never stored, logged or returned.
  cloud-init only needs the hash (`chpasswd` with `type: hash`).
- **Hash never leaves the server.** Every endpoint returns
  `CredentialInfo` (`username`, `has_password`, `ssh_authorized_keys`,
  timestamps) — no hash. `Credential`, `SeedConfig` and the request
  types have hand-written `Debug` impls that print `[REDACTED]`.
- **Owner-only files.** `VmStore::open` sets the database to `0600`;
  `cloud_init::write_seed_image` creates the staging dir `0700` and the
  seed image and its files `0600`, since both hold hashes.
- **Least privilege in the guest.** A VM with a credential authorizes
  only that credential's SSH keys (`SeedConfig::for_credential`); a VM
  without one has no login at all. There are no host-wide default keys
  or passwords.
- **Prefilling keys.** The web UI fills a new credential with the
  user's own `~/.ssh/*.pub` (`GET /users/me/ssh-keys`, read by
  glidex-authd, spec/security.md §5.3); gxctl reads them locally and
  offers them as the default.
- **Transport.** The API is authenticated and served over HTTPS by
  default (plain HTTP only on loopback, spec/security.md §5.1); the
  password crosses it once, at create/update time. Run the control plane on a trusted host/network.

### Why a custom salt

`hash_password` draws 12 bytes from `getrandom`, which Base64-encode to
exactly 16 salt characters, and calls `hash_password_with_salt`.
`ShaCrypt::hash_password` (sha-crypt 0.6) writes a 22-character salt
into the string but, like glibc, hashes with only the first 16. glibc's
`crypt()` then returns a 16-character-salt string that never compares
equal to the stored one, so the guest rejects every password login.
`credentials::tests::stores_only_a_verifiable_sha512_hash` pins the
salt length.

## Validation

- `username`: `^[a-z_][a-z0-9_-]{0,31}$`, and not a reserved system
  account (`root`, `daemon`, `nobody`, `ubuntu`, …) that cloud-init would
  silently modify instead of creating.
- `password`: 8–256 characters, no control characters.
- `ssh_authorized_keys`: single-line OpenSSH public keys (`ssh-*`,
  `ecdsa-sha2-*`, `sk-*`); blank entries dropped.

Violations → `CredentialError::Invalid` → `400 invalid_credential`.

## Use by VMs

`VmConfig.credential: Option<String>` names a credential by username.

- `create_vm` requires firmware boot without a custom `cloud_init_path`
  (the credential only reaches the guest through the generated seed) and
  that the credential exists; otherwise `400 invalid_config`. The check
  runs under the VM-map write lock.
- `start_vm` loads the credential and builds the seed with
  `SeedConfig::for_credential`: guest user = username, password hash via
  both `users[].passwd` and `chpasswd`, the credential's keys.
- `delete_credential` holds the VM-map read lock and refuses with
  `409 conflict` (naming the VMs) while any VM references it, so a VM
  never points at a missing credential.

**Invariant.** Updates reach a VM only on its first boot. cloud-init
provisions users once per `instance-id`, and a VM's instance-id is its
id for life. Changing a password afterwards does not change the login
of an already-provisioned guest.

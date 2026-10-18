# Tailscale certificates for the web UI

Status: **implemented (TS0–TS2)**; TS3 waits on tailscale-rs (§6).
The open questions are settled as decisions T6–T11 (§8).
Extends [security.md](security.md) §5.1 and §5.1.1 and
[web-ui.md](web-ui.md) *Server*. The edits it needs in those documents
and in [installer.md](installer.md) are applied; §11 is their change
record.

The web UI serves HTTPS with a self-signed certificate unless the
operator supplies one (security.md §5.1.1). Every user's first visit is
a browser warning, a fingerprint comparison most people skip, and a
"proceed anyway" that trains them to click through warnings. Many glidex
hosts are already on a Tailscale tailnet, and Tailscale issues Let's
Encrypt certificates for a node's `<node>.<tailnet>.ts.net` name. This
document makes the UI serve that certificate to browsers that reach it
by that name, without an operator handling certificate files and with
renewal in place.

## 1. Scope

- **In:** `glidex-ui` (port 5173), the only listener browsers use. They
  never reach port 8841: the UI proxies `/api`, and OIDC redirect URIs
  are on the UI origin (security.md §5.4).
- **In:** reloading a certificate without a restart (§4.1), which
  operator-supplied certificates get too.
- **Out:** the control plane's port 8841. Its clients (gxctl,
  scripts) pin or are given a CA (gxctl-auth.md §5); see T8.
- **Out:** reaching glidex from the public internet. A ts.net
  certificate is browser-trusted, but the name only resolves and routes
  inside the tailnet. Tailscale Funnel would publish it to the internet;
  glidex does not configure it (§7).

## 2. Decisions

| # | Topic | Decision |
|---|---|---|
| T1 | Source | Get the certificate from the host's **`tailscaled`** through its local API now. **tailscale-rs** (Tailscale's Rust library for embedding a node, preview) does not issue certificates; it is a later phase (§6). |
| T2 | Who fetches | `glidex-ui` itself, over `/run/tailscale/tailscaled.sock`, as user `glidex-ui` granted by `tailscaled`'s `TS_PERMIT_CERT_UID`. The key stays in the UI's memory: no key file of its own, no copy shared with another process. |
| T3 | Which connections | Selected per handshake by SNI: a ts.net certificate domain gets the Tailscale certificate; every other name or address keeps today's certificate (self-signed or configured). LAN users and existing pins are unaffected. |
| T4 | Opt-in | Off by default (`GLIDEX_UI_TLS_TAILSCALE=off`). Turning it on publishes the node's ts.net name in Certificate Transparency logs (§7), so it is the operator's choice, offered by the installer. |
| T5 | Failure | Never a failed handshake because of Tailscale: without a usable Tailscale certificate, ts.net names get the default certificate and a warning is logged. A held certificate is kept until it expires. |
| T6 | `TS_PERMIT_CERT_UID` | The installer writes a `tailscaled.service` drop-in setting it to `glidex-ui`, **only after the operator accepts the prompt**, and refuses when `tailscaled` already has it set to another user. A non-interactive run does not turn the feature on unless asked (`--tailscale-cert`); a previous install's choice is kept on updates (§4.6). |
| T7 | Control plane `Origin` | From the `glidex-ui` peer on `ui.sock` only, an `Origin` equal to `https://` + `X-Forwarded-Host` is accepted, since the UI has already checked that `Host` against its allowlist (§4.3). |
| T8 | Port 8841 | Not in scope; the control plane keeps its own certificate. |
| T9 | Port | The UI stays on 5173. |
| T10 | HSTS on ts.net names | `max-age=86400` (one day) in the first release (§4.4). |
| T11 | Older `tailscaled` | Certificate domains come from `cert-domains`, falling back to `CertDomains` in `status` when that endpoint is missing (`404`). |

## 3. What `tailscaled` provides

Verified against `tailscale/tailscale` `main` (October 2026):

- `GET /localapi/v0/cert/<domain>?type=pair` returns the key PEM
  followed by the certificate chain PEM (`ipn/localapi/cert.go`).
  `min_validity=<Go duration>` forces a synchronous renewal when the
  cached certificate would expire sooner. Rate-limit errors are `429`
  with `Retry-After`.
- Without `min_validity`, `tailscaled` returns its cached certificate
  and, when renewal is due (ACME Renewal Information, else expiry-based),
  **starts an asynchronous renewal**; the next request gets the new one
  (`feature/acme/cert.go`). It renews on its own only for Tailscale
  Serve's HTTPS entries (`feature/acme/refresh.go`), so **a consumer
  that never asks never gets a renewal**: glidex must poll.
- Fetching a certificate requires write access to the local API (root
  or the operator user) **or** a peer uid equal to
  `TS_PERMIT_CERT_UID`, an environment variable of `tailscaled`
  holding **exactly one** uid or user name
  (`ipn/ipnserver/server.go`, `CanFetchCerts`).
- `GET /localapi/v0/cert-domains` (read access, which any local user
  has) lists the node's certificate domains; older daemons only report
  them as `CertDomains` in `GET /localapi/v0/status`. The list is empty
  when HTTPS certificates are disabled for the tailnet (admin console →
  DNS → HTTPS Certificates).
- The local API is HTTP/1.1 on the Unix socket; the `Host` header is
  `local-tailscaled.sock`.

## 4. Design

### 4.1 A reloadable certificate (`glidex-tls`)

Today `glidex_tls::server_config` builds a rustls `ServerConfig` with
`with_single_cert` once at startup (control plane `serve.rs`, UI
`main.rs`). Replace that with a resolver:

- `CertSlot`: an `ArcSwap<CertifiedKey>`. `Resolver`
  (`rustls::server::ResolvesServerCert`) holds the default slot plus
  zero or more `(domain, CertSlot)` entries, matched on SNI exactly
  (ASCII case-insensitive). No SNI, an IP address, or no match → the
  default slot.
- File-backed slots (a configured `cert`/`key`) are re-read when the
  certificate file's modification time or size changes, checked every
  minute, the same rule web-ui.md applies to trusted files. A file that
  fails to load or whose key doesn't match is logged and the old pair
  kept. Self-signed slots are unchanged (generated, kept 5 years).
- Invariant: a slot never holds a pair whose key doesn't match its leaf
  (`CertifiedKey::keys_match`), and a swap never drops connections
  already established.

Both services use the resolver. It also lets an operator renew their
own certificate without a restart (operator-guide `tls.md`,
*Renewing*), with one exception: a key given as a systemd credential
(`LoadCredential=`, the documented way) is copied into
`$CREDENTIALS_DIRECTORY` when the service starts, so a renewal that
changes the key still needs a restart; the reload warning says so. The
Tailscale slot (§4.2) never touches files, so it is unaffected.

The control plane re-publishes `<run dir>/tls.crt` after a reload, so
local clients trust the new certificate; `GET /auth/server-info` reads
the certificate file per request and needs no change.

Implementation: `glidex_tls::{CertSlot, Resolver, FileCert,
resolver_config, certified_key}`; a file's identity for change
detection is device, inode, size and modification time, so
write-then-rename and in-place rewrites are both seen.

### 4.2 The Tailscale slot (`glidex-ui`)

With `GLIDEX_UI_TLS_TAILSCALE=on` (and TLS not `off`):

1. At startup, and on every poll, read the certificate domains (§3).
   Wildcard entries (`*.…`) are ignored. Missing socket, `403`, or an
   empty list → log once per state change and keep polling (T5).
2. For each domain, `GET /localapi/v0/cert/<domain>?type=pair`; parse
   the key and chain; on success put them in that domain's slot.
3. **Poll** every 6 hours, with ±10 % jitter. If the certificate
   returned has less than 14 days left, ask again with
   `min_validity=336h` (a synchronous renewal). A `429` sets the next
   attempt to `Retry-After`; other errors retry with backoff from 1
   minute to the poll interval.
4. The startup fetch runs in the background: the UI listens at once
   with the default certificate, so a slow ACME issuance (seconds to a
   minute) never delays the UI.
5. A domain that disappears from the list (node or tailnet renamed)
   loses its slot at the next poll; a new one gains one.

A held certificate whose expiry passes without a renewal is dropped at
the next poll, so that name falls back to the default certificate
rather than an expired one. The certificate's fingerprint and expiry
are logged when it changes; errors are logged when they change, not on
every retry.

Implementation: `crates/glidex-ui/src/tailscale.rs`. The UI checks the
SNI of each finished handshake against the held names to mark the
connection for HSTS (§4.4), so HSTS follows the certificate actually
chosen, not the `Host` header.
The key is never written to disk by glidex (`tailscaled` keeps its own
copy under `/var/lib/tailscale/certs`).

### 4.3 Names: `Host` and `Origin`

- **UI `Host` check** (security.md §5.6): the current certificate
  domains are allowed hosts. They are read at request time from the
  Tailscale state above, so this works with the default list *and* an
  explicit `GLIDEX_UI_HOSTS`. That name is one the UI holds a
  certificate for, so it is not a rebinding risk.
- **Control plane `Origin` check (T7):** its default
  `auth.allowed_origins` is computed from the host's local names and
  doesn't know the ts.net name. The control plane also accepts, **from
  the `glidex-ui` peer on `ui.sock` only**, an `Origin` equal to
  `https://` + `X-Forwarded-Host` (scheme, host and port compared as
  the browser serializes them; ASCII case-insensitive host). The UI
  overwrites `X-Forwarded-Host` with the request's `Host`, which it has
  already checked against its allowlist, so this is a same-origin check
  and admits no name the UI wouldn't serve. It applies with the
  feature off too, which also covers custom names in an explicit
  `GLIDEX_UI_HOSTS`. Over TCP, and from any other peer, only
  `auth.allowed_origins` counts, as before.

### 4.4 HSTS

`Strict-Transport-Security: max-age=86400` (T10) is sent on
connections that were served the Tailscale certificate, as with any
configured certificate (security.md §5.1.1) but shorter. HSTS applies
to every port of the name, so a browser that later opens
`https://<node>.ts.net:8841` (where the control plane serves its
self-signed certificate) gets a non-bypassable error. Browsers have no
reason to go there (§1), and one day bounds the damage while the
feature is new; raising it to a year is a later, separate change.

### 4.5 Permissions and sandbox

- `glidex-ui.service` already allows `AF_UNIX` and doesn't hide
  `/run/tailscale`. `tailscaled.sock` is `0666`, and `ProtectSystem=strict`
  doesn't prevent `connect()` on a read-only mount. **No unit change.**
- `tailscaled` must run with `TS_PERMIT_CERT_UID=glidex-ui`. The
  installer arranges it (T6, §4.6); an operator may also set it by hand.
  The UI only needs the permission to exist: it never checks how it was
  granted.

### 4.6 Installer

Options `--tailscale-cert` / `--no-tailscale-cert`, saved in
`install.conf` like the other options. The setting is decided in this
order:

1. The command-line option, when given.
2. Otherwise the saved choice of a previous install, so an update (and
   a non-interactive re-run) keeps the feature as it was.
3. Otherwise, interactively (stdin and stdout are terminals) and only
   when `tailscale status` reports certificate domains: a prompt,
   default **no**, explaining the Certificate Transparency exposure and
   the `tailscaled` restart. It is asked before the build, while the
   operator is at the terminal. The answer is saved either way, so a
   "no" isn't asked again (`--tailscale-cert` turns it on later).
4. Otherwise (non-interactive, nothing saved): off, and nothing saved,
   so a later interactive run still asks.

When the result is on:

- **`tailscaled` (T6).** Its configured `TS_PERMIT_CERT_UID`:
  `systemctl show tailscaled -p Environment`, overridden by its
  `EnvironmentFiles` (e.g. `/etc/default/tailscaled`), later files
  winning, as systemd applies them:
  - already `glidex-ui` (or its uid), by glidex's drop-in or by hand:
    nothing to write; `tailscaled` is restarted only if the running
    daemon's environment (`/proc/<MainPID>/environ`) lacks it;
  - set to another user: **refuse** — say which user holds it, leave
    `tailscaled` untouched, and leave the UI setting off for this run.
    `install.conf` keeps its previous value (this run's choice isn't
    saved as on), so the operator can resolve it and re-run;
  - unset: write `/etc/systemd/system/tailscaled.service.d/50-glidex-cert.conf`
    (`[Service]` + `Environment=TS_PERMIT_CERT_UID=glidex-ui`, with a
    comment naming glidex), `daemon-reload`, and restart `tailscaled`
    after saying the tailnet connection drops for a few seconds.
- **No `tailscaled`** (`tailscale status` fails): treated like a
  refusal: said, nothing written, previous value kept.
- **UI.** `GLIDEX_UI_TLS_TAILSCALE=on` in
  `/etc/systemd/system/glidex-ui.service.d/50-tailscale-cert.conf`; the
  UI is restarted when that file changes. `tailscaled` is restarted
  (and waited for, up to 30 s, until `Running`) before the UI, so the
  UI's first fetch finds the permission.
- Both drop-ins carry the `Managed by glidex-install` marker; only
  marked files are ever removed.
- **Blurb.** `https://<node>.<tailnet>.ts.net:5173` first, without a
  fingerprint; LAN URLs and fingerprints follow as today. The
  installer checks it as a browser would: a TLS handshake to
  `127.0.0.1:5173` with that SNI, verified against the system trust
  store, retried for up to a minute (a first issuance takes about
  40 s). If it doesn't verify by then, the blurb says so and points to
  `journalctl -u glidex-ui`.

When the result is off and a previous install had turned it on
(`--no-tailscale-cert`), both drop-ins written by glidex are removed
(each directory too, if that leaves it empty) and `tailscaled`
restarted; a `TS_PERMIT_CERT_UID` set by hand is left alone. Uninstall
does the same (`systemctl try-restart tailscaled` after its
`daemon-reload`) and keeps an administrator's other drop-ins.

Implementation: `crates/glidex-install/src/tailscale.rs` (decisions as
pure functions, tested), wired into `main.rs` (`install_services`,
`print_usage`) and `uninstall.rs` (`plan`).

Because the UI only needs the permission to exist (§4.5), a host where
the operator granted it by hand works with the UI drop-in alone.

## 5. Configuration

| Variable (`glidex-ui`) | Default | Meaning |
|---|---|---|
| `GLIDEX_UI_TLS_TAILSCALE` | `off` | `on`: also serve the node's Tailscale certificate for its ts.net names (§4.2). Refused at startup with `GLIDEX_UI_TLS=off`. |
| `GLIDEX_UI_TAILSCALE_SOCKET` | `/run/tailscale/tailscaled.sock` | The `tailscaled` local API socket. |

Nothing changes in `control-plane.json`. Installer options: §4.6.

## 6. Later: embedding tailscale-rs

[tailscale-rs](https://github.com/tailscale/tailscale-rs) (crate
`tailscale`, BSD-3-Clause) lets a process join a tailnet as its own
node, with no `tailscaled` on the host. As of its April 2026 preview it
lists HTTPS certificates, Serve and Funnel as unsupported, relays all
traffic through DERP (no direct connections), makes no compatibility
promises, has not been externally audited, and Tailscale advises
against production use.

When it gains certificate issuance, a cargo feature could let
`glidex-ui` join as its own node (its own tags and ACLs, a name like
`glidex-<host>`) and serve its tailnet listener from the same slot
mechanism (§4.1). Starting that work is gated on upstream: certificate
support, direct connections, and a stability statement.

## 7. Security considerations

- **Certificate Transparency.** Every Let's Encrypt certificate is
  logged publicly, so `<node>.<tailnet>.ts.net` becomes public. Tailscale
  offers a randomly generated tailnet DNS name that reveals less; the
  installer prompt and the operator guide recommend it. This is why the
  feature is opt-in (T4).
- **Reachability doesn't change.** The UI listens on the same addresses
  as before; the name only resolves for tailnet members (MagicDNS).
  Tailscale ACLs remain the operator's tool to restrict who can reach
  port 5173.
- **No Funnel.** glidex never turns on Funnel or Serve. An operator who
  does so publishes a hypervisor's login page to the internet; the
  operator guide says not to.
- **Key handling.** The key exists in `tailscaled`'s state and in the
  UI's memory. `TS_PERMIT_CERT_UID=glidex-ui` lets the `glidex-ui` user
  fetch the certificate *and key* for every certificate domain of this
  node, and nothing else in the local API.
- **Trust in `tailscaled`.** The UI trusts what the socket returns. The
  socket path is root-owned (`/run/tailscale`, `0755`), and the pair is
  checked (`keys_match`, the leaf's SAN covers the domain, not expired)
  before use.

## 8. Settled questions

Each was an open question in the draft; the answer is now a decision
in §2.

| Question | Answer |
|---|---|
| Who grants `TS_PERMIT_CERT_UID`, which holds one uid? | The installer, behind its prompt, refusing when another user holds it; updates keep a previous choice (T6, §4.6). Rejected: printing instructions only; a root fetch helper (a new privileged component). |
| How does the control plane accept the ts.net `Origin`? | Same-origin against `X-Forwarded-Host`, from the UI peer only (T7). Rejected: the control plane querying `tailscaled` itself (couples it to Tailscale); manual `auth.allowed_origins` (breaks on rename). |
| Port 8841 too? | No (T8). Pins already work for gxctl; the one-uid limit would need a second fetcher. |
| Port 443? | No, 5173 (T9). 443 needs `CAP_NET_BIND_SERVICE` or a sysctl. |
| HSTS on ts.net names? | One day for now (T10). |
| `tailscaled` without `cert-domains`? | Fall back to `status` (T11). |

## 9. Testing

- `glidex-tls`: resolver selects by SNI (match, case, no SNI, IP,
  unknown); a swap is seen by the next handshake and not by an
  established one; a file slot reloads on change and keeps the old pair
  on a bad file or mismatched key.
- `glidex-ui` against a fake `tailscaled` on a Unix socket (rcgen-made
  pairs): startup without the socket, `403`, empty domains, `429` with
  `Retry-After`, renewal below 14 days with `min_validity`, a domain
  rename, a chain whose SAN doesn't cover the domain (rejected). `Host`
  allowlist includes the domain while it is held. HSTS `max-age=86400`
  only on connections served the Tailscale certificate.
- Control plane: from the `glidex-ui` peer, `Origin` equal to
  `https://` + `X-Forwarded-Host` is accepted (port and case variants
  per T7), a different one refused; over TCP the same pair is refused
  unless listed in `auth.allowed_origins`.
- Installer: the four-step precedence of §4.6; `TS_PERMIT_CERT_UID`
  unset / ours / another user's (refusal, nothing changed, not saved);
  `--no-tailscale-cert` removes only glidex's drop-ins.
- Host test (manual, on a tailnet with HTTPS enabled): a browser with
  no warning on `https://<node>.<tailnet>.ts.net:5173`, LAN URL still
  the self-signed certificate, login and console work, HSTS header only
  on the ts.net name.

  Done for TS1 on 2026-10-18 against `tailscaled` 1.102.4, the UI as a
  transient unit with `glidex-ui.service`'s user and sandbox. Without
  `TS_PERMIT_CERT_UID` the socket was reachable from the sandbox, the
  domains were read and the fetch got `403` with the hint logged; the
  ts.net name got the self-signed certificate. After the grant and a
  `tailscaled` restart (back to `Running` in 6 s), the next retry
  obtained a Let's Encrypt certificate (the first issuance took about
  40 s inside one request) and `curl` verified it with only the system
  trust store; HSTS `max-age=86400` on that connection only; `localhost`
  kept the self-signed certificate without HSTS; the ts.net `Host` was
  accepted over the `localhost` certificate, an unknown one got `421`;
  `/api` reached the control plane through `ui.sock`. Not covered live:
  renewal (a fresh certificate is valid 90 days) and the T7 `Origin`
  rule (the host's installed control plane predates it; covered by
  `security_tests.rs`).

## 10. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **TS0** reloadable certificates *(done)* | `CertSlot` and SNI `Resolver` in `glidex-tls`; control plane and UI use them; configured certificate files reload on change | §9 `glidex-tls` tests; operator guide drops "restart to renew". |
| **TS1** Tailscale slot in the UI *(done)* | `GLIDEX_UI_TLS_TAILSCALE`, local API client, polling and renewal, `Host` allowlist, HSTS, the T7 Origin rule | §9 `glidex-ui` tests; host test. |
| **TS2** installer *(done)* | Option precedence, prompt, `tailscaled` drop-in with refusal (T6), UI drop-in, usage blurb, uninstall, operator guide section | Installer tests; host test from a clean install. |
| **TS3** *(gated upstream)* | Embedded tailscale-rs node (§6) | — |

## 11. Changes to other specs (applied — change record)

- **security.md:** decision 19; §5.1 certificate table, row
  "Tailscale"; §5.1.1 HSTS bullet; §5.6 `Host` and `Origin` checks;
  §17 status row.
- **web-ui.md:** *Server* environment table rows for
  `GLIDEX_UI_TLS_TAILSCALE` and `GLIDEX_UI_TAILSCALE_SOCKET`; the HSTS
  sentence.
- **installer.md:** the `--tailscale-cert` option, step 13's
  Tailscale step and usage blurb, uninstall.
- **README.md, SUMMARY.md:** this document's entry.

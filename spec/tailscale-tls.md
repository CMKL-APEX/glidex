# Tailscale certificates for the web UI

Status: **draft; open questions in §8, no code yet** (milestones §10).
Extends [security.md](security.md) §5.1 and §5.1.1 and
[web-ui.md](web-ui.md) *Server*. The edits it needs in those documents
and in [installer.md](installer.md) are applied and tagged *(draft)*;
§11 is their change record.

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
- **Out, for now:** the control plane's port 8841. Its clients (gxctl,
  scripts) pin or are given a CA (gxctl-auth.md §5); see §8 Q3.
- **Out:** reaching glidex from the public internet. A ts.net
  certificate is browser-trusted, but the name only resolves and routes
  inside the tailnet. Tailscale Funnel would publish it to the internet;
  glidex does not configure it (§7).

## 2. Decisions (proposed)

| # | Topic | Decision |
|---|---|---|
| T1 | Source | Get the certificate from the host's **`tailscaled`** through its local API now. **tailscale-rs** (Tailscale's Rust library for embedding a node, preview) does not issue certificates; it is a later phase (§6). |
| T2 | Who fetches | `glidex-ui` itself, over `/run/tailscale/tailscaled.sock`, as user `glidex-ui` granted by `tailscaled`'s `TS_PERMIT_CERT_UID`. The key stays in the UI's memory: no key file of its own, no copy shared with another process. |
| T3 | Which connections | Selected per handshake by SNI: a ts.net certificate domain gets the Tailscale certificate; every other name or address keeps today's certificate (self-signed or configured). LAN users and existing pins are unaffected. |
| T4 | Opt-in | Off by default (`GLIDEX_UI_TLS_TAILSCALE=off`). Turning it on publishes the node's ts.net name in Certificate Transparency logs (§7), so it is the operator's choice, offered by the installer. |
| T5 | Failure | Never a failed handshake because of Tailscale: without a usable Tailscale certificate, ts.net names get the default certificate and a warning is logged. A held certificate is kept until it expires. |

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

Both services move to the resolver. It also fixes renewing an
operator's own certificate, which currently needs a restart
(operator-guide `tls.md`, *Renewing*).

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

The certificate's fingerprint and expiry are logged when it changes.
The key is never written to disk by glidex (`tailscaled` keeps its own
copy under `/var/lib/tailscale/certs`).

### 4.3 Names: `Host` and `Origin`

- **UI `Host` check** (security.md §5.6): the current certificate
  domains are allowed hosts. They are read at request time from the
  Tailscale state above, so this works with the default list *and* an
  explicit `GLIDEX_UI_HOSTS`. That name is one the UI holds a
  certificate for, so it is not a rebinding risk.
- **Control plane `Origin` check:** its default `auth.allowed_origins`
  is computed from the host's local names and doesn't know the ts.net
  name. See §8 Q2; the recommendation is that the control plane accept,
  **from the `glidex-ui` peer on `ui.sock` only**, an `Origin` equal to
  `https://` + `X-Forwarded-Host`, since the UI has already checked that
  `Host` against its allowlist.

### 4.4 HSTS

`Strict-Transport-Security: max-age=31536000` is sent on connections
that were served the Tailscale certificate, as with any configured
certificate (security.md §5.1.1). HSTS applies to every port of the
name, so a browser that later opens `https://<node>.ts.net:8841` (where
the control plane serves its self-signed certificate) gets a
non-bypassable error. Browsers have no reason to go there (§1). See §8
Q5.

### 4.5 Permissions and sandbox

- `glidex-ui.service` already allows `AF_UNIX` and doesn't hide
  `/run/tailscale`. `tailscaled.sock` is `0666`, and `ProtectSystem=strict`
  doesn't prevent `connect()` on a read-only mount. **No unit change.**
- `tailscaled` must run with `TS_PERMIT_CERT_UID=glidex-ui`. This is
  `tailscaled`'s configuration, not glidex's: see §8 Q1 for whether the
  installer writes it.

### 4.6 Installer

When `tailscaled` is running and reports certificate domains:

- Offer to turn it on (`--tailscale-cert` / `--no-tailscale-cert`,
  saved in `install.conf`), explaining the Certificate Transparency
  exposure in one line (§7).
- If accepted: set `GLIDEX_UI_TLS_TAILSCALE=on` in a `glidex-ui`
  drop-in, and arrange `TS_PERMIT_CERT_UID` (§8 Q1).
- The usage blurb prints `https://<node>.<tailnet>.ts.net:5173` first,
  without a fingerprint; LAN URLs and fingerprints follow as today.

## 5. Configuration

| Variable (`glidex-ui`) | Default | Meaning |
|---|---|---|
| `GLIDEX_UI_TLS_TAILSCALE` | `off` | `on`: also serve the node's Tailscale certificate for its ts.net names (§4.2). Refused at startup with `GLIDEX_UI_TLS=off`. |
| `GLIDEX_UI_TAILSCALE_SOCKET` | `/run/tailscale/tailscaled.sock` | The `tailscaled` local API socket. |

Nothing changes in `control-plane.json` unless §8 Q3 is answered yes.

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

## 8. Open questions

1. **Granting `TS_PERMIT_CERT_UID`.** It holds one uid, and it belongs
   to `tailscaled`'s configuration. Options: (a) the installer writes a
   `tailscaled.service` drop-in setting it to `glidex-ui` and restarts
   `tailscaled` (a brief tailnet blip), refusing if it is already set to
   another user (e.g. Caddy's); (b) the installer only prints the
   instruction; (c) a root helper fetches for the UI instead (a new
   privileged component). *Recommendation: (a) with the refusal, behind
   the installer prompt.*
2. **Control plane `Origin` for the new name.** (a) Accept
   `Origin == https://<X-Forwarded-Host>` from the `glidex-ui` peer only
   (general, also fixes custom names behind the UI); (b) the control
   plane reads the certificate domains itself (read-only, no permission
   needed) and adds `https://<domain>:5173`; (c) the operator adds it to
   `auth.allowed_origins`. *Recommendation: (a). It changes the Origin
   rule in security.md §5.6, so it needs an explicit yes.*
3. **Port 8841 too?** Would gxctl users on the tailnet benefit enough
   to justify it? Because of the one-uid limit it means a second
   fetcher (root helper, or the control plane fetching and handing the
   pair to the UI over `ui.sock`). *Recommendation: no for now; pins
   already work.*
4. **Port 443.** Browsers would get `https://<node>.ts.net/` with no
   port. That needs `CAP_NET_BIND_SERVICE` (or
   `net.ipv4.ip_unprivileged_port_start`), or a listener only on the
   Tailscale address. *Recommendation: stay on 5173.*
5. **HSTS on the ts.net name** (§4.4): send it from the start, start
   with a short `max-age` (e.g. one day) and raise it later, or not at
   all? *Recommendation: one day in the first release.*
6. **Minimum `tailscaled` version.** Support only daemons with
   `cert-domains`, or fall back to `status`? *Recommendation: fall back;
   it costs one more request.*

## 9. Testing

- `glidex-tls`: resolver selects by SNI (match, case, no SNI, IP,
  unknown); a swap is seen by the next handshake and not by an
  established one; a file slot reloads on change and keeps the old pair
  on a bad file or mismatched key.
- `glidex-ui` against a fake `tailscaled` on a Unix socket (rcgen-made
  pairs): startup without the socket, `403`, empty domains, `429` with
  `Retry-After`, renewal below 14 days with `min_validity`, a domain
  rename, a chain whose SAN doesn't cover the domain (rejected). `Host`
  allowlist includes the domain while it is held.
- Host test (manual, on a tailnet with HTTPS enabled): a browser with
  no warning on `https://<node>.<tailnet>.ts.net:5173`, LAN URL still
  the self-signed certificate, login and console work, HSTS header only
  on the ts.net name.

## 10. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **TS0** reloadable certificates | `CertSlot` and SNI `Resolver` in `glidex-tls`; control plane and UI use them; configured certificate files reload on change | §9 `glidex-tls` tests; operator guide drops "restart to renew". |
| **TS1** Tailscale slot in the UI | `GLIDEX_UI_TLS_TAILSCALE`, local API client, polling and renewal, `Host` allowlist, HSTS, the §8 Q2 Origin rule | §9 `glidex-ui` tests; host test. |
| **TS2** installer | Detection, prompt and option, `TS_PERMIT_CERT_UID` per §8 Q1, usage blurb, operator guide section | Installer tests; host test from a clean install. |
| **TS3** *(conditional)* | Port 8841 (§8 Q3) | — |
| **TS4** *(gated upstream)* | Embedded tailscale-rs node (§6) | — |

## 11. Changes to other specs (applied — change record)

- **security.md:** decision 19; §5.1 certificate table, row
  "Tailscale"; §5.1.1 HSTS bullet; §5.6 `Host` check; §17 status row.
  All tagged *(draft)*.
- **web-ui.md:** *Server* environment table rows for
  `GLIDEX_UI_TLS_TAILSCALE` and `GLIDEX_UI_TAILSCALE_SOCKET`; the HSTS
  sentence. Tagged *(draft)*.
- **installer.md:** step 13 usage blurb and the Tailscale prompt.
  Tagged *(draft)*.
- **README.md, SUMMARY.md:** this document's entry.

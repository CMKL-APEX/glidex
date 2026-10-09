# `gxctl` login profiles and client trust

Status: **design accepted; specs merged, code pending** (milestones §12).
Complements [cli.md](cli.md) "Transport and authentication" and
[security.md](security.md) §5; it does not change the server's
authorization model. The spec edits it required are already applied to
those documents — §9 and §10 are their change record — and every
not-yet-built row there carries a *(draft)* tag, so the implemented
behaviour stays describable without this file.

`gxctl` today keeps one credential in one file
(`~/.config/glidex/token`, `crates/glidex-control-plane/src/bin/gxctl/client.rs:59-62`)
and takes its target and overrides from `--url`, `GLIDEX_TOKEN` and
`GLIDEX_CA_CERT` per invocation. That works for one server you also
physically own, and fails for the two cases this document covers:

1. **Several targets.** An operator drives a lab host, a prod cluster
   and an edge node from one laptop: today that means env-var juggling
   and re-running `login` to swap the single token file, with no record
   of which token belongs to which server.
2. **Self-signed certificates.** Remote control planes present the
   `rcgen` self-signed certificate of [security.md §5.1.1](security.md).
   gxctl verifies against the system store plus `GLIDEX_CA_CERT` and
   offers *no* way to accept anything else (cli.md:43-45,
   `glidex-tls/src/lib.rs:453` "never accepts anything the WebPKI
   verifier refuses"), so every first connection to a new host is a
   dead end unless someone copies the PEM file out of band.

`gxctl auth login` is the command that creates the configuration file,
from an interactive or scripted login against a chosen target.

## 1. Decisions

| # | Topic | Decision |
|---|---|---|
| A1 | File format | One JSON file, `config.json` in the gxctl config dir, matching the house style of `netd.json` / `control-plane.json`. No YAML, no kubeconfig compatibility — gxctl config and kubeconfig on the same laptop are separate universes. |
| A2 | Profiles | Named profiles keyed by short name (`prod`, `lab-edge`); each bundles **target (server URLs, cluster identity) + trust + credential + session defaults**. One `current` profile; `--profile` picks another for one run. |
| A3 | Secrets in the file | The bearer token is stored in the profile. The file is `0600` in a `0700` directory and, like the token file, is refused when group/others can read it or another uid owns it (`client.rs:64-90`). `token_command` lets a site keep the secret outside the file entirely. |
| A4 | Self-signed trust | The supported "bypass" is **pinning the leaf-certificate SHA-256** (TOFU at login), not skipping verification. Verification of signature, validity dates and (by default) the name still run against the pinned certificate. |
| A5 | Insecure bypass | A true "verify nothing" mode exists (`insecure`) but is a declared escape hatch: it needs an interactive confirmation echoed into the file, prints a warning on every command, and is never reachable by default, by expiry, or by an untrusted-certificate error. |
| A6 | Cluster binding | Each profile records the `cluster_id` the server reports (`GET /auth/server-info`, new, §7.1); connecting to a server that reports a different id is a hard error, not a prompt. |
| A7 | HA targets | A profile may list several server URLs of the **same** cluster (any node with the server role serves the API, [clustering.md §8](clustering.md)); gxctl tries them in order and reorders by recent success. |
| A8 | Command surface | New `auth` command group (`auth login`, `auth logout`, `auth status`, `auth use`, `auth token …`, `auth trust …`). The existing `login` / `logout` / `whoami` / `token …` names stay as aliases; `auth` is free today (main.rs:1441). |
| A9 | Back compatibility | With no config file, behaviour is byte-for-byte today's. The legacy token file keeps working as the credential of an implicit `localhost` profile and is folded in on first `auth login`. |

## 2. The configuration file

### 2.1 Location and hygiene

| Item | Rule |
|---|---|
| Path | `$XDG_CONFIG_HOME/glidex/config.json`, default `~/.config/glidex/config.json` — the directory of `token_path()` (`client.rs:59-62`). |
| Override | `GLIDEX_CONFIG=<path>` names a different file (CI, multiple laptops' users); `GLIDEX_CONFIG=` (empty) disables reading any file, which CI should set so a stray profile dir cannot change what a pipeline talks to. |
| Modes | Directory `0700`, file `0600`; written to a temp file and renamed, exactly as `save_token_file` does (`client.rs:92-116`). |
| Read guards | Refused, with the same wording as the token file, when any of `0o077` is set or the owner is not the euid. **Invariant:** the file holds a bearer token, which is as good as a password (security.md §5.5). |
| Debug | The parsed config type implements `Debug` printing only profile names, URLs and trust modes — never a token or `token_command` output. |
| Certificates | PEM files a profile references live under `~/.config/glidex/certs/` (dir `0700`, files `0600`…`0644`), or are embedded as `ca_pem`. A `ca_file` outside the glidex dir must pass the `trustworthy()` owner/mode check (`glidex-tls/src/lib.rs:387-397`) before being added to the trust store, so a plantable file in a world-writable directory never becomes a CA. |

### 2.2 Schema

Unknown keys and unknown `version` major are errors that name the file
and line; gxctl refuses to edit a file it cannot fully parse, so
hand-editing typos fail early instead of silently dropping a pin.

```json
{
  "version": 1,
  "current": "lab",
  "profiles": {
    "lab": {
      "url": ["https://lab-cp.example.org:8841", "https://10.0.0.21:8841"],
      "cluster_id": "6f0c2c9a-1d4b-4e07-9a51-2f3b8d0c7e11",
      "cluster_name": "lab",
      "project": "web",

      "tls": {
        "ca_file": "~/.config/glidex/certs/lab-ca.pem",
        "pins": ["sha256/2bf8c6c4f3d2…"],
        "verify_host": true,
        "insecure": false
      },

      "token": "gxt_…",
      "token_command": null,

      "identity": {
        "method": "oidc",
        "user": "alice@example.org",
        "token_id": "3f2a…",
        "token_name": "gxctl@lab",
        "logged_in_at": 1760000000
      },

      "added_at": 1760000000,
      "last_used": { "at": 1760086400, "url": "https://10.0.0.21:8841" }
    },

    "edge-ci": {
      "url": ["https://192.168.1.5:8841"],
      "cluster_id": "b11a…",
      "tls": {
        "insecure": true,
        "i_understand": "192.168.1.5"
      },
      "token_command": "vault kv get -field=token -raw secret/glidex/edge-ci"
    }
  }
}
```

Field rules:

| Field | Meaning |
|---|---|
| `url` | One or more API base URLs of **one** cluster (all must report the same `cluster_id`; §4.3). Stored as given, `https://` always (plain `http://` only for loopback, cli.md:20). |
| `cluster_id` | Written by `auth login` from `GET /auth/server-info`; the profile is bound to it (§4.3). `null` = never verified (hand-edited or pre-login skeleton); any request before the check completes is refused. |
| `cluster_name` | Display only. |
| `project` | Session default, equivalent to `--project` (cli.md:22). The server-side default from `project use` still wins when unset. |
| `tls.ca_file` / `ca_pem` | Extra roots merged with the system store. |
| `tls.pins` | Lowercase `sha256/<64 hex>` leaf-certificate fingerprints, §5.3. |
| `tls.verify_host` | Default `true`. `false` is accepted **only** together with a matching `pins` entry, for dial-by-IP edge nodes whose certificate lacks the SAN (security.md §5.1.1: regenerate the cert is the better fix). |
| `tls.insecure` + `i_understand` | §5.4. `i_understand` must equal the profile's host, typed by the user at `auth login --insecure`. |
| `token` | The `gxt_…` secret (security.md §5.5). Mutually exclusive with `token_command`; `token_command` wins when both exist (with a warning), so a site-wide helper beats a stale copy. |
| `identity.*` | Written from the `GET /auth/whoami` answer after the token is checked, and shown by `auth status`; never used for authorization, purely to show what the credential is. |
| `last_used.url` | Failover reordering hint (§4.2); losing it costs nothing. |

## 3. How a request decides where it goes

Precedence, first match wins (mirrors kubeconfig habits, replaces the
one-off `--url` dance):

```
--profile NAME          GLIDEX_PROFILE          config: current
      │                       │                        │
      └───── ad-hoc: --url overrides the chosen profile's url list for
              this run only; no file is written unless --save
```

1. `--profile` / `GLIDEX_PROFILE` names the profile; `--url` overrides
   the endpoint list for that run (token and trust still come from the
   profile unless overridden by env).
2. No flag: `GLIDEX_PROFILE`, else `current`, else — when the file has
   profiles but no `current` — an error listing them.
3. **No config file:** today's behaviour exactly — `--url` or
   `GLIDEX_TOKEN` → TCP, else the Unix socket ladder, else
   `https://localhost:8841` (cli.md:16-21). A hand-edited skeleton file
   with only `url` fields behaves the same until a login fills it in.
4. **Credential:** `GLIDEX_TOKEN` wins over the profile token (cli.md:29
   kept); `token_command`'s stdout (trimmed, one line, ≤ 4 KiB) wins
   over the stored field but not over `GLIDEX_TOKEN`, is never echoed or
   logged, and its failure aborts the request with the helper's exit
   status. On the Unix socket no credential applies, as today.
5. The legacy `~/.config/glidex/token`: used as the credential of an
   implicit `localhost` profile; `auth login` into any profile that
   ends up talking to that same API base copies it over (after the
   whoami check) and leaves a one-line note in the REPL banner. It is
   never deleted without `--revoke`.

**Why precedence this way:** scripts pinning `GLIDEX_TOKEN` must keep
working unchanged (cron jobs written against cli.md's current table),
and an operator with `--profile prod` selected must be able to debug
against `--url` without any chance of the prod token leaking into the
debug target — the ad-hoc URL starts from the profile's trust settings
but the credential is only attached when the host matches one of the
profile's own `url` entries.

## 4. Connecting

### 4.1 Server info handshake

Every new connection to a TCP endpoint runs, before any other call:

`GET /auth/server-info` (§7.1, unauthenticated) →
`{cluster_id, cluster_name, node_id, version, fingerprint, methods:{pam, oidc, disabled}}`
— the `methods` map is what `GET /auth/methods` returns today
(access.rs:33-39), extended with the binding fields.

- `cluster_id` must equal the profile's (§4.3).
- `version` mismatch with the gxctl build is a warning, not an error
  (old server, new client must keep working across a rolling upgrade).

### 4.2 Endpoint failover

Endpoints are tried in order: the `last_used.url` first, then the
profile list order, 5 s connect timeout each; the first success rewrites
`last_used` (a metadata-only write of the whole file, `0600`, atomic).
A `401`/`403` answer means the server is up — no failover across
authorization errors, so a firewalled node never turns "token revoked"
into three confusing errors against two other servers. All endpoints
down: one error listing each URL and why it failed.

### 4.3 Cluster binding and re-pointing

`cluster_id` mismatch, or an endpoint answering with a different id
than the profile: **hard error** —

> `refusing to continue: profile 'lab' is bound to cluster 6f0c… but https://10.0.0.21:8841 answers for b11a…. A machine was repurposed, or the profile is stale. Re-point with: gxctl auth login --profile lab --rebind`

**Why hard, not a prompt:** a prompt here trains users through the one
signal that catches a hostile or leftover host serving a fake UI (think
a decommissioned node reinstalled by someone else, or a DHCP reuse).
`--rebind` re-runs the login and *forces* rewriting `cluster_id` and
the pins; it is the only way, and it prints the old and new ids and
fingerprints side by side first.

## 5. TLS: trust ladder, self-signed by pinning, and the bypass

### 5.1 The ladder

A connection is accepted when **any** of these verifies it; all still
require a well-formed certificate and successful key agreement:

1. The system store (rustls-native-certs) — real CA certificates.
2. A profile `ca_file` / `ca_pem` — a corporate CA or a copied
   self-signed PEM, owner-checked as §2.1.
3. A profile `pins` entry matching the leaf certificate's SHA-256
   (`fingerprint_der`, `glidex-tls/src/lib.rs:224` — same computation as
   the server logs at start, security.md §5.1.1, so the numbers users
   compare are the same numbers).
4. Loopback only: the published `/run/glidex-cp/tls.crt` ladder
   (`published_certs()`, cli.md:40-42), unchanged.

Name verification (SAN match) runs in all four, and in 3 unless
`verify_host: false` **with** pins present. The `Recording` verifier
(`glidex-tls/src/lib.rs:456-473`) already captures the rejected
fingerprint; the ladder adds one more verifier implementation beside it,
so the error message path is shared.

### 5.2 TOFU at login (the supported "self-signed bypass")

`auth login` (and any connect from a profile whose pins are empty) that
fails verification on an *unpinned* target prints, instead of today's
`tls_error` (client.rs:335-344):

```
the certificate of lab-cp.example.org:8841 is not trusted by this system:
  SHA-256 2bf8c6c4f3d2…   (the control plane prints this line at startup)
  reported by the server as its own: matches / differs
Trust this exact certificate for profile 'lab'? [y/N]
```

- `y` stores the fingerprint in `tls.pins`; every later connection
  verifies against it. A change later is a **hard error** (§5.3), never
  a re-prompt.
- Non-interactive (`--pin sha256/…`, or stdin not a TTY): the flag
  value is compared and stored without prompting; a TTY-less login
  **without** `--pin` fails with the fingerprint and the exact retry
  command, never silently pins.
- `reported by the server as its own` is the `/auth/server-info`
  `fingerprint` field cross-checked against what we observed — it
  catches a proxy that terminated TLS and is presenting something else,
  which the "compare with the startup log" step alone would not.

**Invariant:** the token is never sent on a connection that passed none
of the four ladder rungs. TOFU does not skip verification; it records a
trust anchor on first use and enforces it from then on.

### 5.3 Pin maintenance

- `auth trust list --profile P` / `auth trust remove --profile P`
  (and `--all` for the pin list) manage §5 pins; `auth trust fetch`
  re-reads the server's published fingerprint and reports match/no-match
  without connecting anything else.
- The server regenerates its certificate only in the narrow cases of
  security.md §5.1.1 (missing, unreadable, < 30 days to expiry), and
  the fingerprint-stable invariant keeps pins valid across restarts.
  When it *does* change, the pin-mismatch error prints the old pin, the
  new fingerprint, the `auth trust fetch` command, and warns that a
  re-pin should be confirmed against a second channel (the startup log
  line, another operator, the UI's warning page).

### 5.4 `insecure` — declared, contained, boring to watch

True bypass, for hosts where even TOFU is impractical (dial-by-IP with
no SAN, test rigs). Semantics: *nothing* about the peer's certificate
is checked — signature chain, validity dates and name all skipped; the
transport is still encrypted against a passive listener, not an active
one. This is the **user's explicit risk**, and the design's job is to
make it impossible to reach by accident:

| Guard | Behaviour |
|---|---|
| Setup | Only `auth login --insecure`, interactively typing the host as the confirmation phrase, writes `insecure: true` + `i_understand: "<host>"`. Hand-editing `insecure` without `i_understand` (or with the wrong phrase) is a config error. |
| No drift | `GLIDEX_TLS_INSECURE=1` grants it for one run (CI on throwaway rigs) but never for an interactive login, and never when the profile has pins or a CA — those must be removed with `auth trust remove` first, so an env var in a shared script cannot quietly override a laptop's pins. |
| Loud | Before the first request of every invocation, one line to stderr: `UNVERIFIED TLS: the certificate of 192.168.1.5 was not checked — a network attacker can read your token`. `auth status` and `whoami` repeat it. `watch`/`connect` re-print it. |
| Last resort | A verification failure never suggests `--insecure`; it suggests `--pin` (§5.2). |

**Why keep it at all:** refusing every bypass pushes users to `curl -k`
scripts that skip gxctl's other guards entirely. A bypass that is
recorded in the config file, attributed to the host, and visible on
every run beats a hidden one; it also gives `auth status --all` the job
of showing which profiles are unsafe, which is auditable.

## 6. The `auth` command family

`auth` joins `COMMANDS` (main.rs:1441) with subcommands in
`SUBCOMMANDS` (main.rs:1453); old names stay as aliases so scripts and
muscle memory survive.

| Command | Effect |
|---|---|
| `auth login [--profile P] (--url U… \| --server U…) [--oidc \| --token \| --pam-user U] [--pin sha256/…] [--insecure] [--rebind] [--use] [--save] [--name N] [--days N] [--non-interactive]` | The **generator of the config file** (§6.1). `--use` makes the profile `current`; `--name N` overrides the token name `gxctl@<profile>`; `--days N` is passed to the mint (`--save`, with an ad-hoc `--url`, records the target as a new profile instead of using it once). |
| `auth logout [--profile P] [--revoke] [--keep-token]` | Removes the credential from the profile (`--revoke`: `DELETE /tokens/{id}` of *this profile's* token first — via `identity.token_id`; then the file edit). Profile, pins and CA stay: `login` again later needs only the credential step. |
| `auth status [--profile P]` / `auth` with no argument | REPL banner and this: target URLs, cluster id/name, trust mode (`system+ca+pinned(<n>)` / `pinned+no-name-check` / **`UNVERIFIED`**), profile token name/expiry, server methods, and the `current` marker. |
| `auth use P` / `auth use -` | Set / clear `current`. |
| `auth profiles [--all]` | List profiles; `--all` adds each one's trust mode and token name. |
| `auth token list\|create\|revoke …` | The old `token` commands (cli.md:138-140): `create` keeps `--days N --service-account [--project P] --role ROLE[@PROJECT]…` forwarded as today. |
| `auth trust list\|fetch\|remove …` | §5.3. |

### 6.1 `auth login` — what it does, in order

1. **Resolve the target.** `--url`/`--server` (repeatable — several
   endpoints of one cluster, §4.2), or an existing profile's urls.
   Without either and without a usable `current`: refuse, "which
   server?" (there is no defaulting to a guess).
2. **Connect** through the ladder (§5). Untrusted cert → TOFU prompt /
   `--pin` / refuse non-interactively (§5.2).
3. **`GET /auth/server-info`** → bind `cluster_id`: existing profile →
   must match or `--rebind`; new profile → store. Also notes which
   methods the server has (`--oidc` refused with the server's answer
   when OIDC is off, instead of today's device-flow 404).
4. **Prove the credential.**
   - `--oidc`: the existing device flow (admin.rs:141-190): print
     verification URI + user code, poll; `token_name: "gxctl@<profile>"`
     (§7.2), plus `{device: hostname, client: "gxctl/<version>"}`.
   - `--token`: read the secret with echo off from a TTY, or raw from
     stdin when piped (as `login --token` does, cli.md:134).
   - `--pam-user U`: `POST /auth/token {username, password,
     token_name}` (§7.3) — hidden password prompt → returns a fresh
     personal token. Rate limits and lockout are the server's (§5.3 of
     security.md), so gxctl needs none of its own.
   - Ad-hoc `--url` with an existing profile whose host matches: reuse
     the profile credential without re-prompting (a no-op convenience,
     like `docker login`'s silent token reuse).
5. **Check it**: `GET /auth/whoami` must answer; the reply's
   `{user, method, token.id, token.name}` becomes `identity.*`. Wrong
   credentials are reported and nothing is written.
6. **Write the file.** Merge into the existing config (never clobber
   other profiles; `0700`/`0600`, temp+rename), set `current` when it
   is the first profile or `--use`. Print the path and one redaction-
   awareness line: `token saved to ~/.config/glidex/config.json
   (mode 0600) — profile 'lab', anyone who can read that file can act
   as alice`.
7. `--non-interactive` / stdin-not-a-TTY forbids every prompt: TOFU and
   `--insecure` confirmations become errors naming the flag to pass;
   `--token` still consumes stdin (one read, no write).

### 6.2 REPL banner

With a profile in effect (not the implicit local socket), the REPL
opens with `gxctl → https://lab-cp.example.org:8841 · cluster lab ·
pinned · token gxctl@lab (expires in 82 d)` — plus the §5.4 warning
line when unverified. `auth status` prints the same on demand. The
banner is *why* the profile model pays for itself: at a glance, an
operator knows which cluster `delete vm` points at.

## 7. Server-side additions

Small, all consistent with security.md; §9 records the needed edits.

### 7.1 `GET /auth/server-info` (public)

Same public allowlist as `/auth/methods` (`api/mod.rs:246`,
`gate.rs:29`). Returns
`{cluster_id, cluster_name, node_id, version, fingerprint, methods:{pam, oidc, disabled}}`.
`fingerprint` is `glidex_tls::fingerprint()` over the serving
certificate — public information (the server logs it at start; a
certificate's fingerprint is not a secret). `cluster_id`/`node_id` come
from `<state dir>/cluster/identity.json` (clustering.md:1565); a
standalone host reports its single-node cluster id, and `null` before
clustering state exists. Caches nothing per client; no rate needed
(it is static after boot). `GET /health` can carry `version` too so
monitoring sees it without a new route.

### 7.2 Token naming and metadata

- Device and PAM token flows stamp `api_tokens` with
  `{name: "gxctl@<profile>", device: "<hostname>", client: "gxctl/<version>"}`
  (columns next to `last_used_from`, security.md §5.5). The name is
  what lets an owner `token revoke` the right laptop's credential from
  `token list`; `device` is display and audit only, never checked.
- `token_name` on the device poll is already a parameter
  (admin.rs:159) — clients today hard-code `"gxctl"`; the server caps
  length (≤ 64) and rejects control characters.
- The server's `auth.tokens.max_days` clamp (security.md §13) applies
  to login-created tokens exactly as to `token create`.

### 7.3 `POST /auth/token` — credential → personal token

`{username, password, token_name?, device?, client?, days?}` →
`{token, token_id, expires_at}` (or `401`, rate-limited). It runs the
same authd path as `POST /auth/login` (security.md §5.3: peer check,
`pam_acct_mgmt`, `allowed_groups`, the 5/15 min + 30/min limits, the
fixed delay) but mints a *bearer token* like the device flow does,
instead of a browser session cookie — the CLI and CI need bearer
tokens; sessions are the browser's (security.md §5.5). TLS-only,
password zeroized after use, never logged (security.md §10). JIT
provisioning and `pam.group_teams` sync happen here as they do on any
PAM login. Accepts the request only when `pam.enabled`; otherwise
`401` pointing at the server config, so a site cannot not notice the
method is off.

### 7.4 Identities: generation and administration

The complete answer to "where do identities come from" for a login this
config file caches:

| Identity | Created | First credential | Administered |
|---|---|---|---|
| `unix:<name>` (local) | Just-in-time on first `api.sock` connect, `glidex-users` only (security.md §5.2) | none (peer uid) | groups; `glidex-admin` = break-glass. Never reachable from a profile: TCP never carries unix identity. |
| `pam:<name>` | JIT on first successful `glidex-authd` login when in `pam.allowed_groups` (security.md §5.3) | local Unix password → `auth login --pam-user` (§6.1.4) → personal `gxt_` token | `pam.group_teams` sync at each login; disable by removing from the Unix group **and** `PATCH /users/{id} {"disabled": true}`. |
| `oidc:<issuer>+<sub>` | JIT on first device/browser login passing `allowed_domains`/`required_groups` (security.md §5.4) | browser at the IdP (device flow) | `oidc.group_teams` re-synced each login; deprovision at the IdP, then disable here. |
| personal token | `token create` or `auth login` (§6.1) | the `gxt_` secret in the profile | owner may list/revoke own tokens; ∩ with owner on every request (§7.5 of security.md). |
| service-account token | `token create --service-account --project P` | handed out once, pasted into CI, or `token_command` | `project.members` in P or `system.identity`; linked roles live on the token itself. |
| `system-admin` etc. | role links by an existing `system-admin` (`system-binding add`) / `project.members` (`binding add`) | — | break-glass on the socket bootstraps the first one. |

**Remote bootstrap (the chicken-and-egg).** No OIDC yet, no remote
login possible: from a shell on the host, root runs
`gxctl auth token create bootstrap-1 --days 1 --role role.system-admin`
(a host role, so the link lands on the `Cluster`, security.md §7.3; the
secret prints once, §5.5), hands it over any channel,
the operator runs `gxctl auth login --url https://… --token`, and the
token expires or is revoked after the real identities exist. This is
the documented path, not a back door: the grant still ends at the
`role.system-admin` link, and `unix:glidex-admin` break-glass remains
impossible remotely — a token is a `Token` principal and the
`base.break-glass` team only gains members from `SO_PEERCRED`
(security.md §7.3, "They can't be created, joined or synced from
OIDC").

Lifecycle (admin side, unchanged surface — `user list` shows
identities, `token list` shows `last_used_from` + the new `device`):

- **Rotation:** `token create` a sibling, re-`auth login` on the box
  with the new secret, `token revoke` the old one; no overlap promise,
  matching §5.5's revocation-is-immediate semantics.
- **Expiry** is mandatory (90 d default / 365 max, §5.5); `auth
  status` and the banner count down so laptops re-login before they
  are surprised mid-demo.
- **Disabling a person** covers their tokens on the next request
  (`base.disabled-user`), so a lost laptop needs only `user disable`
  (admin `PATCH /users/{id}`), not hunting tokens.

## 8. Errors

| Condition | Shown |
|---|---|
| pin / cluster mismatch | §4.3 / §5.3 hard errors, with the commands to fix, never auto-repaired. |
| config unreadable (mode/owner) | token-file wording reused, naming the file and the mode. |
| bad JSON / version | file path + parser message; "fix it or set GLIDEX_CONFIG= to ignore this file". |
| `token_command` failed | `secret helper exited N: <its stderr, first line>` — its stdout is never shown. |
| server has the method off | `the server does not accept <oidc/pam> logins` (+ its config key), not a raw 404. |
| all endpoints down | each URL and reason (§4.2). |

## 9. Changes to spec/security.md (applied — change record)

Applied alongside this design so the two documents agree; none of them
changes an authorization decision. Reviewers can check security.md
against this list.

1. **§2 Decisions — add row 18.** *Client trust*: `Pinning the leaf
   certificate's SHA-256 (TOFU) is the supported way to accept
   self-signed control planes. Verification of the chain, dates and
   name keeps running against the pinned certificate. A no-verify mode
   exists only as an explicitly confirmed, per-profile, loudly marked
   escape hatch ([gxctl-auth.md §5](gxctl-auth.md)).`
2. **§5.1.1 bullet "remote gxctl uses `GLIDEX_CA_CERT`"** → extend:
   `…or the profile's ca_file/ca_pem/pins (gxctl-auth.md §5);
   GLIDEX_CA_CERT remains as the per-invocation override.` The
   fingerprint-stable invariant in that bullet is the *reason* pinning
   is sound, and should say so.
3. **§5.1 public-endpoint list** (`GET /health`, `GET /auth/methods`,
   …) → add `GET /auth/server-info`.
4. **§5.5 Access tokens** → note the login-minted personal tokens
   (`gxctl@<profile>` names, `device`/`client` metadata, same
   expiry clamp), and point `token list`'s output at the new columns.
5. **§5.3 (PAM) or §13** → add `POST /auth/token` (§7.3): body, the
   authd path it reuses, the rate limits it inherits, TLS-only,
   "returns a bearer token, not a session".
6. **§10 Audit** → add events `auth.login` (method, profile-supplied
   device string, client IP — never the secret), and note the CLI's
   `auth login` is that event's usual source.
7. **§13 endpoints table** → rows for `GET /auth/server-info`
   (public) and `POST /auth/token` (none); config example gains
   `"server_name"` (display name used by `server-info`, defaulting to
   the host name).
8. **§14 Testing** → the cases of §11 below (pin mismatch, cluster
   swap, insecure confirmation, precedence).
9. **§17 status table** → row when implemented: profiles, the ladder
   verifier in `glidex-tls` next to `Recording`, the two endpoints.

## 10. Changes to spec/cli.md (applied — change record)

Applied alongside this design; the new rows carry a *(draft)* tag until
their milestone ships. Check cli.md against this list:

1. **Transport table** — `--profile` / `GLIDEX_PROFILE` and
   `GLIDEX_CONFIG` rows added after `--url`.
2. **TCP bullet** — the token-file sentence extended: the profile file
   is the credential once it exists, `~/.config/glidex/token` is the
   implicit `localhost` profile's legacy fallback.
3. **TLS bullet** — closing sentence pointing the trust ladder, TOFU
   pinning and the `insecure` last resort at §5, noting they are not
   yet built.
4. **Access-control table** — `auth login` (full flag set), `auth
   logout`, `auth status` / `use` / `profiles`, `auth trust` rows, all
   *(draft)*; the old `login` / `logout` / `whoami` / `token …` rows
   stay as the implemented behaviour.
5. **Errors table** — the cluster-mismatch, pin-mismatch and
   bad-profile-file rows plus a pointer to §8.

## 11. Testing

- Config: mode/owner refusals; unknown-version refusal; concurrent
  edit loses nothing (two `auth login`s merge); `GLIDEX_CONFIG=` →
  today's behaviour under the full legacy test.
- Precedence matrix: `GLIDEX_TOKEN` vs profile vs `token_command`;
  `--url` + profile host-match token reuse; ad-hoc URL off-profile
  gets no token.
- TLS: self-signed accepted only after pin (prompt / `--pin` /
  refusal non-interactive); pin change = hard error; `verify_host
  false` requires a pin; `insecure` without `i_understand` = config
  error; `GLIDEX_TLS_INSECURE=1` cannot beat pins; the warning line
  lands on stderr exactly once per invocation; planted CA files fail
  `trustworthy()`.
- Bindings: server-info `cluster_id` swap → refuse; `--rebind` works;
  failover reorders; no failover on 401.
- Logins: device flow stamps `gxctl@lab` + device; `--token` from pipe
  never echoes; PAM exchange rate-limited and zeroized; server-method-
  off returns the friendly error; logout --revoke removes only the
  profile's token id.
- Mock server: an rcgen-cert server on a loopback name missing from
  the SANs drives the whole §5.2→§5.4 ladder, and one presenting a
  *foreign* chain (proxy-in-the-middle impersonation) is caught by the
  server-info fingerprint cross-check.

## 12. Implementation plan

Two tracks: **client** (`gxctl`, `glidex-tls`) and **server**
(`glidex-control-plane`). The spec edits (§9, §10) are merged already;
each milestone lands code + its §11 tests and flips the matching
*(draft)* tags. Order: G1 → G2 → G3 → G4 → G5; within G2 the
`server-info` server half must land before the client's TOFU
cross-check, and G4's server half can run parallel to G3's client
half. DoD every milestone: `cargo test -p glidex-control-plane -p
glidex-tls` green — including the route-coverage pin
(`api/mod.rs:768-772`) — and the un-drafted rows still match the docs.

| # | Scope | Depends on |
|---|---|---|
| G1 | config file + profile model + precedence (client only) | — |
| G2 | trust ladder, TOFU, pins, `auth trust`, `GET /auth/server-info` | G1 (profile fields to read) |
| G3 | `auth` command group, login flows, banner, token metadata | G1+G2 |
| G4 | `POST /auth/token`, `--pam-user` | G3 (token mint + naming exist) |
| G5 | `insecure` hatch, CI story, hardening pass | G3 |

**Status: G1–G5 landed.** Client side: `bin/gxctl/config.rs` (profile
file, validation, merge-write), `glidex_tls::Trust` +
`ClientTls::with_trust` (the ladder), `bin/gxctl/client.rs` (multi-
endpoint failover with `last_used`, binding preflight, ad-hoc `--url`
host-match), `bin/gxctl/admin.rs` (`auth` group); server side:
`api/access.rs` (`server_info`, `pam_token`) and the device/client
stamp in `auth::create_token` (`stamp`, sanitised at the door).

### G1 — config file, profiles, precedence (§2–§4)

Client-only; ships usable at once for the `GLIDEX_CA_CERT`-carrying
sites (profiles for *where*, today's TLS story unchanged).

- **`gxctl/client.rs`** — `Config`/`Profile` types (serde, unknown-key
  reject, `Debug` redacting `token`/`token_command` output per §2.1);
  `load_config()` with the mode/owner guards cloned from
  `read_token_file` (`client.rs:64-90`); merge-writer reusing the
  temp+rename pattern of `save_token_file` (`client.rs:92-116`);
  `token_command` runner (stdout trim, ≤ 4 KiB, never echoed).
- **`gxctl/main.rs`** — `--profile`/`--save` on `Cli`
  (`main.rs:26-52`); `build_client` (`main.rs:57+`) profile resolution
  and the legacy-`localhost` fallback; `GLIDEX_PROFILE`,
  `GLIDEX_CONFIG`.
- Tests (inline `#[cfg(test)]`, the house pattern): guard refusals,
  unknown-version, concurrent merge, the precedence matrix,
  `GLIDEX_CONFIG=` → legacy behaviour.

Acceptance: §11 config + precedence rows; every pre-existing gxctl test
still passes with no config file (byte-identical behaviour).

### G2 — trust ladder, TOFU, pins, server-info

- **Server track first** — `src/config.rs`/`src/serve.rs`:
  `server_name` beside the tls config; `api/access.rs`: `server_info`
  handler (fingerprint via `glidex_tls::fingerprint()`, `lib.rs:224`;
  `cluster_id`/`node_id` from `<state dir>/cluster/identity.json`,
  `null` pre-init — clustering.md:1565; `methods` as in
  `access.rs:33-39`); route as `PUBLIC` in the `api/mod.rs` table
  beside `/auth/methods` (`mod.rs:246`), `gate.rs:29`, and the
  public-list pin `mod.rs:768-772`; `packaging/control-plane.json.example`
  gains `server_name`.
- **Client track** — `crates/glidex-tls`: `ClientTls::with_trust
  (system_store, extra_roots, pins, verify_host)`; the pin check as a
  second verifier next to `Recording` (`lib.rs:456-473`), reusing its
  rejected-fingerprint capture; tests beside the existing client tests
  (`lib.rs:598-638`). `gxctl/client.rs`: `tls_config()`
  (`client.rs:323`) fed from the profile; `tls_error()`
  (`client.rs:335-344`) becomes the §5.2 TOFU transcript for
  unpinned targets; `--pin`; `auth trust list|fetch|remove`.
- Tests: mock control plane serving an rcgen certificate (the
  `lib.rs:598` pattern, promoted to a shared test helper); pin
  mismatch hard error; cluster-swap refusal; failover reorder, none on
  401; planted-CA `trustworthy()` refusals; proxy-terminates-TLS caught
  by the server-info fingerprint cross-check.

Acceptance: §11 TLS + binding rows.

### G3 — `auth` group, login flows, banner, token metadata

- **Server track** — `src/auth/store.rs`: `device`/`client` columns on
  `api_tokens` (`store.rs:24`, beside `last_used_*` at `:183-185`),
  exposed by `GET /tokens` and the `whoami` token object; device poll
  accepts `token_name` cap ≤ 64, control chars refused; `auth.login`
  audit entries (§10 of security.md).
- **Client track** — `COMMANDS`/`SUBCOMMANDS` (`main.rs:1441-1469`)
  gain `auth`; `handle_words` (`main.rs:2472+`) dispatches; new
  `admin.rs` section implementing §6.1 exactly (reuses `device_login`,
  `admin.rs:141-190`, adding the `gxctl@<profile>` name + device
  fields); `identity.*` filled from whoami; REPL banner at the
  `main.rs:2611-2613` auth summary; `auth status/use/profiles`;
  logout via `identity.token_id`.
- Old aliases must keep passing their current tests untouched.

Acceptance: §11 login rows.

### G4 — `POST /auth/token` + `--pam-user`

- **Server track** — `api/access.rs`: handler sharing the authd
  client path of `POST /auth/login` (`src/auth/mod.rs`) and the token
  mint of `POST /tokens`; register `PUBLIC` (like `/auth/login`),
  TLS-only refuse on the TCP listener, shared per-user/global rate
  counters and the fixed delay; `401` + config key when
  `pam.enabled` is off.
- **Client track** — `--pam-user U`: hidden prompt ×1 (server owns
  lockout), exchange, then the §6.1 steps 5–6 unchanged.
- Tests: rate limit mirrors authd's; zeroized/never-logged secret
  (audit redaction test pattern); JIT user + `group_teams` sync
  reachable via the new endpoint; method-off error text.

Acceptance: §11 PAM rows.

### G5 — `insecure` hatch and polish

- Client: `--insecure` interactive host-typed confirmation writing
  `i_understand`; `GLIDEX_TLS_INSECURE=1` one-run mode with the
  pins-present refusal; the per-invocation stderr warning (once);
  errors for verification failures stop suggesting anything but
  `--pin`; `auth status --all` unsafe-profile roll-up.
- Tests: §11 TLS refusal rows; docs: flip every *(draft)* tag, update
  the security.md §17 row from **draft** to the landed modules.

### Sequencing note

G1 + the G2 server half are independent enough to start together; the
G2 client half then has server-info to cross-check against on its
first test run. Nothing after G1 blocks on server review — the
client-only milestones land value to multi-server users immediately.

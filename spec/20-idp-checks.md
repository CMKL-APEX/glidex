# Real-IdP checks (spec/19-tenancy.md §17, T2–T6)

The connector presets are tested in CI against a mock IdP
(`tests/connector_tests.rs`). What a mock can't show — how the real
providers fill in their tokens and treat their clients — is checked by
hand, against an installed host, before a release that changes a preset.

**Accounts and secrets stay out of the repository.** The test accounts
belong to whoever runs the checks. Their passwords and second factors
are entered only on the provider's own page, in a browser. OAuth client
secrets go in only through `gxctl connector secret set <id> <field>`
(hidden prompt or standard input), never on a command line, in a file
in the repository, in an issue or in a chat. Non-secret settings
(client ids, domains) used by a script go in a local file outside the
repository, mode 0600. Write results here as pass/fail with the date
and build, never with account names or tokens.

## Google Workspace (`google`, `accounts: workspace`)

1. A web client and a *TVs and Limited Input devices* client in a Google
   Cloud project of the Workspace; consent screen *Internal*.
2. `gxctl connector add google … --domain <workspace domain>
   --device-client-id …`; both secrets set; `connector test` passes every
   step.
3. A Workspace account logs in from the web UI.
4. A personal Google account whose address is at the Workspace's domain
   (if one exists) is refused.
5. An account of another Workspace is refused.
6. `gxctl login --oidc --connector <id>` works through the device client.
7. With `--mfa` and without `--trust-2sv`, logins are refused; with
   `--trust-2sv`, they pass.

## Google consumer accounts (`google`, `accounts: any`)

1. A web client of a project with an *External* consent screen.
2. With `--jit --allow-email <address>`: that account logs in, another
   is refused.
3. With `--no-jit`: a new account is refused; an invitation for its
   address lets it in once; a second use of the link is refused; the
   invitation of another address doesn't take it.

## GitHub Actions workload federation (T5)

A GitHub-hosted runner reaches glidex through a tunnel (as for the SCIM
check) that publishes **only** `/api/auth/exchange` and the one read the
job makes (`/api/auth/whoami`), and nothing else of the web UI or the
API. The repository's workflow needs `permissions: id-token: write`; it
stores no glidex secret. Turn the tunnel off after the checks.

1. A federation on a test tenant: issuer
   `https://token.actions.githubusercontent.com`, audience `glidex`, one
   rule `repository=<owner>/<repo>`, `ref=refs/heads/main`, mapped to a
   test service account. Making it reads GitHub's discovery and keys.
2. A workflow on `main` asks GitHub for its OIDC token with that
   audience, exchanges it, and calls `/api/auth/whoami` with the token:
   it answers as the service account.
3. The same workflow on another branch is refused (`invalid_grant`); the
   audit log says no rule matched.
4. A second exchange of the same GitHub token is refused.
5. Disabling the federation makes the job's token fail at once.
6. The audit log has the exchanges as `system:federation:<tenant>/<id>`
   with the repository and ref, and no token.

## Google Workspace directory sync (`provisioning: pull`, T4)

The service account's JSON key goes in only through `gxctl connector
secret set <id> service_account_key --from-file` or the web UI's file
field, and the file is deleted afterwards. Use a test group with a few
test accounts, never the whole organization.

1. `connector test` passes `token` and `group` for each named group;
   with a scope missing from the domain-wide delegation, `token` fails
   with Google's message.
2. A sync creates the groups' members (nested groups included) as
   *managed by* the connector, with their names and emails, and the
   groups as teams (`auto`).
3. A member's first Google login lands on that user; somebody in the
   domain but in no named group is refused.
4. **Suspending** a member in the Admin console disables the glidex
   user at the next run, and ends their session; restoring them enables
   it.
5. **Removing** a member from the group disables them; adding them back
   enables them.
6. A user who logged in before the pull is adopted, not made twice.
7. Emptying the test group (or naming a group that doesn't exist)
   blocks or fails the run, and changes nothing; `--confirm` applies a
   blocked one.
8. The audit log has the runs as `system:sync:<id>`, without tokens.

## JumpCloud (`jumpcloud`)

1. An SSO application with OIDC: the redirect URI, the Login URL
   `…/api/auth/c/<id>/start`, *Client Authentication Type* **Client
   Secret Basic** (with *Post*, the token request is refused with
   `invalid_client`), and *Include group attribute*.
2. A user in the required group logs in, and lands in the mapped team.
3. A user outside it is refused.
4. **Whether the ID token carries `email_verified`.** The preset counts
   an email as verified when the token says nothing about it (the
   organization sets its users' addresses). If JumpCloud sends
   `email_verified: false` for real users, note it here: invitations and
   `allowed_emails` would then refuse them.
5. Whether the device grant is offered (`connector test`).

## JumpCloud SCIM (`provisioning: scim`, T3)

The endpoint must be reachable from JumpCloud: a reverse proxy, or a
Tailscale Funnel of the SCIM path alone (Funnel publishes only ports
443, 8443 and 10000, and isn't on every Tailscale plan; see the
operator guide's "Making the endpoint reachable"). The token is made in the web UI or by `gxctl connector
scim-token create` and pasted into JumpCloud only, never into a file or
a chat. Turn the Funnel off after the checks.

1. **Test connection** in JumpCloud's Identity Management succeeds;
   without the token, or with a wrong one, it fails.
2. **Activate**: the bound users appear on the Directory page as
   *managed by* the connector, and its bound groups as teams (`auto`)
   or in the mapped teams.
3. A provisioned user's **first login** lands on that user (matched by
   email: JumpCloud sends no `externalId`), and no second user is made.
   A JumpCloud user who isn't bound to the application is refused.
4. **Changing** a user's name or email in JumpCloud shows in glidex.
5. **Suspending** the user in JumpCloud (or `active: false`) ends their
   glidex session at once: the next page load signs them out, and their
   tokens stop working.
6. **Restoring** them enables the user; an administrator's disable in
   glidex holds even then.
7. **Unbinding** them from the application disables the glidex user and
   detaches it; its role links stay.
8. **Group** changes (members added and removed, a rename) show in the
   teams.
9. The audit log has the writes as `system:scim:<id>`, with no token.
10. Note what JumpCloud sends that glidex ignores (attributes), and any
    request that failed (status and `scimType` from the audit log).

## Two organizations (T6)

Two installs that share nothing but the network: a cluster (here
gpu-dev2, with an agent) and a standalone host serving another
organization's directory (here spark-1, split off the cluster for it).
No IdP is involved; the accounts are the directory's own.

1. At the directory host: a tenant with a project, and a user who is its
   tenant admin.
2. At the cluster: `gxctl admission offer <tenant> --directory <issuer>`
   (the directory's issuer, `https://<host>:5173/d/<dir>`). Carry the
   token to the tenant admin outside any chat or file in the repository.
3. At the directory: `gxctl tenant opt-in <tenant> --offer …` (or the
   tenant page). Both sides list the other (`admission directories`,
   `tenant clusters`).
4. Within a minute: the cluster's last feed is seconds old, and the
   directory's tenant page shows the cluster's report.
5. gxctl: a profile for the directory host (`--directory`), then one for
   the cluster with `--via` it; `whoami` there names the directory's
   user with method `directory`, and the tenant's project is listed.
6. A browser signed in at the directory opens the cluster's web UI:
   **Sign in with &lt;tenant&gt;** brings it back signed in to the cluster.
7. An API token made at the directory with `--cluster <cluster id>`:
   refused at the cluster, working after the next feed, refused again
   after it is revoked at the directory.
8. `gxctl --profile <directory profile> auth logout --revoke`: its access
   token is refused at once and its refresh token too.

## Verified addresses and pre-added users (T8)

On a cluster with a Google Workspace connector, as its operator:

1. The connector's allowed domain (and, if wanted, *Allowed emails*).
2. On Access, **Add user** with an address in that domain and a role,
   before its owner has signed in: listed as *not signed in yet*.
3. They sign in with Google: they arrive with that role.
4. An address outside the domain or the allowlist is refused at sign-in.

## Self-service tenants (T9) and members by email (T10)

On a cluster with a Google Workspace connector:

1. The operator turns on self-service tenants in the directory's
   **Settings**, with a per-user limit and automatic admission at the
   default cap (an open device pool too, to try one).
2. A Workspace user without a role creates a lab: it is admitted.
3. They add a member by email: a user who has signed in is added; an
   address with no user is offered an invitation.
4. They create a project and run a VM in it.
5. A second lab past the per-user limit is refused, as is going past
   the cap.

## Token paths on aws-lc-rs (#349, #353)

#349 moved jsonwebtoken to its `aws_lc_rs` backend, and #353 moved the
issuer's ES256 signing and WebAuthn's ES256 and RS256 checks there too.
The unit tests cover issuing and checking tokens, key rotation, a key
stored before #353, and passkey assertions; these steps show the real
paths, on a cluster running a build with both.

1. Sign in through an OIDC connector in a browser (Google's ID tokens are
   RS256; an ES256 provider too, where one is configured).
2. The cluster's own tokens: open a second server's web UI directly (not
   through the load balancer) while signed in at another, and sign in
   with the directory there (the issuer's ES256 tokens). With gxctl,
   `auth login --directory` on a profile for one server, then a command
   against another server with that profile.
3. A trusted login (another organization's directory, D69: a profile
   `--via` it) and a token exchange; skip when no second organization
   is set up, and say so.
4. A passkey sign-in on a key made before the install (ES256), and on a
   Windows Hello key (RS256) where one is at hand.
5. Google Directory sync's service-account assertion (RS256 signing):
   only with a test directory (T4 is deferred by the operator).

## Results

| Date | Build | Check | Result |
|---|---|---|---|
| 2026-11-10 | `9b69618` | Token paths on aws-lc-rs, step 2 (the cluster's own tokens) | pass. gxctl: two `auth login --directory` device-flow logins, one completed at prism1 and one at prism2; each token then listed projects at the other server, with gxctl connecting only to that server (seen with `strace`). Browser: signed in with a passkey on a server's own web UI, not through `glidex.cmkl.ai`, and its pages worked, as reported by the operator. Step 3 not run: no second organization is admitted (spark-1's admission went with gpu-dev2's old state). Step 5 not run: T4 deferred by the operator |
| 2026-11-09 | `4ff7c0b` (#349, #400) | Token paths on aws-lc-rs, steps above: step 1 (Google sign-in through `glidex.cmkl.ai`) and step 4 (passkey sign-in) | pass, as reported by the operator; two sign-ins in the cluster's audit at 11:16 and 11:17 +07. Which passkey kinds were used (ES256, RS256) wasn't reported. Steps 2 (the cluster's own tokens through gxctl and a second server), 3 (a trusted login: needs a second organization) and 5 (Directory sync: T4 deferred) **not run yet**: moved to v0.0.9 by the operator (#354) |
| 2026-11-05 | `6e61817`, then `2c05489` (T9 #260–#267, T10 #278) | Self-service tenants and members by email on the prism cluster through `glidex.cmkl.ai`, steps 1–5 above | pass, as reported by the operator. The cluster's audit has the lab's members and roles added (10:41 +07, `6e61817`) and a lab created with its project, network and VMs (17:56 +07, `2c05489`); settings, admissions and earlier labs from 2–4 November. No refusal at the limit is in the audit |
| 2026-11-03 | `72b9d7f` (T8 #243, #244, #246) | Verified addresses and pre-added users on the prism cluster through `glidex.cmkl.ai`, steps 1–4 above | pass, as reported by the operator; the pre-adds are in the cluster's audit at 08:53 and 08:59 +07 |
| — | T4 (#74–#85) | Google Workspace directory sync (`provisioning: pull`) | **not run yet**: deferred by the operator; run it before relying on the pull |
| 2026-10-22 | `ac5a0e2` (T6 #165, #167, #168) | Two organizations: steps 1–8 above, between a cluster and a standalone host on a tailnet | pass (step 8 at `985e97b`), after three fixes found by it and made in #168: a standalone host's server-info now names its token audience (gxctl's directory login asked for the wrong one); a tenant role lists the tenant's projects; `auth logout --revoke` ends a directory login (it refused for want of an API token id). Staleness and logout at the directory ending cluster sessions were checked in-process only |
| — | T5 (#92–#100) | GitHub Actions workload federation | **not run yet**: needs a test repository and a tunnel; run it before relying on federation with GitHub |
| 2026-10-19 | `4a2c488` (T3, #61–#69) | JumpCloud SCIM, through a Cloudflare quick tunnel that published only `/api/scim/v2/`. JumpCloud did lookups (`GET /Users` with a filter), created and updated users (`POST`, `PUT`), created groups and changed their members (`POST`, `PATCH`), and removed a user and a group (`DELETE`); it never called `/ServiceProviderConfig` | pass: every request succeeded (no 4xx or 5xx from JumpCloud) |

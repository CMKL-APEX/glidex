# Authentication

Every request to the control plane is authenticated, on every listener.
How depends on where it comes from:

| Client | How it is identified |
|---|---|
| `gxctl` on the host, on `/run/glidex-cp/api.sock` | The caller's Unix account, from the socket. No login. |
| Web UI | A browser session, after a login with a local password or single sign-on. |
| `gxctl` from another machine, scripts, API clients | An access token (`Authorization: Bearer …`). |

What an authenticated user may then do is decided by roles and policies:
see [Access control](access-control.md).

## The local socket

The control plane listens on `/run/glidex-cp/api.sock`. It reads the
caller's user id from the socket itself and looks up their groups on
every request:

- members of `glidex-users` (`users_group`) are let in as themselves;
- root and members of `glidex-admin` (`admin_group`) are break-glass
  administrators: they may do anything, whatever the roles and site
  policies say;
- everyone else is refused.

A local user gets a Glidex user the first time they use the socket. Their
Unix groups count as teams named `unix:<group>` for requests on the
socket, so a role can be given to a whole Unix group (see [Access
control](access-control.md#teams)).

Adding someone to a group takes effect on their next request:

```bash
sudo usermod -aG glidex-users alice
```

`/run/glidex-cp/ui.sock` is the web UI's: only the `glidex-ui` user may
use it, and it carries the browser's session, never rights of its own.

## Local accounts

People in `glidex-users` can log in to the web UI with their Unix user
name and password. The control plane doesn't read passwords itself: it
asks `glidex-authd`, a small root helper that checks them with PAM.

- `glidex-authd.socket` listens on `/run/glidex-authd/auth.sock`;
  systemd starts `glidex-authd.service` on the first login.
- It answers only the `glidex` user (the control plane).
- It uses the PAM service `/etc/pam.d/glidex`, which includes the
  system's normal authentication and account checks (`common-auth` and
  `common-account` on Debian and Ubuntu, `system-auth` elsewhere).
- Every failure is answered after a fixed delay, and repeated failures
  are rate-limited per user and overall.

The installer manages `/etc/pam.d/glidex` while it contains the line
`Managed by glidex-install`. Delete that line to keep your own edits;
later installs and the uninstaller then leave the file alone. PAM stacks
that need network access from the helper itself (LDAP or RADIUS without
`sssd`) or setuid helpers are not supported.

### Who may log in

Two lists must both allow a user, and should match:

- `allowed_groups` in `/etc/glidex/authd.json` (the helper's own);
- `auth.pam.allowed_groups` in `/etc/glidex/control-plane.json`.

Both default to `["glidex-users"]`. To let a second group log in:

```json
{ "service_user": "glidex", "allowed_groups": ["glidex-users", "lab"] }
```

```json
{ "auth": { "pam": { "allowed_groups": ["glidex-users", "lab"] } } }
```

```bash
sudo systemctl restart glidex-authd.service glidex-control-plane
```

`/etc/glidex/authd.json`:

| Key | Default | Meaning |
|---|---|---|
| `service_user` | `"glidex"` | The only user allowed to ask. |
| `allowed_groups` | `["glidex-users"]` | A user must be in one of these groups. |
| `failure_delay_ms` | `1000` | Delay before every failure answer. |
| `max_connections` | `8` | Connections served at once. |
| `allow_root` | `false` | Also answer root. For debugging only; leave it off. |

The rest of the PAM settings are in `auth.pam` (see
[Configuration](configuration.md#auth)): `enabled` turns password logins
off, `jit: false` stops new users from being created at their first
login, and `group_teams` maps Unix groups to Glidex teams, updated at
every login.

A user's local account and their password logins are the same Glidex
user. In a cluster, local accounts are per node (`alice` on one node is
not `alice` on another) unless `cluster.shared_local_accounts` is set.

## Single sign-on (OIDC)

Glidex logs users in through any OpenID Connect provider, with the
authorization code flow (PKCE) in the browser and the device
authorization grant for `gxctl`.

1. Register Glidex with the provider as a confidential web client:

   - redirect URI: `https://<ui-host>:5173/api/auth/oidc/callback`, with
     the name users type to reach the web UI;
   - grant types: authorization code, and device code if `gxctl` users
     should log in with it;
   - make the provider put the user's groups in the ID token (a `groups`
     claim) if you use `required_groups` or `group_teams`.

2. Put the client secret in a root-only file and hand it to the service
   as a credential:

   ```bash
   sudo install -m 0600 /dev/null /etc/glidex/oidc-client-secret
   sudoedit /etc/glidex/oidc-client-secret       # paste the secret
   sudo systemctl edit glidex-control-plane
   ```

   ```ini
   [Service]
   LoadCredential=oidc-client-secret:/etc/glidex/oidc-client-secret
   ```

3. Configure `auth.oidc` in `/etc/glidex/control-plane.json`:

   ```json
   {
     "auth": {
       "allowed_origins": ["https://glidex.example.org:5173"],
       "oidc": {
         "enabled": true,
         "issuer": "https://idp.example.org",
         "client_id": "glidex",
         "redirect_uri": "https://glidex.example.org:5173/api/auth/oidc/callback",
         "required_groups": ["glidex-users"],
         "group_teams": { "lab-staff": "staff" }
       }
     }
   }
   ```

4. Restart: `sudo systemctl restart glidex-control-plane`. The login page
   now has a **Sign in with Single sign-on** button.

Set `redirect_uri` explicitly: by default it is built from the first of
`auth.allowed_origins`, which, when that list is empty, is the host's
own name. The issuer must be an `https://` URL; the control plane fetches
its discovery document and keys from it, so it needs outbound HTTPS to
the provider.

Who gets in:

- `required_groups`: only members of one of these groups (from
  `groups_claim`). Empty lets in everyone the provider authenticates for
  this client, so set it, or restrict the client at the provider.
- `allowed_domains`: only users whose email is in one of these domains.
- `jit: false`: only people who already have a Glidex user.

`group_teams` maps provider groups to Glidex teams, which must exist
(`gxctl team create staff`); memberships are updated at every login.
Users are identified by the provider's issuer and subject, never by
email or name.

From another machine:

```bash
gxctl --url https://glidex.example.org:8841 login --oidc
```

`gxctl` prints a code and a URL to open in a browser, then saves a token
valid for one day.

## Login connectors

Each way to log in is a *login connector*. `auth.pam` and `auth.oidc`
above appear as the connectors `file-pam` and `file-oidc`; they can only
be changed in the file. Directory administrators can add more PAM and
OIDC connectors, for example a second identity provider, from the web
UI's **Connectors** page or with `gxctl connector`:

```bash
gxctl connector add oidc corp --issuer https://login.example.com \
  --client-id glidex --name "Example Corp" --require-group glidex-users \
  --team lab-staff=staff
gxctl connector secret set corp                 # hidden prompt
gxctl connector secret set corp --from-file /path/to/secret
gxctl connector test corp
gxctl connector list
```

- Each OIDC connector gets its own button on the login page, and its own
  redirect URI: `https://<ui-host>:5173/api/auth/c/<id>/callback`.
- Secrets are read from a hidden prompt, standard input or a file, never
  the command line, and are never shown again. They are stored encrypted
  with the host's secrets key (`secrets.kek` beside the database, or the
  `secrets-kek` credential); back that key up with the database.
- `--mfa` admits only logins the provider reports as multi-factor (`amr`
  contains `mfa`, or `acr` is one of the `--mfa-acr` values).
- `--token-max-days N` caps the tokens a login through the connector
  mints (default 30 days).
- `--jit` / `--no-jit`, `--require-group`, `--team GROUP=TEAM` and
  `--domain` work as in the file settings.
- `--default-tenant <t>` makes new users members of tenant `<t>`. If
  that is the `admin` tenant or another operator tenant, a connector that creates users must also
  have `--require-group`.
- Changing a connector needs a recent login (see [Sessions and
  re-authentication](#sessions-and-re-authentication)).

Run `gxctl connector` without arguments for every option.

### Google

A `google` connector logs in Google accounts. In the Google Cloud
console, create an OAuth client of type *Web application* with the
connector's redirect URI. For `gxctl login --oidc`, create a second
client of type *TVs and Limited Input devices*: Google only allows
device logins through that type.

```bash
# Google Workspace: accounts of these domains.
gxctl connector add google gws --client-id <web-client-id> --domain example.org \
  --device-client-id <tv-client-id> --name "Example (Google)"
gxctl connector secret set gws client_secret
gxctl connector secret set gws device_client_secret
gxctl connector test gws
```

- **Workspace** (the default) allows the accounts of the `--domain`
  domains. It checks the Workspace the account belongs to, never the
  email address: anyone can open a personal Google account under any
  address. Set the OAuth consent screen's user type to *Internal* as
  well, so only the Workspace's accounts can use the client.
- Google doesn't report whether a login used a second factor. If the
  Workspace enforces 2-step verification, `--trust-2sv` makes its logins
  count as multi-factor. The `admin` tenant and operator tenants only
  take a Workspace connector with `--trust-2sv`.
- **`--consumer`** allows personal Google accounts too, but only for
  people you already know: with `--jit` it needs `--allow-email` for each
  address; without it, people get in through an [invitation](#invitations).
  A personal account can't join the `admin` tenant or an operator tenant.
- Google sends no groups, so `--require-group` and `--team` don't apply;
  give people roles in Glidex instead.

### JumpCloud

A `jumpcloud` connector logs in JumpCloud users. In JumpCloud, add an
SSO application with OIDC, the connector's redirect URI, and *Include
group attribute* if logins should carry the user's groups.

```bash
gxctl connector add jumpcloud jc --client-id <client-id> --groups-claim memberOf \
  --require-group glidex-users --team lab-staff=staff
gxctl connector secret set jc
```

- `--groups-claim` is the name set under *Include group attribute*
  (`memberOf` by default); `--require-group` and `--team` read it.
- `--allow-email` limits which addresses get a new account with `--jit`.

## Invitations

An invitation lets one person into a tenant, with a role, through one
connector, even when the connector doesn't create users (`--no-jit`):
the usual way to let in people with personal Google accounts.

```bash
gxctl invite create web bob@gmail.com --connector google-personal --role member
gxctl invite list web
gxctl invite revoke <id>
```

- `create` prints a link, once. Send it to the person; it lets in the
  first login through that connector with that verified email address,
  once, within 7 days (`--days`, at most 30). Another account can't use
  it.
- `--role` is `member` (the default: membership only),
  `role.tenant-admin` or `role.tenant-auditor`.
- Tenant administrators invite to their tenant from its page in the web
  UI too (**Invitations**), where the link is likewise shown once.
- Invitations into the `admin` tenant or an operator tenant go only
  through a connector that requires MFA.

## Access tokens

Tokens are for `gxctl` on other machines, scripts and API clients. A
token is shown once, when it is made; Glidex keeps only a hash of it.

| Kind | Acts as | Made with |
|---|---|---|
| Personal | Its owner. Never more than the owner may do; roles given to the token narrow it further. | `gxctl token create <name>`, the web UI's **Tokens** page |
| Service account | Only the project roles it was given, in one project. | `gxctl token create <name> --service-account --project <p> --role editor` |

```bash
gxctl token create ci --days 30 --service-account --project web --role operator
gxctl token list
gxctl token revoke <id>
```

- A token lives `auth.tokens.default_days` (90) unless `--days` says
  otherwise, and at most `auth.tokens.max_days` (365).
- `gxctl login --oidc` gets a token valid for one day; `gxctl auth login
  --pam-user <user>` one capped by its connector (30 days by default).
- Clients send it as `Authorization: Bearer <token>`; `gxctl` reads it
  from `GLIDEX_TOKEN` or the file `gxctl login --token` saved
  (`~/.config/glidex/token`, mode 0600).
- A disabled user's personal tokens stop working.

Store tokens in a secret manager or a mode 0600 file, not in shell
history or scripts. Revoke a token you no longer need, and any token
that may have been exposed. `gxctl token list` shows when each was last
used.

## Sessions and re-authentication

A web UI login starts a session, kept in a cookie. It ends after
`auth.session.idle_minutes` (30) without a request, or
`auth.session.absolute_hours` (12) after login, whichever comes first,
or when the user signs out.

Some actions need a login in the last 10 minutes:

- writing site policies;
- adding, changing or removing login connectors and their secrets;
- managing admissions and deleting tenants;
- host network changes that can cut the host off (installing Open
  vSwitch, DPDK, committing or removing uplinks, deleting bridges);
- cluster membership and recovery (joining, join tokens, removing nodes,
  CA and secrets key rotation, OVN settings).

In the web UI, such an action opens a "This action needs a recent login"
dialog; after signing in again the request goes through. On the local
socket a request always counts as fresh. A personal token acting only
as its owner never does: run those actions from the web UI, from the
host's socket, or with a token that has its own roles.

## Who is logged in

```bash
gxctl whoami          # method, teams, roles
gxctl user list       # every user and their identities
```

The audit log records every login, write and denied request (see
[Access control](access-control.md#audit-log)).

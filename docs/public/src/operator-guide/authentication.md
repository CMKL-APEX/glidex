# Authentication

Every request to the control plane is authenticated, on every listener.
How depends on where it comes from:

| Client | How it is identified |
|---|---|
| `gxctl` on the host, on `/run/glidex-cp/api.sock` | The caller's Unix account, from the socket. No login. |
| Web UI | A browser session, after a login with a local password or single sign-on. |
| `gxctl` from another machine, scripts, API clients | An access token (`Authorization: Bearer …`). |
| CI jobs and other workloads | A short-lived token, exchanged for the workload's own signed token through a [workload federation](workload-federation.md). |

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
own name. The web UI needs no `allowed_origins` entry for the name
users reach it by; [`GLIDEX_UI_HOSTS`](configuration.md#web-ui) decides
which names it answers. The issuer must be an `https://` URL; the
control plane fetches its discovery document and keys from it, so it
needs outbound HTTPS to the provider.

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
be changed in the file. Directory administrators can add more, from the
web UI (**Directory** page, **Connectors** tab, **+ Add Connector**) or
with `gxctl connector`:

| Kind | For |
|---|---|
| `pam` | Local accounts of the host. |
| `oidc` | Any OpenID Connect provider. |
| `google` | Google Workspace, or personal Google accounts (see [Google](#google)). |
| `jumpcloud` | JumpCloud SSO (see [JumpCloud](#jumpcloud)). |

`google` and `jumpcloud` are OIDC with the provider's endpoints and
rules built in: you give a client id and secret, not an issuer.

```bash
gxctl connector add oidc corp --issuer https://login.example.com \
  --client-id glidex --name "Example Corp" --require-group glidex-users \
  --team lab-staff=staff
gxctl connector secret set corp                 # hidden prompt
gxctl connector secret set corp --from-file /path/to/secret
gxctl connector test corp
gxctl connector list
```

- Each `oidc`, `google` and `jumpcloud` connector gets its own **Sign
  in with &lt;name&gt;** button on the login page, and its own redirect URI:
  `https://<ui-host>:5173/api/auth/c/<id>/callback`. The web UI's
  connector form shows it, on the address you opened the UI with.
- Secrets are read from a hidden prompt, standard input or a file, never
  the command line, and are never shown again. In the web UI, each
  secret field has its own **Set secret** button. They are stored encrypted
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
- `--provisioning scim` and `--group-mode mapped|auto` let the IdP
  create and remove the connector's users (`oidc` and `jumpcloud` only);
  see [Provisioning with SCIM](#provisioning-with-scim).
  `--provisioning pull` makes Glidex read them from Google Workspace
  instead (`google` only); see [Directory sync from Google
  Workspace](#directory-sync-from-google-workspace).
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
gxctl connector secret set gws client_secret            # hidden prompt
gxctl connector secret set gws device_client_secret
gxctl connector test gws
```

In the web UI, choose **Kind** *Google*, then **Google accounts**:
*Google Workspace (by domain)* or *Any Google account (invited or listed
people)*. The web client and the device client each have their own
**Set secret** button.

- **Workspace** (the default) allows the accounts of the `--domain`
  domains. It checks the Workspace the account belongs to, never the
  email address: anyone can open a personal Google account under any
  address. Set the OAuth consent screen's user type to *Internal* as
  well, so only the Workspace's accounts can use the client. Public mail
  domains (`gmail.com`, `googlemail.com`) can't be Workspace domains.
- Logins must come with a verified email address.
- Google doesn't report whether a login used a second factor. If the
  Workspace enforces 2-step verification, `--trust-2sv` makes its logins
  count as multi-factor. The `admin` tenant and operator tenants only
  take a Workspace connector with `--trust-2sv`.
- **`--consumer`** allows personal Google accounts too, but only for
  people you already know: an [invitation](#invitations), an address in
  `--allow-email`, or a Google account already linked to a Glidex user.
  With `--jit` (create users at their first login) it must also have
  `--allow-email`; without `--jit`, the default for connectors made with
  `gxctl`, new people get in by invitation only.
  A personal account can't join the `admin` tenant or an operator tenant.
- Without a device client, `gxctl auth login --oidc` through the
  connector fails with `device_flow_unsupported`. `connector test`
  reports whether device logins are set up.
- Google sends no groups at login, so `--require-group` doesn't apply,
  and `--team` only does with a [directory
  sync](#directory-sync-from-google-workspace), which reads the groups
  from Google. Otherwise give people roles in Glidex.

#### Directory sync from Google Workspace

A Workspace `google` connector can **pull** its users from Google: the
members of the Google groups you name, nested groups included, get
Glidex users ahead of their first login, and nobody else logs in
through it. Glidex reads the directory every 15 minutes, and on demand.
It is the Google Workspace counterpart of [SCIM](#provisioning-with-scim):
Google can't push users to Glidex, so Glidex reads them instead.

Glidex reads the directory as a Google Cloud **service account** with
**domain-wide delegation**, read-only:

1. In the Google Cloud console, in a project of your organization:
   enable the **Admin SDK API**, create a service account (it needs no
   roles in the project), and create a **JSON key** for it. Keep the
   key file safe; it goes only into Glidex's secret, below.
2. In the Google Admin console, **Security → Access and data control →
   API controls → Domain-wide delegation → Add new**: the service
   account's **client ID** (the number on its page, not its email), and
   exactly these two scopes:

   ```text
   https://www.googleapis.com/auth/admin.directory.user.readonly
   https://www.googleapis.com/auth/admin.directory.group.readonly
   ```
3. Pick the **administrator** it acts as: a super administrator, or a
   delegated administrator who may read users and groups.
4. Switch the connector to the pull, and set the key:

   ```bash
   gxctl connector update gws --no-jit --provisioning pull --group-mode auto \
     --sync-admin admin@example.org --sync-group glidex-users@example.org
   gxctl connector secret set gws service_account_key --from-file <service-account-key.json>
   gxctl connector test gws      # the key, the token, and each group
   gxctl connector sync gws
   ```

   Give the key with `--from-file`: it is several lines long, and the
   hidden prompt reads one. In the web UI: the connector's form,
   **Provisioning** *Directory sync*, **Save**, then **Set service
   account key** (choose the JSON file, or paste it). Either way, delete
   the key file afterwards: Glidex keeps the key encrypted and never
   shows it, not even in the form.
5. Let the control plane reach Google: it needs outbound HTTPS to
   `oauth2.googleapis.com` and `admin.googleapis.com`. In a cluster, the
   runs happen on the leader, so every server node needs it. Unlike
   SCIM, nothing has to reach Glidex from outside.

The settings, as `gxctl` flags and in the form (**Directory sync**):

| Flag | Form | Meaning |
|---|---|---|
| `--sync-admin EMAIL` | **Administrator to act as** | The Workspace administrator of step 3. |
| `--sync-group GROUP` (repeat it) | **Groups** | 1 to 50 group emails. Their members, nested groups' members included, are who gets in. |
| `--sync-interval MIN` | **Every (minutes)** | Minutes between runs: 15 by default, at least 5. |
| `--sync-max-disable PCT` | **Stop above (%)** | The [safety stop](#the-safety-stop): 10 by default, 1 to 100. |
| `--ssh-keys-field SCHEMA.FIELD` | **SSH key field** | Optional: where the users' [SSH keys](#ssh-keys-from-google) are. |

- A pull needs a Workspace connector (*Google Workspace (by domain)*,
  not `--consumer`) with `--no-jit`; the web UI turns JIT off for you.
- Any of the `--sync-…` flags sets them all: repeat the ones you keep
  when you change one, or edit them in the web UI.
- `--group-mode auto` makes each named group a Glidex team, named after
  the group. With `mapped` (the default), `--team GROUP=TEAM` puts a
  group's members in an existing team, by the group's email or name. In
  the form, this is **Groups**, *A team per IdP group* or *Mapped to
  teams below*. Only the named groups become teams; members of a nested
  group count as members of the group that holds it. A group you stop
  naming leaves its team behind, as a group deleted at a SCIM IdP does
  (see [Teams](access-control.md#teams)).
- `--default-tenant <t>` makes new users members of tenant `<t>`.
- `connector test` checks the key is set, that the service account gets
  a token as the administrator, and that it can read each group.

<!-- TODO(verify): gxctl can't clear a connector's sync settings, so `gxctl connector update <id> --provisioning none` on a pulled connector is refused ("sync settings are for provisioning pull"); the web UI's form clears them. Document how to stop a pull with gxctl once fixed. -->
To stop pulling, set **Provisioning** back to *None* in the web UI's
connector form, or delete the connector (below).

##### What a run does

A run reads the groups' members and those users from Google first, then
compares them with the connector's users and teams, then applies the
difference:

- **New members** get a Glidex user, managed by the connector, and join
  `--default-tenant`. Someone already suspended in Google gets none.
- **First login.** It is matched to the pulled user by Google's account
  id (the subject of Google's login), never by email address. Someone
  the pull didn't bring in is refused (`login_not_allowed`).
  Invitations can't be taken through the connector, and nobody can add
  it to their account as a sign-in method. `--mfa` and `--trust-2sv`
  still apply.
- **Changes** in Google (name, primary email) reach Glidex at the next
  run.
- **Leaving.** Someone suspended or archived in Google, deleted there,
  or no longer in any of the groups is disabled in Glidex: their
  sessions end, their personal tokens are revoked, and they leave the
  teams the pull put them in. The user stays managed by the connector.
- **Coming back.** Once they are active and in a group again, the next
  run enables the user. Not if an administrator disabled it in Glidex:
  then only an administrator enables it again.
- **Users who logged in before the pull** through the same connector
  are adopted: the pull takes over the user they already have, with its
  roles, rather than making a second one. So are users who added the
  connector as a sign-in method before Glidex refused that; the audit
  log's `oidc-link` entries show who did, and removing that sign-in
  method before the next run keeps their account out of the pull.
- **What Google owns.** A pulled user's name, email and whether it is
  enabled are Google's, and the pulled teams' names and memberships
  too: see [Users an IdP
  manages](access-control.md#users-an-idp-manages).
- Every change a run applies is in the audit log, done by
  `system:sync:<connector-id>` (the action `syncConnector`); a run that
  changed nothing isn't recorded.

A disabled connector isn't pulled on schedule. Deleting the connector disables its
users, unless `gxctl connector remove gws --detach-users` (or the choice
in the web UI) keeps them, as for SCIM.

##### The safety stop

A run that would disable more than `--sync-max-disable` percent of the
connector's users changes nothing at all, and shows as **blocked**,
listing who it would disable. Up to 5 can always be disabled. It guards
against a group renamed, emptied or deleted by mistake, which would
otherwise read as "everyone left".

- If a group was changed by mistake, fix it in Google. The next run
  (or **Sync now**) sees it and applies normally.
- If the change is right, apply it: `gxctl connector sync gws
  --confirm`, or **Review blocked sync** in the web UI, which lists the
  users first. This reads Google again and applies what it finds then,
  without the stop, once. It needs a recent login (see [Sessions and
  re-authentication](#sessions-and-re-authentication)).

An error from Google, or an answer Glidex can't read, changes nothing
either: the run shows as **failed**, with Google's message, and the
next one tries again. Glidex retries Google's `429` and `5xx` answers a
few times within a run; any other error ends it.

##### Watching the runs

`gxctl connector show gws` prints the pull's settings and the last run;
the **Connectors** tab shows it under each pulled connector:

| State | Meaning |
|---|---|
| `ok` (*synced*) | Applied. How many users were created, adopted, updated, disabled and enabled. |
| `FAILED` (*failed*) | Google's error, or a missing or unreadable key. Nothing changed. |
| `BLOCKED` (*blocked*) | The safety stop: who it would disable. Nothing changed. |

**Sync now** (or `gxctl connector sync gws`) runs one at once and shows
its result; if a run of that connector is already going, it is refused
with `409 sync_running`. `directory-auditor` sees the status;
running a sync takes `directory-admin`.

##### SSH keys from Google

With `--ssh-keys-field <schema>.<field>`, each run also reads the users'
SSH public keys from a Workspace custom attribute:

1. In the Google Admin console, add a custom attribute for users (a
   category, which is the schema, and a field in it), of type text,
   multi-valued.
2. Put one OpenSSH public key per value (`ssh-ed25519 AAAA…
   alice@laptop`) on the users who need it.
3. Set `--ssh-keys-field <schema>.<field>` (in the form, **SSH key
   field**), for example `glidex.ssh_keys`.

<!-- TODO(verify): how the Admin console's custom attribute category and field names map to the Directory API's schema and field names (spaces, case); the docs assume they are the same. -->

- Only public keys are kept: OpenSSH keys of a known type, without
  repeats, at most 32 per user. Anything else, such as a private key
  pasted by mistake, is dropped.
- Each run replaces the user's keys with what Google holds.
- The keys go into a VM only through a guest credential: when the user
  has no local account on the host, the web UI's **Add Credential** form
  fills them in (see [Guest
  credentials](../user-guide/credentials.md#adding-a-credential)).
  Removing a key in Google doesn't remove it from credentials or VMs
  that already have it.
- Whoever can edit the attribute in Google decides which keys a user
  is offered: keep it to administrators.

### JumpCloud

A `jumpcloud` connector logs in JumpCloud users. In JumpCloud, add an
SSO application with OIDC:

- **Redirect URIs:** the connector's redirect URI,
  `https://<ui-host>:5173/api/auth/c/<id>/callback`.
- **Login URL** (where JumpCloud's user portal sends people):
  `https://<ui-host>:5173/api/auth/c/<id>/start`.
- **Client Authentication Type:** *Client Secret Basic*. Glidex sends
  its client secret that way, the OpenID Connect default; with *Client
  Secret Post* JumpCloud refuses the login (`invalid_client`).
- *Include group attribute*, if logins should carry the user's groups,
  and the user groups allowed to use the application.

```bash
gxctl connector add jumpcloud jc --client-id <client-id> --groups-claim memberOf \
  --require-group glidex-users --team lab-staff=staff
gxctl connector secret set jc
```

- `--groups-claim` is the name set under *Include group attribute*
  (`memberOf` by default); `--require-group` and `--team` read it.
- `--allow-email` limits which addresses get a new account with `--jit`.
- JumpCloud's addresses are set by your organization's administrators,
  so an address the login doesn't mark as unverified counts as verified
  (for `--allow-email` and invitations).

In the web UI, choose **Kind** *JumpCloud*; the form shows the redirect
URI and the Login URL to enter in JumpCloud.

### Provisioning with SCIM

A JumpCloud or OIDC connector can be **provisioned**: the IdP creates,
updates and removes its users and groups in glidex over SCIM 2.0, ahead
of their logins. Then only people the IdP provisions can log in, and
deactivating someone in the IdP takes their access away at once.

1. Switch the connector to SCIM. It stops creating users at login:

   ```bash
   gxctl connector update jc --no-jit --provisioning scim --group-mode auto
   ```

   `--group-mode auto` makes a glidex team per IdP group. The default,
   `mapped`, fills existing teams named by `--team GROUP=TEAM`. In the
   web UI, these are **Provisioning** and **Groups** in the connector's
   form.
2. Make a token for the IdP. It is shown once:

   ```bash
   gxctl connector scim-token create jc --name jumpcloud
   ```

   Or use the connector's **SCIM** button on the **Connectors** tab,
   which also shows the endpoint URL. A token lasts 180 days by default
   (`--days`, at most 365). A connector can have five at once, so make
   the next one before the old one expires, then revoke the old one:

   ```bash
   gxctl connector scim-token list jc              # ids, names, expiry; never the tokens
   gxctl connector scim-token revoke jc <token-id>
   ```

   The token is a password for the connector's users: whoever holds it
   can create users and put them in teams. Paste it straight into the
   IdP, keep any copy in a secret manager, never in shell history,
   scripts or tickets, and revoke a token that may have been exposed.
   Making and revoking tokens needs a recent login; a connector that
   isn't `--provisioning scim` refuses them (`409 not_provisioned`).
   Expired tokens stay listed for 30 days.
3. Make the endpoint reachable from the IdP (see below).
4. In JumpCloud, open the application's **Identity Management**, choose
   **SCIM 2.0**, and enter:
   - **Base URL:** the endpoint, for example
     `https://<name>.<tailnet>.ts.net/api/scim/v2/c/jc`;
   - **Token key:** the `gxs_…` token.

   **Test connection**, then **Activate**. Users bound to the
   application are pushed, and so are its user groups, as glidex groups.

What happens then:

- **First login.** It is matched to the provisioned user by the IdP's
  id or, as for JumpCloud (which sends none), by the verified email
  address. Somebody the IdP didn't provision is refused. The
  connector's `--require-group` and `--mfa` still apply; invitations
  can't be taken through it, nobody can add it to their account as a
  sign-in method, and teams aren't synced at login: the IdP pushes them.
- **Deactivating** someone in the IdP disables the glidex user: their
  sessions end, their tokens are revoked, and they leave the teams the
  IdP put them in.
- **Removing** someone from the application disables the user and
  detaches it from the IdP. Its records and role links stay; delete the
  user from the **Directory** page if it should go.
- **What the IdP owns.** A provisioned user's name, email and whether
  it is enabled are the IdP's: the web UI says *managed by JumpCloud*,
  and glidex refuses edits to them. An administrator can still
  **disable** one at once, without waiting for the IdP; the IdP can't
  undo that.
- **Users who logged in before SCIM** (with JIT) are adopted when the
  IdP pushes them by the same email address: they aren't made twice.
- Deleting the connector disables its users, unless `gxctl connector
  remove jc --detach-users` (or the choice in the web UI) keeps them as
  the directory's own.

#### Making the endpoint reachable

The IdP calls glidex from its own servers, so the endpoint has to be
reachable from the internet; glidex doesn't open anything itself. It
lives on the API (`/scim/v2/c/<id>`) and on the web UI under `/api`.
Use a reverse proxy, or a tunnel that publishes only that path.

With **Tailscale Funnel**: Funnel listens on ports **443, 8443 and
10000 only**, and the web UI is on 5173. So publish 443 and forward it
to the web UI, and publish the SCIM path alone, so the rest of the web
UI stays on the tailnet:

```bash
sudo tailscale funnel --bg --set-path=/api/scim/v2/ \
  https://<name>.<tailnet>.ts.net:5173/api/scim/v2/
tailscale funnel status
```

- The IdP's URL then has no port:
  `https://<name>.<tailnet>.ts.net/api/scim/v2/c/<id>`.
- Funnel needs HTTPS certificates and the `funnel` node attribute in
  the tailnet's policy, and isn't part of every Tailscale plan (not
  Starter). Without it, use a reverse proxy, or another tunnel that can
  publish one path (Cloudflare Tunnel's ingress rules can).
- If the web UI doesn't use the Tailscale certificate (`installation.md`,
  `--tailscale-cert`), forward to
  `https+insecure://127.0.0.1:5173/api/scim/v2/` instead.
- Turn it off with `sudo tailscale funnel --https=443
  --set-path=/api/scim/v2/ off`.

Only the SCIM token opens the endpoint: any other request gets `401`.
A disabled connector, or one that isn't `--provisioning scim`, answers
`403`. It answers at most 20 requests a second per connector. What the
endpoint supports is in the [REST API](../api/rest.md#scim-20-for-identity-providers).

## Invitations

An invitation lets one person into a tenant, with a role, through one
connector, even when the connector doesn't create users (`--no-jit`):
the usual way to let in people with personal Google accounts.

```bash
gxctl invite create web bob@example.org --connector google-personal --role member
gxctl invite list web
gxctl invite revoke <id>
```

- **Who invites.** Tenant administrators, to their own tenant; directory
  administrators, to any tenant. Giving a tenant role with it needs the
  right to give that role.
- **The link.** `create` prints a link, once:
  `https://<ui-host>:5173/invite/<id>#<secret>`. It can't be shown again;
  if it is lost, revoke the invitation and make another. Send it the way
  you would send a password, to the invited person only.
- **Who can take it.** The first login through that connector, with that
  verified email address, that brings the link. It is taken once; another
  account, or the same address without the link, can't use it. The
  connector's own rules still apply: through a Workspace `google`
  connector, the account must still be in one of its domains.
- **Lifetime.** 7 days, or `--days N` (1 to 30). `invite list` shows each
  invitation as `open`, `taken` or `expired`; taken and expired ones stay
  listed for 30 days. `revoke` deletes an invitation.
- `--role` is `member` (the default: membership only),
  `role.tenant-admin` or `role.tenant-auditor`.
- `--connector` is an enabled `oidc`, `google` or `jumpcloud` connector,
  not one [provisioned by SCIM](#provisioning-with-scim) or by a
  [directory sync](#directory-sync-from-google-workspace): its users
  come from the IdP, and an invitation can't be taken through it.
  Invitations into the `admin` tenant or an operator tenant go only
  through a connector that requires MFA and is allowed there.

In the web UI, a tenant's page has an **Invitations** card: the list,
**Invite** (email, role, connector), and the link shown once with
**Copy**. A tenant's first administrator can be invited when the tenant
is created (see [Tenants and
admissions](access-control.md#tenants-and-admissions)).

### Accepting an invitation

The invited person opens the link. The page says which tenant and role
it is for, and which address and connector to sign in with; **Sign in
with &lt;connector&gt;** takes them to the provider. When they come back
signed in with that address, they are a member of the tenant with the
role. If they already have a Glidex user with that sign-in, the
invitation adds to it.

A link that was used, revoked or has expired says so: ask for a new one.
A login that can't take the invitation is refused like any other login
(`login_not_allowed`).

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
- A disabled user's personal tokens stop working; a deleted user's are
  deleted (see [Removing people](access-control.md#removing-people)).

Store tokens in a secret manager or a mode 0600 file, not in shell
history or scripts. Revoke a token you no longer need, and any token
that may have been exposed. `gxctl token list` shows when each was last
used.

## Passkeys

Users can sign in with a **passkey**: their device's fingerprint, face or
PIN (or a security key) instead of a password. A passkey is one more
sign-in method of the same user; it counts as multi-factor, and it can
answer the re-authentication a sensitive action asks for.

- Each user adds their own on the **Account** page (**Add a passkey**),
  after a recent sign-in. Nobody, administrators included, can add a
  passkey to someone else's account; administrators can remove one.
- The login page then offers **Sign in with a passkey**, with no user
  name to type.
- A user keeps another sign-in method, or a second passkey: glidex
  refuses to remove the method that would leave a single passkey as the
  only way in, so losing one device isn't a lock-out.

Passkeys belong to a domain. Set it to the domain your web UI's names
share, so that a passkey made on one name works on the others (and on a
cluster's other servers):

```json
{
  "auth": {
    "passkeys": { "rp_id": "example.com" }
  }
}
```

in `/etc/glidex/control-plane.json` (here for a web UI at
`https://glidex.example.com:5173`; on a tailnet, the tailnet's
`<tailnet>.ts.net`). Without it, each web UI name has its own passkeys,
and the Account page says so. `"enabled": false` turns passkeys off.
Changing `rp_id` later makes existing passkeys stop working: their users
add new ones.

gxctl doesn't sign in with passkeys; it keeps tokens, PAM and OIDC.

## Workload federation (CI without stored tokens)

A CI job usually needs a service account's token to deploy. With
**workload federation** it needs none: the job asks its CI platform for
an OIDC token (GitHub Actions, GitLab CI and Kubernetes give one to every
job), and trades it at glidex for a short-lived token that acts as a
service account of the tenant. Nothing secret is stored in the CI system.

- A tenant trusts an **issuer** and an **audience**, with **rules**:
  each rule names exact claim values (the repository, and usually the
  branch or environment) and the service account a matching job acts
  as. The first rule that matches wins; there are no wildcards.
- The token glidex gives lasts the rule's lifetime (15 minutes by
  default, at most an hour), isn't renewable, and has exactly the
  service account's roles. Each CI token is exchanged once.
- Disabling or deleting the federation, changing the rule, or revoking
  the service account ends the tokens it gave at once.
- Tenant administrators manage their tenant's federations on the
  tenant's page (**Workload federation**) or with `gxctl federation`;
  making one needs a recent login.

```bash
# A service account with the roles the job needs, in one of the tenant's projects:
gxctl token create deployer --service-account --project web --role operator
gxctl federation add acme github --issuer https://token.actions.githubusercontent.com \
  --audience glidex --match repository=acme/infra --match ref=refs/heads/main \
  --service-account <token id>
```

The job exchanges its token at `POST /api/auth/exchange` (RFC 8693),
naming the federation as `<tenant slug>/<federation id>`, and gets
`{access_token, expires_in}`. A refused exchange always answers
`invalid_grant`; why is in the audit log (`system:federation:<id>`).

**GitHub Actions:**

```yaml
permissions:
  id-token: write          # lets the job ask GitHub for an OIDC token
steps:
  - name: glidex token
    run: |
      JWT=$(curl -sfH "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
        "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=glidex" | jq -r .value)
      TOKEN=$(curl -sf https://glidex.example.com/api/auth/exchange \
        -d grant_type=urn:ietf:params:oauth:grant-type:token-exchange \
        -d subject_token_type=urn:ietf:params:oauth:token-type:jwt \
        -d federation=acme/github -d subject_token="$JWT" | jq -r .access_token)
      echo "::add-mask::$TOKEN"
      echo "GLIDEX_TOKEN=$TOKEN" >> "$GITHUB_ENV"
  - run: curl -sf -H "Authorization: Bearer $GLIDEX_TOKEN" https://glidex.example.com/api/vms
```

Rules for GitHub match `repository`, `ref` and `environment` (or the
whole `sub`, such as `repo:acme/infra:environment:production`).

**GitLab CI** (issuer `https://gitlab.com`, or your instance's URL):

```yaml
deploy:
  id_tokens:
    GLIDEX_JWT:
      aud: glidex
  script:
    - >
      TOKEN=$(curl -sf https://glidex.example.com/api/auth/exchange
      -d grant_type=urn:ietf:params:oauth:grant-type:token-exchange
      -d subject_token_type=urn:ietf:params:oauth:token-type:jwt
      -d federation=acme/gitlab -d subject_token="$GLIDEX_JWT" | jq -r .access_token)
```

Rules for GitLab match `project_path` and `ref` (or `environment`).

**Kubernetes:** use the cluster's service account issuer (its
discovery must be reachable by glidex, for example published with
`--service-account-issuer` and `--service-account-jwks-uri`), a
projected token with audience `glidex`, and rules on `sub`
(`system:serviceaccount:<namespace>:<name>`).

**Issuers on your own network.** Glidex reads an issuer's discovery and
keys only from public addresses: an issuer whose host resolves only to
loopback, private, link-local or unique-local addresses (a self-hosted
GitLab on the LAN, an internal Kubernetes API) is refused with
`issuer_unreachable` (*has no public address*). To trust one, set
`auth.federation.allow_private_issuers` to `true` in
`/etc/glidex/control-plane.json` and restart the control plane. That
lets every tenant administrator point a federation at internal
addresses, so turn it on only when you need it.

Every field, limit, issuer requirement and error is on the
[Workload federation](workload-federation.md) page.

## Sessions and re-authentication

A web UI login starts a session, kept in a cookie. It ends after
`auth.session.idle_minutes` (30) without a request, or
`auth.session.absolute_hours` (12) after login, whichever comes first,
or when the user signs out.

Some actions need a login in the last 10 minutes:

- writing site policies;
- adding, changing or removing login connectors, their secrets and
  their SCIM tokens, and applying a [blocked directory
  sync](#the-safety-stop);
- managing admissions and deleting tenants;
- adding, changing and removing [workload federations](workload-federation.md);
- host network changes that can cut the host off (installing Open
  vSwitch, DPDK, committing or removing uplinks, deleting bridges);
- cluster membership and recovery (joining, join tokens, removing nodes,
  CA and secrets key rotation, OVN settings).

In the web UI, such an action opens a "This action needs a recent login"
dialog; after signing in again the request goes through. A user who
signs in with single sign-on picks their provider there, and is asked
to sign in at it again even if it still has a session (Glidex sends
`prompt=login` and `max_age=0`). On the local
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

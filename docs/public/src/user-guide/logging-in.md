# Logging in

How you prove who you are depends on where you connect from:

| From | How |
|------|-----|
| The web UI | Your account's password (PAM), single sign-on (OIDC, Google, JumpCloud) or a passkey, whichever the site enables |
| `gxctl` on a Glidex host | Nothing: the local socket knows your Unix account |
| `gxctl` on another machine | `gxctl auth login` once; it saves a token in a login profile |
| Scripts and the API | An API token, sent as `Authorization: Bearer <token>` |

Which login methods exist, and who may log in, is set by the operator
(see [Authentication](../operator-guide/authentication.md)).

## The web UI

Open `https://<host>:5173` (`gxctl ui` prints the address it knows). The
sign-in page shows what the site enables:

- **Username** and **Password**, then **Sign in**: a local account of the
  host, or of the site's PAM setup. By default the account must be in the
  `glidex-users` group.
- **Sign in with &lt;provider&gt;** (or **Sign in with SSO**): your
  organisation's single sign-on, one button per provider the site
  offers, such as its Google Workspace or JumpCloud.
- **Sign in with a passkey**: a passkey you added earlier (see
  [Passkeys](#passkeys)). There is no user name to type.

If the page says *No login method is enabled on this host*, ask an
administrator, or use `gxctl` on the host.

Some single sign-on providers create your Glidex account for you ahead
of time (*provisioning*): the identity provider pushes it over SCIM, or
Glidex reads it from your organisation's Google Workspace (a *directory
sync*). Through such a provider:

- only people it provisioned can sign in; anyone else is refused
  (`login_not_allowed`), even with an invitation. With a directory sync,
  that is the members of the Google groups your operator chose;
- your first sign-in is matched to the account it made, by your
  provider's id or, over SCIM, your verified email address;
- your name and email come from the provider, and if the provider
  deactivates you, you can't sign in until it reactivates you. With a
  directory sync, being suspended in Google or taken out of the groups
  counts as deactivated. An administrator may also disable you in
  Glidex; then only an administrator can enable you again.

A directory sync reads Google every 15 minutes unless the operator chose
otherwise, so after you are added to a group (or come back), it can take
that long before you can sign in.

In a cluster, open a server's web UI. Opening an agent's sends you to a
server's, at that agent's Host page; sign in there.

The first time, your browser may warn that the certificate is not
trusted: the host made its own. Compare the SHA-256 fingerprint the
browser shows with the one your operator gives you before you accept it
(see [TLS](../operator-guide/tls.md)). If the host is on a Tailscale
tailnet and your operator turned on its Tailscale certificate, open the
UI at its `https://<node>.<tailnet>.ts.net:5173` address instead: the
browser trusts that one, with no warning (see
[Tailscale certificates](../operator-guide/tls.md#tailscale-certificates)).

A browser session ends after 30 minutes without activity and after 12
hours at most, unless the operator changed these. **Log out**, in the
header, ends it now. Some actions (changing policies, host networking or
the cluster) need a recent login: the UI then asks you to sign in again
(*Confirm it's you*) and retries the action. **Confirm with a passkey**
answers it too.

The header also shows who you are and the project you are working in;
see [Projects](projects.md#choosing-a-project).

### Invitations

An administrator may let you in with an invitation: a link of the form
`https://<host>:5173/invite/<id>#<secret>`. Open it in your browser. The
page says which tenant and role it is for, and which address and
provider to sign in with; **Sign in with &lt;provider&gt;** takes you
there. Sign in with that address: another account doesn't take the
invitation.

- A link works once, for one person, and expires (after 7 days unless
  the administrator chose otherwise).
- Keep it to yourself until you have used it: it works like a password.
- If the page says the invitation was already used, revoked or has
  expired, ask for a new one.

### Your account

Click your name in the header for **Your account**. Its **Sign-in
methods** card lists the ways you can sign in as yourself, for example a
local password and a Google account; each gives the same roles.

- Under *Add a way to sign in*, pick one of the site's single sign-on
  providers and sign in there; it is added to you. This needs a recent
  login. A provider that provisions its users, by SCIM or a directory
  sync, can't be added: its sign-ins belong to the accounts it manages.
- **Remove** takes one away. You can't remove your last one.

Signing in a second way without adding it first makes a second, separate
user. If a sign-in already belongs to another user, an administrator
must remove it from that user before you can add it.

### Passkeys

A passkey lets you sign in with your device's fingerprint, face or PIN,
or a security key, instead of a password. When the site has passkeys
on, **Your account** has a **Passkeys** card; it says so if your browser
can't make passkeys.

To add one:

1. Sign in another way, and open **Your account**.
2. Under **Passkeys**, type a **Name** (such as *work laptop*) and click
   **Add a passkey**. If your login isn't recent, the UI asks you to
   sign in again first.
3. Confirm in your browser's or device's prompt.

The card lists your passkeys with when each was made and last used, and
*synced* for one your device keeps in sync across your devices. It also
appears as *Passkey* under **Sign-in methods**. **Rename** changes its
name; **Remove** deletes it. You can have up to 10.

To sign in, click **Sign in with a passkey** on the login page and pick
the passkey in your browser's prompt. Closing the prompt just cancels.

- Keep another sign-in method, or a second passkey. You can't remove the
  one that would leave a single passkey as your only way in.
- If the card says passkeys made here work under this page's name only,
  a passkey you add works only at that address of the web UI. Open the
  UI at the same address to use it.
- If you lose a device, sign in another way and remove its passkey. If
  you have no way left to sign in, ask an administrator.
- `gxctl` doesn't use passkeys; use a password, single sign-on or a
  token there.

## gxctl on a Glidex host

On a host that runs the control plane, `gxctl` talks to its Unix socket
(`/run/glidex-cp/api.sock`) and the control plane identifies you by your
Unix account. There is no login and no token:

```bash
gxctl whoami
```

You need to be in the `glidex-users` group (log out and back in after
being added). Members of `glidex-admin` are administrators.

`--socket <path>` (or `GLIDEX_SOCKET`) picks another socket. On a cluster
*agent* node there is no local API: log in to a server node from there
as from any other machine (`gxctl auth login --url
https://<server>:8841 …`, which runs on the agent itself).

## gxctl from another machine

`gxctl auth login` connects to a control plane over HTTPS, checks its
certificate, gets you a token and saves the lot as a named *login
profile* in `~/.config/glidex/config.json` (mode 0600). Later commands
use the current profile without asking again.

Pick a profile name and a way to prove who you are:

```bash
# Your account's password (asked for, never on the command line)
gxctl auth login --url https://host.example.org:8841 --profile lab --pam-user alice

# Single sign-on: open the link it prints and enter the code
gxctl auth login --url https://host.example.org:8841 --profile lab --oidc

# An API token you already have (typed hidden, or piped)
gxctl auth login --url https://host.example.org:8841 --profile ci --token < token.txt
```

- `--connector <id>` picks one of several sign-in providers. A Google
  provider works with `--oidc` only if the operator set it up for
  device logins; otherwise it answers `device_flow_unsupported`.
- `--name <name>` names the token it creates (default `gxctl@<profile>`),
  so you can tell your machines apart in `gxctl token list`.
- `--days N` asks for a shorter or longer `--pam-user` token, within the
  site's limit (30 days unless the operator changed it). An `--oidc` login
  gets a token valid for one day.
- For a cluster, repeat `--url` with several server nodes; `gxctl` tries
  them in turn.
- Without `--profile`, `--url` refreshes the profile that already points
  at that host; for a new host add `--save` to name the profile after it:
  a host name's first label (`gpu-dev2.example.net` gives `gpu-dev2`),
  or a whole IP address with dashes (`192.0.2.10` gives `192-0-2-10`).

The first profile you create becomes the current one; `--use` makes a
later one current. Run the same `auth login --profile lab` again when its
token expires.

### Certificates and fingerprint pinning

`gxctl` verifies the control plane's certificate. A certificate from a
CA your machine trusts just works. A self-signed one (the default) is
not trusted, and `auth login` shows its fingerprint:

```text
the certificate of host.example.org is not trusted by this system:
  SHA-256 AB:CD:…   (the control plane prints this line at startup)
  reported by the server as its own: it matches what we are being handed (…)
Trust this exact certificate for profile 'lab'? [y/N]:
```

Compare the fingerprint with the one your operator gave you through
another channel, then answer `y`. The profile *pins* that certificate:
from then on `gxctl` accepts exactly it for this profile, and refuses to
connect if it changes. If the line says the two do not match, stop and
ask the operator: something between you and the host is presenting its
own certificate.

Without a terminal (CI), there is no prompt; give the fingerprint up
front:

```bash
gxctl auth login --url https://host.example.org:8841 --profile ci \
  --pin sha256/<fingerprint> --token < token.txt
```

When the operator legitimately replaces the certificate (for example
after rotating a cluster's CA), confirm the new fingerprint with them and
re-pin:

```bash
gxctl auth login --profile lab --pin sha256/<new fingerprint> --rebind
```

`--rebind` is also what lets a profile log in to a machine that now
answers for a different cluster than the one it first saw; without it
`gxctl` refuses.

`gxctl auth trust list` shows a profile's pins and extra CAs.
<!-- TODO(verify): `auth trust fetch` and `auth trust remove <pin>|--all` are documented in gxctl's usage text, but auth_trust reads its subcommand from the wrong argument index, so only `list` works today. Add them here once fixed. -->

Instead of a profile, a one-off `--url` connection trusts a self-signed
certificate through `GLIDEX_CA_CERT`, the path of a PEM copy of the
host's certificate that your operator provides.

### Managing profiles

```bash
gxctl auth status             # the profile in use: target, cluster, trust, token
gxctl auth profiles --all     # every profile; * marks the current one
gxctl auth use lab            # make 'lab' current
gxctl --profile ci list       # use another profile for one command
gxctl auth logout             # forget this profile's token (target and pins stay)
gxctl auth logout --revoke    # revoke it on the server first, then forget it
gxctl auth profiles remove lab
```

`auth profiles remove <name>` deletes a profile from the file. It refuses
one that still holds a token, since the token stays valid on the server:
run `gxctl auth logout --profile <name> --revoke` first, or add
`--forget-token` to drop it unrevoked (revoke it later from **Tokens** or
`gxctl token revoke`). Removing the current profile leaves none current;
pick another with `gxctl --profile <other> auth use <other>`.

`GLIDEX_PROFILE` does what `--profile` does. `--url` on a command
overrides the target for that run but keeps the profile's trust; the
profile's token is only sent if the URL points at one of the profile's
own hosts.

Once the profile file holds any profile, `gxctl` uses profiles instead of
the local socket. To reach the local socket on a host where you also
keep profiles, run it with the file switched off:

```bash
GLIDEX_CONFIG= gxctl whoami
```

`GLIDEX_CONFIG=<path>` reads profiles from another file.
<!-- TODO(verify): after `gxctl auth use -` (clear the current profile) every gxctl invocation fails with "no current profile; choose one with --profile", including `auth use`; recover with `gxctl --profile <name> auth use <name>`. Check whether this is intended before documenting `auth use -`. -->

### Older single-token login

`gxctl --url https://host.example.org:8841 login --oidc` (or
`login --token`) still works when you have no profiles: it saves one
token in `~/.config/glidex/token` and `gxctl --url …` sends it.
`logout [--revoke]` removes it. Profiles are the better choice: they
remember the target and pin its certificate.

`GLIDEX_TOKEN`, when set, is sent instead of any saved token.

## API tokens

An API token lets a script, CI job or another tool act through the API.
There are two kinds:

- **Personal**: acts as you, and can never do more than you can. Give it
  roles to narrow it further (only `viewer` in project `lab`, say); without
  roles it can do everything you can.
- **Service account**: belongs to a project, not a person, so it keeps
  working when people leave. It can only do what its roles in that
  project allow; without roles it can do nothing. Creating one needs the
  project's `owner` role.

Tokens expire: after 90 days unless you ask otherwise, at most 365
(the operator sets both). The secret is shown once, when you create
the token; Glidex keeps only a hash of it.

### Web UI

**Tokens** in the header (shown when you are signed in with an account,
not with a token):

1. **+ Create Token**.
2. Enter a **Name** and **Expires in (days)**. For a service account,
   choose **Kind**: *Service account of &lt;project&gt;* (offered if you own
   the selected project).
3. **+ Add role** for each role, and pick its project, or leave none.
4. **Create Token**, then **Copy** the token and store it in a secret
   manager or a file only you can read. It is not shown again.

The table lists your tokens with their roles, expiry and when they were
last used; **Revoke** ends one immediately.

### gxctl

```bash
gxctl token create laptop --days 30                                 # personal, all your rights
gxctl token create readonly --role viewer@lab                       # personal, narrowed
gxctl token create ci --service-account --project lab --role editor # service account
gxctl token list
gxctl token revoke <token-id>
```

`--role ROLE[@PROJECT]` repeats; roles are `viewer`, `operator`,
`editor`, `owner` (see [Roles](projects.md#roles)). A service account's
roles apply in its own project; without `--project` it goes into the
project selected with `--project` or your default project.

### Using a token

```bash
export GLIDEX_TOKEN=<token>
gxctl --url https://host.example.org:8841 whoami

curl --cacert host.crt -H "Authorization: Bearer $GLIDEX_TOKEN" \
  https://host.example.org:8841/auth/whoami
```

Or store it in a profile: `gxctl auth login --profile ci --token`. The
REST calls are `GET /tokens`, `POST /tokens` and `DELETE /tokens/{id}`
([REST API](../api/rest.md)).

If a token leaks, revoke it at once and create a new one.

## Who am I?

`gxctl whoami` (or `gxctl auth whoami`) shows how the control plane sees
you:

```text
  User:     alice (<user-id>)
  Method:   token via https://host.example.org:8841
  Token:    gxctl@lab (<token-id>)
  Teams:    -
  Host roles: -
  Default project: lab (<project-id>)
  Project roles:
    lab                  role.editor    via user:<user-id>
```

`Method` is `peer` on the local socket and `token` whenever a token is
sent (as `gxctl` does from a profile); a web UI session is `pam`, `oidc`
or `passkey`. The user id is what others need to add
you to a project. Over the API: `GET /auth/whoami`.

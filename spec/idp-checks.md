# Real-IdP checks (spec/tenancy.md §17, T2 and T3)

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

## Results

| Date | Build | Check | Result |
|---|---|---|---|

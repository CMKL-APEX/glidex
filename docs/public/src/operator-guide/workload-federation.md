# Workload federation

A **workload federation** lets CI jobs and other workloads call Glidex
without a stored secret. The workload gets a signed token (a JWT) from
its own platform, such as GitHub Actions, GitLab CI or a Kubernetes
cluster, and exchanges it at `POST /auth/exchange` for a short-lived
Glidex token. That token acts as one of the tenant's
[service accounts](authentication.md#access-tokens).

A federation belongs to one tenant. It names:

- the **issuer** it trusts, and the **audience** the workload asks for;
- **rules**, in order: the exact claims a token must carry, and the
  service account a matching token acts as.

There is nothing to rotate on the CI side: no Glidex token is kept in
the CI system's secrets.

## Who manages them

| Action | Who |
|---|---|
| `readTenantFederations` | `tenant-auditor`, `tenant-admin` of the tenant; `directory-auditor`, `directory-admin` |
| `manageTenantFederations` | `tenant-admin` of the tenant, `directory-admin`; with a recent login (see [Sessions and re-authentication](authentication.md#sessions-and-re-authentication)) |

Trusting an issuer gives its jobs a service account's rights in the
tenant: treat it like handing out that service account's token.

In the web UI, a tenant's page has a **Workload federation** card (for
those who may read the tenant's federations): it lists them, and tenant
administrators can **Add federation**, **Enable** or **Disable** one,
**Delete** it, and copy a ready-made exchange snippet. `gxctl` and the
[API](../api/rest.md#tenants-and-login-connectors) do the same.

## Setting one up

1. Make a service account in the project the jobs work in, with only
   the role they need (see [Access tokens](authentication.md#access-tokens)).
   The rule names it by its id, from `gxctl token list`. Its secret
   isn't needed: don't store it in the CI system.

   ```bash
   gxctl token create ci-deploy --days 365 --service-account --project infra --role operator
   gxctl token list
   ```

2. Add the federation, with one rule:

   ```bash
   gxctl federation add acme github \
     --issuer https://token.actions.githubusercontent.com \
     --audience glidex \
     --name "GitHub Actions" \
     --match repository=acme/infra --match ref=refs/heads/main \
     --service-account <sa-token-id> --ttl 900
   ```

   Glidex reads the issuer at once; if it can't, the federation is
   refused (`422 issuer_unreachable`).

3. Check it:

   ```bash
   gxctl federation list acme
   gxctl federation show acme github
   ```

   Each rule shows its claims, the service account (by name and
   project, when you can list it) and its token lifetime.

Several rules go in a file, a JSON list, with `--rules-file` instead of
`--match`:

```json
[
  {"match": {"repository": "acme/infra", "ref": "refs/heads/main"},
   "service_account": "<sa-token-id>", "ttl_secs": 900},
  {"match": {"sub": "repo:acme/infra:environment:production"},
   "service_account": "<other-sa-token-id>", "ttl_secs": 600}
]
```

```bash
gxctl federation add acme github --issuer https://token.actions.githubusercontent.com \
  --audience glidex --rules-file rules.json
```

Enable, disable or remove it:

```bash
gxctl federation disable acme github
gxctl federation enable acme github
gxctl federation remove acme github
```

`gxctl` can't change the rules of an existing federation. Use the API's
`PATCH /tenants/{id}/federations/{federation}` with
`{"changes": {"rules": [...]}}` (the list replaces all the rules), or
remove the federation and add it again.

## The federation

| Field | What it is |
|---|---|
| `id` | `a-z`, `0-9` and `-`, 1–48 characters; unique in the tenant. Can't be changed |
| `display_name` | Optional, at most 128 characters |
| `issuer` | The issuer's URL exactly as its tokens carry it in `iss`: `https://…`. A trailing `/` is dropped |
| `audience` | What the workload asks its issuer for, and what the token's `aud` must hold (GitHub's `audience`, GitLab's `id_tokens` `aud`). At most 256 characters |
| `enabled` | `true` by default. A disabled federation exchanges nothing, and the tokens it gave stop working until it is enabled again |
| `rules` | In order; the first that matches maps the token. At most 50 |
| `kind` | `jwt`, the only kind |
| `tenant_id`, `created_by`, `created_at`, `updated_at` | Set by Glidex |

A rule:

| Field | What it is |
|---|---|
| `match` | Claim name to exact value. Every claim must be in the token, as a string, with that value. It must name at least one of `repository`, `project_path` or `sub`. No wildcards: `*` is refused. Names are letters, digits, `_`, `-`, `.`, `:`, at most 64 characters; values at most 256 |
| `service_account` | A service account's token **id** (never its secret), in one of the tenant's projects (`422 invalid_federation` otherwise) |
| `ttl_secs` | How long the Glidex token lives: 60–3600 seconds, 900 by default, and never past the service account's own expiry |

A tenant has at most 20 federations.

### The issuer

When a federation is added, or its issuer changed, Glidex reads the
issuer's `<issuer>/.well-known/openid-configuration` and the keys at its
`jwks_uri`:

- the document's `issuer` must be the federation's, and `jwks_uri` must
  be `https`;
- redirects aren't followed, and each read has 10 seconds;
- the keys are kept for an hour, and read again sooner when a token is
  signed with a key they don't have (at most once a minute).

So the control plane needs outbound HTTPS to the issuer.
Common issuers:

| Platform | `issuer` | Claims to pin |
|---|---|---|
| GitHub Actions | `https://token.actions.githubusercontent.com` | `repository`, with `ref` or `environment`; or `sub` (`repo:<owner>/<repo>:ref:refs/heads/main`, `repo:<owner>/<repo>:environment:<name>`) |
| GitLab CI (gitlab.com) | `https://gitlab.com` | `project_path`, with `ref` and `ref_type` |
| Kubernetes | The cluster's service account issuer, reachable from Glidex | `sub` (`system:serviceaccount:<namespace>:<name>`) |

<!-- TODO(verify): only the GitHub issuer and a mock issuer are exercised in tests; confirm GitLab and Kubernetes end to end, including whether GitLab ID tokens (which may live as long as the job's timeout) stay within the one-hour limit. -->

## The exchange

The workload posts its token to `POST /auth/exchange`, without any other
credential, as a form or JSON:

| Field | Value |
|---|---|
| `grant_type` | `urn:ietf:params:oauth:grant-type:token-exchange` |
| `subject_token_type` | `urn:ietf:params:oauth:token-type:jwt` |
| `subject_token` | The workload's JWT |
| `federation` | `<tenant slug>/<federation id>` |

The answer (`200`, with `Cache-Control: no-store`):

```json
{
  "access_token": "<token>",
  "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
  "token_type": "Bearer",
  "expires_in": 900
}
```

Send `access_token` as `Authorization: Bearer <token>`, like any API
token; `gxctl` takes it from `GLIDEX_TOKEN`.

The workload's token is accepted when:

- it is signed with one of the issuer's keys, with RS256 or ES256;
- its `iss` is the federation's issuer and its `aud` holds the
  federation's audience;
- it is current (`exp`, `nbf`, `iat`, with 60 seconds of clock skew),
  and valid for at most an hour from `iat` to `exp`;
- a rule matches, and its service account still exists, is the
  tenant's and hasn't expired;
- it wasn't exchanged before: each workload token is exchanged once
  (by its `jti`, or the whole token when it has none). Get a new one
  from the issuer for each exchange.

Errors are OAuth's, not Glidex errors:

| Status | `error` | Meaning |
|---|---|---|
| 400 | `invalid_request` | The request is malformed: a wrong `grant_type` or `subject_token_type`, no token (or one over 16 KiB), `federation` not `<tenant>/<id>` |
| 400 | `invalid_grant` | The token can't be exchanged. Every refusal answers the same, whatever the reason; the reason is in the [audit log](#audit) |
| 429 | `slow_down` | Over 30 exchanges a minute for this federation; retry later |

### From GitHub Actions

The job needs `permissions: id-token: write`. It asks GitHub for a
token with the federation's audience, exchanges it, and masks the
result:

```yaml
permissions:
  id-token: write
  contents: read

steps:
  - name: Glidex token
    run: |
      jwt=$(curl -sS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
        "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=glidex" | jq -r .value)
      token=$(curl -sS --fail-with-body --cacert tls.crt \
        https://host.example.org:8841/auth/exchange \
        --data-urlencode grant_type=urn:ietf:params:oauth:grant-type:token-exchange \
        --data-urlencode subject_token_type=urn:ietf:params:oauth:token-type:jwt \
        --data-urlencode "subject_token=$jwt" \
        --data-urlencode federation=acme/github | jq -r .access_token)
      echo "::add-mask::$token"
      echo "GLIDEX_TOKEN=$token" >> "$GITHUB_ENV"
  - run: gxctl --url https://host.example.org:8841 --project infra list
    env:
      GLIDEX_CA_CERT: tls.crt
      GLIDEX_CONFIG: ""
```

The runner must reach the API, and trust its certificate (`tls.crt`
here, see [TLS](tls.md)).

### From any OIDC issuer

The same request as JSON:

```bash
curl --fail-with-body --cacert tls.crt -X POST https://host.example.org:8841/auth/exchange \
  -H 'Content-Type: application/json' -d '{
  "grant_type": "urn:ietf:params:oauth:grant-type:token-exchange",
  "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
  "subject_token": "<oidc-token>",
  "federation": "acme/github"
}'
```

## The token it gives

- It acts as the rule's service account: the same roles, in the same
  project, and nothing a service account couldn't do (it can't make
  tokens, for one).
- It lives the rule's `ttl_secs`, and never past the service account's
  expiry. It can't be renewed: exchange a new workload token.
- It isn't listed by `gxctl token list` or `/tokens`, and can't be
  revoked on its own. It stops working when:
  - it expires;
  - the federation is disabled (until it is enabled again) or removed;
  - its rule is removed, or the rule's claims or service account
    change (a new `ttl_secs` or order doesn't end it);
  - the service account is revoked or expires.

To cut off a federation's jobs at once, disable it; to end their tokens
for good, remove the rule or the federation, or revoke the service
account.

## Audit

- Adding, changing and removing a federation are recorded as
  `manageTenantFederations`, with target `federation:<tenant>/<id>`; an
  added one also records its issuer and rules.
- Every exchange, granted or not, is recorded as `exchangeToken` by
  `system:federation:<tenant>/<id>`. A granted one names the service
  account (`token:<id>`), the federated token's id, its lifetime, and
  the token's `sub` with `repository`, `ref`, `environment`,
  `project_path` and `workflow` when present. A refused one has the
  reason. The workload's JWT is never recorded.
- Requests made with the Glidex token are recorded as the service
  account's token, with the federated token's id.

These entries have no project: `auditor` and `system-admin` read them
(see [Audit log](access-control.md#audit-log)).

```bash
gxctl audit --limit 50
```

## Keeping it safe

- **Pin narrowly.** Match the repository or project and the branch or
  environment that deploys, not a whole organization. A token from any
  other repository, branch or fork is then refused. On GitHub, pin
  `environment` (or a `sub` with it) to use the environment's
  protection rules.
- **Least privilege.** One service account per pipeline, with the
  smallest project role it needs (`operator` to start and stop,
  `editor` to create); avoid `owner`, which manages the project's
  bindings.
- **Short lifetimes.** Keep `ttl_secs` to the job's length; the default
  is 15 minutes.
- **A dedicated audience.** Use an audience only for this Glidex site,
  so tokens meant for other services can't be exchanged here.
- **Keep the token out of logs.** Mask it in the CI system, and don't
  echo it or pass it on the command line.
- **Review.** `gxctl federation list <tenant>` and the `exchangeToken`
  entries in the audit log; remove federations and rules you no longer
  use.

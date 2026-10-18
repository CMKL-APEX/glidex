# TLS certificates

The control plane API (port 8841) and the web UI (port 5173) serve HTTPS
on every address of the host. Each uses a self-signed certificate unless
you give it your own. Local `gxctl` users on the Unix socket don't use
TLS at all.

## Self-signed certificates

On first start, the control plane and the web UI each generate a
certificate (ECDSA P-256, valid for five years) for this host's names
and addresses: its fully qualified name, its host name, `localhost`, the
loopback addresses and the addresses of its interfaces at that moment.

| Service | Certificate | Key |
|---|---|---|
| Control plane | `/var/lib/glidex-control-plane/.glidex/tls/cp.crt` | `cp.key` next to it, mode 0600 |
| Web UI | `/var/lib/glidex-ui/tls/ui.crt` | `ui.key` next to it, mode 0600 |

A certificate is kept as long as it loads and is valid for more than 30
more days, so its fingerprint stays the same for the clients that trust
it. It is regenerated when it is missing, unreadable or close to expiry.

### Fingerprints

Browsers warn about a self-signed certificate. Before you accept it,
compare the SHA-256 fingerprint the browser shows with the one the
service printed:

- the installer prints both at the end of every run;
- the control plane logs it at every start (`SHA-256:` line):

  ```bash
  journalctl -u glidex-control-plane -b | grep SHA-256
  journalctl -u glidex-ui -b | grep fingerprint
  ```

- or compute it from the certificate:

  ```bash
  openssl x509 -in /run/glidex-cp/tls.crt -noout -fingerprint -sha256
  sudo openssl x509 -in /var/lib/glidex-ui/tls/ui.crt -noout -fingerprint -sha256
  ```

`/run/glidex-cp/tls.crt` is a public copy of the API's certificate (never
the key) that the control plane writes at every start, for local
clients to trust.

### New names or addresses

A self-signed certificate only covers the names and addresses the host
had when it was made. After you rename the host or give it a new address
clients will use, generate new ones:

```bash
sudo systemctl stop glidex-ui glidex-control-plane
sudo rm /var/lib/glidex-control-plane/.glidex/tls/cp.{crt,key} \
        /var/lib/glidex-ui/tls/ui.{crt,key}
sudo systemctl start glidex-control-plane glidex-ui
```

The fingerprints change: tell your users, and update `gxctl` profiles
that pinned the old one (see [gxctl](#gxctl)). Running VMs are not
affected.

## Your own certificate

Use a certificate from your CA for anything users reach over the
network. Put the files somewhere outside `/home`, for example
`/etc/glidex/tls/`; the certificate must be readable by the service's
user, the key only by root:

```bash
sudo install -d -m 0755 /etc/glidex/tls
sudo install -m 0644 glidex.crt /etc/glidex/tls/glidex.crt   # chain, leaf first
sudo install -m 0600 glidex.key /etc/glidex/tls/glidex.key
```

The certificate must name what users type: the host's DNS name (and its
address, if they use it).

### API (control plane)

In `/etc/glidex/control-plane.json`:

```json
{ "tls": { "cert": "/etc/glidex/tls/glidex.crt" } }
```

Give the key as the `tls-key` credential:

```bash
sudo systemctl edit glidex-control-plane
```

```ini
[Service]
LoadCredential=tls-key:/etc/glidex/tls/glidex.key
```

```bash
sudo systemctl restart glidex-control-plane
```

`tls.key` can name the key file instead, but then it must be readable by
the `glidex` user; the credential keeps it root-only.

### Web UI

```bash
sudo systemctl edit glidex-ui
```

```ini
[Service]
Environment=GLIDEX_UI_TLS_CERT=/etc/glidex/tls/glidex.crt
LoadCredential=ui-tls-key:/etc/glidex/tls/glidex.key
```

```bash
sudo systemctl restart glidex-ui
```

With a configured certificate the UI also sends
`Strict-Transport-Security`.

If users reach the UI under a name that isn't one of the host's own,
also set `GLIDEX_UI_HOSTS` and `auth.allowed_origins` (see
[Configuration](configuration.md#web-ui)).

### Renewing

Both services check their certificate and key files once a minute and
serve a replaced pair without a restart. Replace each file in one step
(write a new file, then `mv` it over the old one), the key and the
certificate in either order: until the two match again, the service
keeps serving the previous pair and logs a warning. The control plane
also refreshes its public copy, `/run/glidex-cp/tls.crt`.

A key given as a systemd credential (`LoadCredential=`) is copied when
the service starts, so a renewal that changes the key needs a restart;
running VMs keep running:

```bash
sudo systemctl restart glidex-control-plane glidex-ui
```

`journalctl -u glidex-ui` (or `-u glidex-control-plane`) shows
`TLS certificate reloaded` with the new fingerprint.

## Plain HTTP

`"tls": "off"` (API) and `GLIDEX_UI_TLS=off` (UI) serve plain HTTP, and
are refused unless every listener is a loopback address:

```json
{ "tls": "off", "listen": ["127.0.0.1:8841", "[::1]:8841"] }
```

Use this only behind something on the same host that terminates TLS.
On its HTTPS port the web UI answers a plain-HTTP request with a
redirect to `https://`.

## Client trust

### Browsers

With a CA-issued certificate, browsers need nothing. With the
self-signed one, users compare the fingerprint once and accept it.

### gxctl

On the host itself, `gxctl` uses the Unix socket and needs no
certificate. Over TCP to a loopback address it trusts the published
`/run/glidex-cp/tls.crt` automatically.

From another machine, `gxctl` trusts the system's CA store. For a
self-signed certificate, either:

- copy the host's `/run/glidex-cp/tls.crt` (it is public) and point
  `GLIDEX_CA_CERT` at it:

  ```bash
  export GLIDEX_CA_CERT=~/glidex-lab.crt
  gxctl --url https://glidex.example.org:8841 login --oidc
  ```

- or let a login profile pin the certificate. `gxctl auth login` shows
  the fingerprint of a certificate it doesn't trust and asks whether to
  trust exactly that one; compare it with the one the control plane
  printed. Non-interactively, pass the fingerprint you checked:

  ```bash
  gxctl auth login --url https://glidex.example.org:8841 --profile lab \
    --pin sha256/<fingerprint> --oidc
  ```

  A pin trusts that one certificate; dates and the host name are still
  checked. When the certificate changes, `gxctl` refuses to connect
  rather than ask again. Once you have confirmed the new fingerprint
  through another channel:

  ```bash
  gxctl auth login --profile lab --pin sha256/<new-fingerprint> --rebind
  ```

  `gxctl auth trust list` shows a profile's pins and CA, `gxctl auth
  trust fetch` compares the fingerprint the server reports with the
  certificate it presents.

Prefer a CA-issued certificate, or a pin, over any option that turns
verification off. See the [gxctl reference](../reference/gxctl.md) for
login profiles.

### API clients

Give clients the certificate to trust, for example with `curl`:

```bash
curl --cacert glidex-lab.crt -H "Authorization: Bearer $GLIDEX_TOKEN" \
  https://glidex.example.org:8841/auth/whoami
```

### Clusters

Nodes of a cluster talk to each other on port 8842 with mutual TLS under
the cluster's own CA, which Glidex manages. That is separate from the
certificates on this page; see [Clusters](clusters.md).

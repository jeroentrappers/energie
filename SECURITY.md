# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through
[GitHub's private reporting](https://github.com/jeroentrappers/energie/security/advisories/new),
not in a public issue. This is a personal project maintained on a best-effort basis, but
reports are taken seriously, especially anything that allows unauthorised control of devices.

Security fixes go into the latest release only.

## Security model

energie is designed for a trusted home network. It controls physical equipment, so keep it off
the public internet.

### Dashboard and API

- **energie refuses to start if `listen` can be reached from another machine with neither
  `[oidc]` nor `web_password_file` set.** A password must be at least 12 characters.
- Every route needs a sign-in, except `/healthz`, which returns only a status.
- With `[oidc]`, browsers use the authorization code flow with PKCE, `state` bound to the browser
  and a `nonce`. Sessions are random, server-side, in `HttpOnly`, `SameSite=Lax` cookies, and
  cross-origin writes are refused. An allow-list is mandatory, and the provider must use HTTPS.
- Bearer tokens are verified against the provider's keys: asymmetric signatures only, with issuer,
  audience and expiry checked, and the same allow-list. An ID token can't be used as one.
- Signing out or a session expiring doesn't cut a live-update stream that is already open. It
  ends when the page reconnects. Provider sign-out isn't performed, so the provider's own session
  stays active.
- On a loopback address no password is required, so anything running on that machine, or any
  reverse proxy in front of it, can reach the API. A proxy must do its own authentication.
- energie does not serve TLS, and Basic authentication sends the password in a reversible form.
  For access beyond a trusted LAN, use a VPN, or a reverse proxy that provides TLS.
- New device credentials are accepted only on a separate port, `cli_listen` (default: the
  `listen` port + 1), which always binds to loopback and is used by the `energie credentials`
  command. Anything running on the machine can reach it. Don't forward it through a reverse
  proxy.
- There are no roles: anyone signed in has full control.

### Secrets

- Secrets are only read from files, never from the config, and are redacted from logs.
- Under systemd, use `LoadCredential` with root-owned `0600` files.
- Cloud tokens that rotate are stored in the state directory with mode `0600`.
- On Windows, file permissions are not set by energie. Protect those files yourself.

### Local protocols

Modbus TCP, plain MQTT and the Home Assistant WebSocket link are unencrypted, and Modbus has no
authentication at all. Keep these devices on a network you trust, and ideally a separate VLAN.

### MQTT

- energie refuses to start if the bridge, or a charger on MQTT, uses a broker on the network
  without a `username` and `password_file`.
- OIDC applies to the dashboard and API only, not to MQTT, which uses the broker's own logins.
- The bridge only publishes by default. It accepts settings over `/set` only with
  `allow_set = true`.
- **What other clients may read or write is decided by your broker, and energie cannot enforce
  it.** Set `allow_anonymous false` and use an access list.
  [`deploy/mosquitto-energie.acl`](deploy/mosquitto-energie.acl) is a starting point. Any client
  allowed to write `energie/+/+/set` can change settings, and any client allowed to write
  `SmartEVSE/#` can control that charger.

### Service hardening

The supplied systemd unit runs energie as an unprivileged dynamic user, with a read-only system,
no capabilities, a system call filter and memory limits. Prefer it to running as root.

### Supply chain

Dependencies are checked for known advisories and license compatibility in CI. Release binaries
are built by GitHub Actions and published with checksums and build-provenance attestations.

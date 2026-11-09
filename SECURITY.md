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

- **energie refuses to start if `listen` can be reached from another machine and
  `web_password_file` is not set.** The password must be at least 12 characters.
- With a password, every route uses HTTP Basic authentication, except `/healthz`, which returns
  only a status.
- On a loopback address no password is required, so anything running on that machine, or any
  reverse proxy in front of it, can reach the API. A proxy must do its own authentication.
- energie does not serve TLS, and Basic authentication sends the password in a reversible form.
  For access beyond a trusted LAN, use a VPN, or a reverse proxy that provides TLS.
- `POST /api/cloud/<id>/credentials` and `GET /api/cloud` also accept connections from the
  machine itself without the password, for the `energie credentials` command. **If you run a
  reverse proxy on the same machine, its requests look local, so block `/api/cloud` at the
  proxy.**
- There is no user or role model: the password gives full control.

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

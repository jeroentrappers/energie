# Running energie with Docker Compose

The image contains only the static binary, runs as an unprivileged user and works with a
read-only filesystem. It's published for `linux/amd64`, `linux/arm64`, `linux/arm/v7` and
`linux/riscv64` at `ghcr.io/jeroentrappers/energie`.

## Try it

This starts a simulated house, with no hardware needed. From this directory:

```sh
mkdir -p secrets
openssl rand -base64 30 > secrets/web-password
chmod 400 secrets/web-password
sudo chown 65532:65532 secrets/web-password   # the user inside the container
docker compose up -d
docker compose ps                             # wait for "healthy"
```

Open <http://localhost:8080> and sign in with any user name and the password from
`sudo cat secrets/web-password`.

## Use your real devices

1. Replace `config/energie.toml`, starting from
   [`energie.standalone.example.toml`](../energie.standalone.example.toml). Keep
   `listen = "0.0.0.0:8080"`, `[control] dry_run = true` for now, and read the
   [configuration reference](../../docs/CONFIGURATION.md).
2. Put each secret in `secrets/`, owned by `65532` with mode `400`, add it to `compose.yaml`, and
   refer to it in the config as `/run/secrets/<name>`.
3. Check the config, then restart:

   ```sh
   docker compose run --rm energie --check-config /etc/energie/energie.toml
   docker compose up -d --force-recreate
   docker compose logs -f
   ```

4. Follow [From install to live control](../../docs/INSTALL.md#from-install-to-live-control) before
   removing `dry_run`.

The default bridge network can reach devices on your LAN, so `network_mode: host` isn't needed.

## Sign-in

A dashboard that's reachable from the network needs a sign-in, or energie won't start. To use an
OpenID Connect provider instead of a fixed password, add an
[`[oidc]` section](../../docs/CONFIGURATION.md#oidc). OIDC needs HTTPS, so put a TLS reverse proxy
in front. Keeping `web_password_file` gives you a break-glass login at `/auth/basic` if the provider
is down.

If a reverse proxy runs in front, publish the port to it only, for example `127.0.0.1:8080:8080`,
and read the notes on proxies in [`SECURITY.md`](../../SECURITY.md).

## Day to day

| Task | Command |
| --- | --- |
| Upgrade | `docker compose pull && docker compose up -d` |
| Pin a version | `ENERGIE_TAG=0.2.0 docker compose up -d` |
| Build from this checkout | `docker compose up -d --build` |
| Logs | `docker compose logs -f` |
| Stop | `docker compose down` (keeps your settings) |
| Back up settings and history | `docker run --rm -v docker_energie-state:/s:ro -v "$PWD":/b alpine tar czf /b/energie-state.tar.gz -C /s .` |

The volume is named after this directory, so check `docker volume ls` if yours differs. The
backup can include cloud tokens, so store it accordingly.

To control writes, keep the [kill switch](../../docs/OPERATIONS.md#the-kill-switch) in reach, and
know [what happens when energie stops](../../docs/OPERATIONS.md#what-happens-when-energie-stops):
`docker compose down` stops control too.

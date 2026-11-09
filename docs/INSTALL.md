# Installation

Choose one install method, then follow [From install to live control](#from-install-to-live-control).

| Method | Best for |
| --- | --- |
| [Install script](#install-script-linux) | Most Linux servers and single-board computers |
| [Manual binary](#manual-binary) | Linux without the script, macOS, Windows |
| [Docker](#docker) | Hosts that already run containers |
| [From source](#from-source) | Development, or platforms without a release binary |

## Requirements

- A machine that stays on and is on the same network as your devices. A Raspberry Pi or
  similar is plenty: energie typically uses about 18 MB of memory.
- A correct system clock (NTP), because schedules and quarter-hours depend on it.
- For control mode on Solis over Modbus TCP and Tuya: energie must be the **only** client of
  those devices. Disable other integrations that talk to them first.

## Install script (Linux)

```sh
curl -fsSL https://raw.githubusercontent.com/jeroentrappers/energie/main/scripts/install.sh -o install.sh
less install.sh
sudo sh install.sh
```

The script:

1. Picks the static binary for your CPU and downloads it from the latest release.
2. Verifies it against the release's `SHA256SUMS` and stops on a mismatch.
3. Installs `/usr/local/bin/energie` and the systemd unit, and creates `/etc/energie`.
4. Copies a sample config only if you don't have one. It never overwrites a config.
5. Leaves the service **disabled and stopped**.

To pin a version, run `sudo ENERGIE_VERSION=v0.1.0 sh install.sh`.

## Manual binary

1. Download `energie-<version>-<target>.tar.gz` (`.zip` on Windows) and `SHA256SUMS` from the
   [releases page](https://github.com/jeroentrappers/energie/releases).

   | Device | Target |
   | --- | --- |
   | Linux PC or server | `x86_64-unknown-linux-musl` |
   | Raspberry Pi 3/4/5 with a 64-bit OS, other ARM64 boards | `aarch64-unknown-linux-musl` |
   | Raspberry Pi 2/3 with a 32-bit OS | `armv7-unknown-linux-musleabihf` |
   | RISC-V 64 boards | `riscv64gc-unknown-linux-musl` |
   | Mac with Apple Silicon | `aarch64-apple-darwin` |
   | Intel Mac | `x86_64-apple-darwin` |
   | Windows PC | `x86_64-pc-windows-msvc` |
   | Windows on ARM | `aarch64-pc-windows-msvc` |

2. Check it: `sha256sum --check --ignore-missing SHA256SUMS`. On macOS use
   `shasum -a 256 --check --ignore-missing SHA256SUMS`.

   Optionally verify where it was built, using the GitHub CLI:
   `gh attestation verify energie-<version>-<target>.tar.gz --repo jeroentrappers/energie`.

3. Install on Linux:

   ```sh
   tar xzf energie-*.tar.gz
   cd energie-*/
   sudo install -m 0755 energie /usr/local/bin/
   sudo install -d -m 0755 /etc/energie
   sudo install -m 0644 deploy/energie.service /etc/systemd/system/
   sudo install -m 0644 deploy/energie.standalone.example.toml /etc/energie/energie.toml
   sudo systemctl daemon-reload
   ```

On macOS and Windows, run `energie --config <file>` directly or use your own service manager.
On macOS, you may need `xattr -d com.apple.quarantine energie` because the binaries are not
notarised. On Windows, store secrets files somewhere only your user account can read: energie
cannot set restrictive permissions on them there.

## Docker

Multi-architecture images are published at `ghcr.io/jeroentrappers/energie`. Tags are the version
(`0.2.0`), the minor version (`0.2`) and `latest`.

[`deploy/docker`](../deploy/docker/README.md) has a Compose setup that shows a simulated house
with a few commands, and then how to move to your real devices. The image has no shell and runs as
uid `65532`, on a read-only filesystem.

## From source

```sh
git clone https://github.com/jeroentrappers/energie
cd energie
cargo test --workspace
cargo build --release -p energie
sudo install -m 0755 target/release/energie /usr/local/bin/
```

Then install the unit and config as in [Manual binary](#manual-binary).

## From install to live control

Move through these stages in order. Don't skip the first two on real hardware.

### 1. Write a configuration

Start from one of the samples and read the [configuration reference](CONFIGURATION.md):

- `deploy/energie.standalone.example.toml`: energie talks to the devices itself.
- `deploy/energie.example.toml`: reads from Home Assistant, compares, never writes.

At a minimum, set your `[site]` coordinates and your device addresses. Check the result:

```sh
energie --check-config /etc/energie/energie.toml
```

The check names any problem, and any role that no device fills.

### 2. Store secrets

Secrets are always read from files, never written in the config. Create each as root with mode
`0600` in `/etc/energie`, for example:

```sh
sudo install -m 0600 /dev/null /etc/energie/mqtt-password
sudo editor /etc/energie/mqtt-password
```

Then, in `/etc/systemd/system/energie.service`, add one line per file:

```ini
LoadCredential=mqtt-password:/etc/energie/mqtt-password
```

and refer to it in the config as `/run/credentials/energie.service/mqtt-password`. The service
runs as a throwaway user, and this is how it gets access. Run `sudo systemctl daemon-reload`
after changing the unit.

Which secrets you need depends on your devices: see [supported hardware](HARDWARE.md).

### 3. Set up sign-in

A dashboard reachable from another machine needs a sign-in, or energie won't start. On
`127.0.0.1` it's optional, but then anything on that machine can reach it.

If you run an OpenID Connect provider, use [`[oidc]`](CONFIGURATION.md#oidc) and there's no
fixed password to manage. It needs HTTPS, so put a TLS reverse proxy in front of energie.
Otherwise, generate a password:

```sh
sudo sh -c 'umask 077; openssl rand -base64 18 > /etc/energie/web-password'
```

Add `LoadCredential=web-password:/etc/energie/web-password` to the unit, and to the top of the
config, before any `[section]`:

```toml
web_password_file = "/run/credentials/energie.service/web-password"
```

Your browser will ask for a login: any user name works with this password.

### 4. Run in a mode that doesn't write

```toml
mode = "control"
[control]
dry_run = true
```

```sh
sudo systemctl enable --now energie
journalctl -u energie -f
```

Dry run reads from every device, makes every decision, and logs the exact writes to
`/var/lib/energie/control-dryrun.jsonl` without sending them. If you're coming from Home
Assistant, you can use `mode = "shadow"` first, as described in
[Home Assistant](HOME-ASSISTANT.md).

Check on the dashboard (*Systeem*) that every device is available and its readings are
plausible: especially the **sign of the battery power and grid power**. Then confirm that the
planned writes look right. [`CONTROL.md`](CONTROL.md) has a full hardware checklist.

### 5. Go live

Remove `dry_run` and restart during daylight while you can watch it:

```sh
sudo systemctl restart energie
```

For the first hours, watch *Systeem → Sturing* or `control-audit.jsonl`. Each write shows
`verified` once its effect has been read back. Know how to reach the
[kill switch](OPERATIONS.md#the-kill-switch) before you start.

Set up [notifications](CONFIGURATION.md#notifications) so that a failed write reaches your
phone.

## Upgrading

Read the [changelog](../CHANGELOG.md), then rerun the install script, or replace the binary and:

```sh
sudo cp /usr/local/bin/energie /usr/local/bin/energie.prev   # before replacing it
sudo systemctl restart energie
```

Settings and history stay in `/var/lib/energie`. To roll back, restore `energie.prev` and
restart.

## Removing

```sh
sudo systemctl disable --now energie
sudo rm /usr/local/bin/energie /etc/systemd/system/energie.service
sudo systemctl daemon-reload
sudo rm -r /etc/energie            # your config and secrets
sudo rm -r /var/lib/private/energie /var/lib/energie   # your settings and history
```

Before removing it from a live system, check that the battery has returned to self-use and
set your EV chargers to the behaviour you want. They keep their last setting.

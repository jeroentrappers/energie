#!/bin/sh
# Install or upgrade energie on Linux from a GitHub release.
#
# Verifies the download against the release's SHA256SUMS, installs the binary and the systemd
# unit, and never overwrites an existing configuration. It does not start the service.
#
#   sudo sh install.sh                          # the latest release
#   sudo ENERGIE_VERSION=v0.1.0 sh install.sh   # a specific one
set -eu

REPO="jeroentrappers/energie"
BIN="/usr/local/bin/energie"
UNIT="/etc/systemd/system/energie.service"
CONFIG_DIR="/etc/energie"

fail() { echo "error: $*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || fail "this script is for Linux. See docs/INSTALL.md for other platforms."
[ "$(id -u)" -eq 0 ] || fail "run as root, for example with sudo"
for tool in curl tar sha256sum uname mktemp install; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done

case "$(uname -m)" in
    x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
    aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
    armv7l | armv8l) target="armv7-unknown-linux-musleabihf" ;;
    riscv64) target="riscv64gc-unknown-linux-musl" ;;
    *) fail "no release binary for $(uname -m). Build from source: see docs/INSTALL.md." ;;
esac

version="${ENERGIE_VERSION:-}"
if [ -z "$version" ]; then
    latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" ||
        fail "could not find the latest release"
    version="${latest##*/}"
    case "$version" in
        v[0-9]*) ;;
        *) fail "could not find the latest release" ;;
    esac
fi

archive="energie-$version-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$version"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $archive"
curl -fsSL -o "$tmp/$archive" "$base/$archive" || fail "download failed: $base/$archive"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || fail "could not download SHA256SUMS"

echo "Verifying the checksum"
grep -q "  $archive\$" "$tmp/SHA256SUMS" || fail "$archive is not listed in SHA256SUMS"
(cd "$tmp" && grep "  $archive\$" SHA256SUMS | sha256sum -c - >/dev/null) || fail "checksum mismatch: not installing"

tar -xzf "$tmp/$archive" -C "$tmp"
src="$tmp/energie-$version-$target"
[ -x "$src/energie" ] || fail "unexpected archive layout"

if [ -e "$BIN" ]; then
    cp -p "$BIN" "$BIN.prev"
    echo "Kept the previous binary as $BIN.prev"
fi
install -m 0755 "$src/energie" "$BIN"
install -d -m 0755 "$CONFIG_DIR"

sample_note=""
if [ ! -e "$CONFIG_DIR/energie.toml" ]; then
    install -m 0640 "$src/deploy/energie.standalone.example.toml" "$CONFIG_DIR/energie.toml"
    sample_note="yes"
fi

if command -v systemctl >/dev/null 2>&1; then
    if [ -e "$UNIT" ]; then
        install -m 0644 "$src/deploy/energie.service" "$UNIT.new"
        echo "Kept your $UNIT. The unit from this release is at $UNIT.new"
    else
        install -m 0644 "$src/deploy/energie.service" "$UNIT"
        systemctl daemon-reload
    fi
else
    echo "systemd not found: the service was not installed."
fi

echo
"$BIN" --version
if [ -n "$sample_note" ]; then
    cat <<EOF

Installed. A sample config is at $CONFIG_DIR/energie.toml. Edit it before use:
it describes an example house, with dry_run on.

Next, follow "From install to live control" in docs/INSTALL.md. Begin with:
  energie --check-config $CONFIG_DIR/energie.toml
EOF
else
    echo
    echo "Upgraded. Restart when ready: sudo systemctl restart energie"
fi

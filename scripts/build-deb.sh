#!/bin/sh
# Packages the static binary as a Debian package (amd64):
# target-deb/mypaas_<version>_amd64.deb
#
#   scripts/build-static.sh && scripts/build-deb.sh [path/to/mypaas]
#
# The package installs /usr/bin/mypaas and nothing else; `mypaas server init`
# writes the configuration and the systemd units, as with a manual install.
# Without dpkg-deb on the host (Fedora, macOS), it runs itself in a Debian
# container.
set -eu

cd "$(dirname "$0")/.."
BIN="${1:-target-musl/x86_64-unknown-linux-musl/release/mypaas}"
OUT="target-deb"

if ! command -v dpkg-deb >/dev/null 2>&1; then
  exec docker run --rm -v "$PWD:/src" -w /src -u "$(id -u):$(id -g)" \
    debian:13-slim sh scripts/build-deb.sh "$BIN"
fi

[ -x "$BIN" ] || { echo "error: $BIN not found; run scripts/build-static.sh first" >&2; exit 1; }

# Cargo's 1.2.0-rc.1 sorts before 1.2.0 only as Debian's 1.2.0~rc.1.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1 | tr - '~')"
FILE_VERSION="$(echo "$VERSION" | tr '~' -)"
PKG="$OUT/root"
rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/share/doc/mypaas"

install -m 0755 "$BIN" "$PKG/usr/bin/mypaas"
install -m 0644 README.md "$PKG/usr/share/doc/mypaas/README.md"
install -m 0644 docs/security.md "$PKG/usr/share/doc/mypaas/security.md"
{
  echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
  echo "Upstream-Name: mypaas"
  echo "Source: https://github.com/hydroid7/mypaasr"
  echo
  echo "Files: *"
  echo "Copyright: 2026 Lóránt Meszlényi"
  echo "License: MIT"
  sed -n '/^Permission/,$p' LICENSE | sed 's/^$/./; s/^/ /'
} > "$PKG/usr/share/doc/mypaas/copyright"
chmod 0644 "$PKG/usr/share/doc/mypaas/copyright"

cat > "$PKG/DEBIAN/control" <<EOF
Package: mypaas
Version: $VERSION
Architecture: amd64
Maintainer: Lóránt Meszlényi <meszlenyilorant@gmail.com>
Installed-Size: $(du -sk "$PKG/usr" | cut -f1)
Recommends: docker.io | docker-ce, iptables
Section: admin
Priority: optional
Homepage: https://github.com/hydroid7/mypaasr
Description: single-server PaaS on Docker and Traefik
 Deploy Dockerfiles to your own server over one small, signed HTTPS API.
 One static binary is the client CLI, the server CLI and the daemon.
 .
 On a server, run "mypaas server init" after installing. On a laptop or in
 CI, the client needs nothing else.
EOF

cat > "$PKG/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
[ "$1" = configure ] || exit 0
if [ -x /usr/local/bin/mypaas ]; then
  echo "warning: /usr/local/bin/mypaas comes first in PATH and shadows this package's /usr/bin/mypaas; remove it" >&2
fi
if [ -f /etc/systemd/system/mypaasd.service ]; then
  if ! grep -q '^ExecStart=/usr/bin/mypaas ' /etc/systemd/system/mypaasd.service; then
    echo "note: the systemd units start another mypaas binary; run 'mypaas server init --domain <domain>' again to use /usr/bin/mypaas" >&2
  fi
  # An upgrade: the daemon and the socket proxy pick up the new binary.
  if [ -d /run/systemd/system ]; then
    systemctl try-restart mypaasd.service mypaas-socket-proxy.service || true
  fi
else
  echo "mypaas installed. On a server, continue with: mypaas server init --domain <admin domain> --key <your.pub>"
fi
EOF

cat > "$PKG/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
# On removal (not upgrade) stop what runs the binary. Traefik, the services,
# ~/_mypaas and the unit files stay, so a reinstall picks up where it left off.
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
  systemctl disable --now mypaasd.service mypaas-socket-proxy.service 2>/dev/null || true
fi
EOF
chmod 0755 "$PKG/DEBIAN/postinst" "$PKG/DEBIAN/prerm"

DEB="$OUT/mypaas_${FILE_VERSION}_amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$PKG" "$DEB" >/dev/null
rm -rf "$PKG"
dpkg-deb --info "$DEB" | sed -n 's/^ *\(Package\|Version\|Architecture\):/&/p'
ls -lh "$DEB"

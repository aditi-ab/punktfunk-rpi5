#!/usr/bin/env bash
# Build the punktfunk-web .deb — the management web console (Nitro/Node SSR + React).
#
# Architecture: all — the .output is pre-built JS (no compiled binary, so NO dpkg-shlibdeps).
# Runtime is apt-native: Depends on nodejs (>= 20). The host's punktfunk-host .deb Recommends this,
# so a default `apt install punktfunk-host` pulls the console too. It is auto-wired to the host's
# mgmt token via the systemd --user units (no env editing on a packaged install).
#
# Usage: VERSION=0.0.1~ci42.gdeadbee bash packaging/debian/build-web-deb.sh
# Output: dist/punktfunk-web_<version>_all.deb
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
PKG="punktfunk-web"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

# Build the console if not already built (.output is gitignored — CI builds it each run).
if [ ! -f web/.output/server/index.mjs ]; then
  echo "==> building web console"
  (cd web && bun install --frozen-lockfile && bun run build)
fi
# The build MUST be the node-server preset (runnable by apt-native node) — never bun.
if grep -rq 'Bun\.serve' web/.output/server/index.mjs 2>/dev/null; then
  echo "ERROR: web/.output contains Bun.serve — wrong nitro preset (need 'node-server')" >&2
  exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
SHAREDIR="$STAGE/usr/share/$PKG"
DOCDIR="$STAGE/usr/share/doc/$PKG"

# --- file layout -------------------------------------------------------------
mkdir -p "$SHAREDIR/.output"
cp -r web/.output/server "$SHAREDIR/.output/server"
cp -r web/.output/public "$SHAREDIR/.output/public"
# Stable PATH-independent ExecStart wrapper.
install -d "$STAGE/usr/bin"
cat > "$STAGE/usr/bin/punktfunk-web-server" <<'WRAP'
#!/bin/sh
exec /usr/bin/node /usr/share/punktfunk-web/.output/server/index.mjs "$@"
WRAP
chmod 0755 "$STAGE/usr/bin/punktfunk-web-server"
install -Dm0644 scripts/punktfunk-web.service      "$STAGE/usr/lib/systemd/user/punktfunk-web.service"
install -Dm0644 scripts/punktfunk-web-init.service "$STAGE/usr/lib/systemd/user/punktfunk-web-init.service"
install -Dm0755 scripts/web-init.sh                "$SHAREDIR/web-init.sh"
install -Dm0644 web/web.env.example                "$SHAREDIR/web.env.example"
install -Dm0644 LICENSE-MIT                         "$DOCDIR/LICENSE-MIT"
install -Dm0644 LICENSE-APACHE                      "$DOCDIR/LICENSE-APACHE"
install -Dm0644 web/README.md                       "$DOCDIR/README.md"

cat > "$DOCDIR/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: punktfunk
Source: https://git.unom.io/unom/punktfunk

Files: *
Copyright: punktfunk contributors
License: MIT or Apache-2.0
 Dual-licensed. Full texts in /usr/share/doc/$PKG/LICENSE-MIT and
 /usr/share/doc/$PKG/LICENSE-APACHE.
EOF
printf '%s (%s) stable; urgency=medium\n\n  * Automated build %s.\n\n -- unom <noreply@anthropic.com>  %s\n' \
  "$PKG" "$VERSION" "$VERSION" "$(date -uR 2>/dev/null || echo 'Thu, 01 Jan 1970 00:00:00 +0000')" \
  | gzip -9n > "$DOCDIR/changelog.Debian.gz"

INSTALLED_KB="$(du -k -s "$STAGE" | cut -f1)"

install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: all
Maintainer: unom <noreply@anthropic.com>
Installed-Size: $INSTALLED_KB
Section: net
Priority: optional
Homepage: https://git.unom.io/unom/punktfunk
Depends: nodejs (>= 20)
Description: punktfunk management web console (Nitro/Node SSR + React)
 The browser console for a punktfunk streaming host: status, paired devices, and the
 SPAKE2 PIN pairing flow every client needs. Runs as a systemd --user service on port
 3000, login-gated (a password generated on first start), proxying the host's loopback
 HTTPS management API with a bearer token injected server-side (never sent to the browser).
 .
 Auto-wired to the host on a packaged install: it sources the host's
 ~/.config/punktfunk/mgmt-token and a generated login password — no env editing. Enable
 the systemd user service punktfunk-web; read the login password from the --user journal.
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    echo "punktfunk-web installed. Enable it for your user:"
    echo "    systemctl --user enable --now punktfunk-web"
    echo "A login password is generated on first start — read it with:"
    echo "    journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'"
    echo "    (or: sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password)"
    echo "Then open http://<host-ip>:3000"
fi
exit 0
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_all.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
dpkg-deb -I "$OUT" | sed -n 's/^/  /p' | grep -E 'Version|Installed-Size|Depends' || true

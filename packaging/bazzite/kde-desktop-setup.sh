#!/usr/bin/env bash
# One-shot setup so the punktfunk host can stream the Bazzite KDE *Desktop* session (KWin virtual
# output at the client's resolution). Run ONCE as the streaming user (no root needed). Gaming Mode
# (gamescope) needs none of this — it auto-attaches. Idempotent: safe to re-run.
#
#   bash /usr/share/punktfunk/bazzite/kde-desktop-setup.sh
#
# Two things a normal KDE login lacks that the headless host needs:
#  1. KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 — so KWin exposes the privileged `zkde_screencast`
#     virtual-output protocol to the host (an external client) at all.
#  2. The `kde-authorized` RemoteDesktop grant — so libei input setup auto-approves instead of
#     popping an "Allow remote control?" dialog the headless host can't answer.
# After running, log out + back into the KDE Desktop session once (or reboot) so KWin restarts
# with the flag. Gaming Mode is unaffected.
set -euo pipefail

GRANT_SRC="${PUNKTFUNK_GRANT_SRC:-/usr/share/punktfunk/headless/kde-authorized}"
ENVD="$HOME/.config/environment.d/10-punktfunk-kwin.conf"
GRANT_DST="$HOME/.local/share/flatpak/db/kde-authorized"

echo "punktfunk: KDE Desktop-mode setup"

# 1. KWin permission-check bypass (persistent, picked up by the next KDE session via systemd).
mkdir -p "$(dirname "$ENVD")"
cat > "$ENVD" <<'EOF'
# punktfunk: let the streaming host bind KWin's privileged zkde_screencast (virtual output).
# A dedicated streaming box; this relaxes KWin's Wayland permission checks for the desktop path.
KWIN_WAYLAND_NO_PERMISSION_CHECKS=1
EOF
echo "  wrote $ENVD"

# 2. RemoteDesktop portal grant for headless libei input (never clobber an existing one).
if [[ -s "$GRANT_DST" ]]; then
    echo "  grant DB already present ($GRANT_DST) — leaving it"
elif [[ -s "$GRANT_SRC" ]]; then
    mkdir -p "$(dirname "$GRANT_DST")"
    cp "$GRANT_SRC" "$GRANT_DST"
    echo "  seeded RemoteDesktop grant -> $GRANT_DST"
    # Pick it up now without a relogin (the portal store caches at start).
    systemctl --user restart xdg-permission-store 2>/dev/null || true
    systemctl --user restart plasma-xdg-desktop-portal-kde xdg-desktop-portal-kde 2>/dev/null || true
    systemctl --user restart xdg-desktop-portal 2>/dev/null || true
else
    echo "  WARN: grant source not found at $GRANT_SRC — input will need a manual portal approval" >&2
fi

echo "punktfunk: done. Log out + back into the KDE Desktop session (or reboot) so KWin restarts"
echo "          with the flag, then connect a client while in Desktop Mode."

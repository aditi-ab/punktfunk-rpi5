#!/usr/bin/env bash
# Headless KDE Plasma session for the punktfunk host (no KMS scanout → kwin --virtual).
#
# Brings up the full desktop, not just the compositor. The env matters: without
# XDG_MENU_PREFIX=plasma- the launcher resolves ${XDG_MENU_PREFIX}applications.menu →
# "applications.menu", which doesn't exist on KDE installs (it ships
# plasma-applications.menu) — plasmashell runs fine but the menu shows NO applications
# and no System Settings entry. kded6/krunner/kglobalacceld are D-Bus-activated once
# plasmashell starts in the right session env.
#
#   bash scripts/headless/run-headless-kde.sh [WxH]    # default 1920x1080
#
# Then in another shell:
#   WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE PUNKTFUNK_ZEROCOPY=1 \
#     punktfunk-host m3-host --source virtual --seconds 14400
set -euo pipefail

RES="${1:-1920x1080}"
W="${RES%x*}"
H="${RES#*x}"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"
export XDG_CURRENT_DESKTOP=KDE
export XDG_MENU_PREFIX=plasma-
export KDE_FULL_SESSION=true
export KDE_SESSION_VERSION=6
export DESKTOP_SESSION=plasma
export WAYLAND_DISPLAY=wayland-kde
export KWIN_WAYLAND_NO_PERMISSION_CHECKS=1

kwin_wayland --virtual --width "$W" --height "$H" --no-lockscreen \
    --socket "$WAYLAND_DISPLAY" &
KWIN_PID=$!
sleep 2

kbuildsycoca6 >/dev/null 2>&1 || true # rebuild the menu cache under the correct env
plasmashell &

echo "headless KDE up on $WAYLAND_DISPLAY ($RES), kwin pid $KWIN_PID"
wait "$KWIN_PID"

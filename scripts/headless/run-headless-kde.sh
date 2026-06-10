#!/usr/bin/env bash
# Headless KDE Plasma session for the punktfunk host (no KMS scanout → kwin --virtual).
#
# Brings up the full desktop, not just the compositor, and waits for it to be *actually
# ready* before starting the portals/plasma — no blind `sleep`. The env matters: without
# XDG_MENU_PREFIX=plasma- the launcher resolves ${XDG_MENU_PREFIX}applications.menu →
# "applications.menu", which doesn't exist on KDE installs (it ships
# plasma-applications.menu) — plasmashell runs fine but the menu shows NO applications.
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

# The probe binary (gates readiness on KWin actually exposing zkde_screencast — not merely
# the socket existing). Use a release build if present, else fall back to `cargo run`.
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ -x "$ROOT/target/release/punktfunk-host" ]]; then
    PROBE=("$ROOT/target/release/punktfunk-host" probe-compositor)
elif [[ -x "$ROOT/target/debug/punktfunk-host" ]]; then
    PROBE=("$ROOT/target/debug/punktfunk-host" probe-compositor)
else
    PROBE=(cargo run -q --manifest-path "$ROOT/Cargo.toml" -p punktfunk-host -- probe-compositor)
fi

# kwin to its own log (so its EGL/GPU-init errors are captured, not lost to the terminal).
KWIN_LOG="${TMPDIR:-/tmp}/punktfunk-kwin.log"
kwin_wayland --virtual --width "$W" --height "$H" --no-lockscreen \
    --socket "$WAYLAND_DISPLAY" >"$KWIN_LOG" 2>&1 &
KWIN_PID=$!

# Active readiness wait: poll until KWin is up AND advertises the zkde_screencast global
# (what the virtual-output backend needs), or fail fast with a useful message. kwin can also
# exit immediately if EGL/GPU init fails — catch that.
echo "waiting for KWin ($RES) to become ready…"
DEADLINE=$(( SECONDS + 30 ))
until "${PROBE[@]}" >/dev/null 2>&1; do
    if ! kill -0 "$KWIN_PID" 2>/dev/null; then
        echo "ERROR: kwin_wayland exited during startup — see $KWIN_LOG:" >&2
        tail -n 20 "$KWIN_LOG" >&2 || true
        exit 1
    fi
    if (( SECONDS >= DEADLINE )); then
        echo "ERROR: KWin did not become ready within 30s. Last probe:" >&2
        "${PROBE[@]}" >&2 || true
        exit 1
    fi
    sleep 0.5
done
echo "KWin ready."

# Only NOW restart the portals, and against the correct env: the xdg-desktop-portal chain
# binds the compositor that existed when it started, so a stale portal points at a dead
# socket and RemoteDesktop/EIS (input injection) times out. Import the session env into the
# systemd/D-Bus activation environment FIRST (the missing piece — the Sway script does this;
# without it the restarted portal can inherit an empty WAYLAND_DISPLAY), then restart.
systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP DBUS_SESSION_BUS_ADDRESS XDG_RUNTIME_DIR 2>/dev/null || true
dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP DBUS_SESSION_BUS_ADDRESS 2>/dev/null || true
systemctl --user try-restart plasma-xdg-desktop-portal-kde.service xdg-desktop-portal-kde.service xdg-desktop-portal.service 2>/dev/null || true

kbuildsycoca6 >/dev/null 2>&1 || true # rebuild the menu cache under the correct env
plasmashell &

echo "headless KDE up on $WAYLAND_DISPLAY ($RES), kwin pid $KWIN_PID (log: $KWIN_LOG)"
wait "$KWIN_PID"

#!/usr/bin/env bash
# Run AFTER headless Sway is up (run-headless-sway.sh), from a second shell on the same
# user. It: (1) points this shell at the running Sway, (2) gives HEADLESS-1 a real refresh
# clock (an idle/0 mHz output produces no frames), (3) imports the env the ScreenCast portal
# needs to find Sway and pick the wlr backend, and (4) writes /tmp/punktfunk-sway-env.sh so
# other shells (e.g. `cargo run -p punktfunk-host`) can `source` it.
#
# Usage: bash scripts/headless/prepare-session.sh [WxH@RHz]   (default 1920x1080@60Hz)
set -euo pipefail

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
export XDG_CURRENT_DESKTOP=sway
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"
SWAYSOCK="$(ls -t "$XDG_RUNTIME_DIR"/sway-ipc.*.sock 2>/dev/null | head -1)"
export SWAYSOCK
MODE="${1:-1920x1080@60Hz}"

if ! swaymsg -t get_outputs >/dev/null 2>&1; then
    echo "Sway IPC not reachable ($SWAYSOCK) — is run-headless-sway.sh running?" >&2
    exit 1
fi

# A real refresh clock so the compositor actually drives frames (and screencopy/PipeWire
# get content). Without on-screen motion the screen is static; launch something animated
# (e.g. `swaymsg exec foot`) to exercise the encoder.
swaymsg output HEADLESS-1 mode "$MODE" >/dev/null
echo "HEADLESS-1 set to $MODE"

# The portal (xdg-desktop-portal-wlr) is D-Bus/systemd activated and must inherit these to
# find the compositor and select the wlr ScreenCast backend.
systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP SWAYSOCK XDG_RUNTIME_DIR 2>/dev/null || true
dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP SWAYSOCK 2>/dev/null || true

cat > /tmp/punktfunk-sway-env.sh <<EOF
export XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR
export WAYLAND_DISPLAY=$WAYLAND_DISPLAY
export XDG_CURRENT_DESKTOP=sway
export DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS
export SWAYSOCK=$SWAYSOCK
EOF

cat <<EOF
session ready. From any shell on this user:
    source /tmp/punktfunk-sway-env.sh
    swaymsg exec foot          # optional: animated on-screen content
    cargo run -p punktfunk-host -- m0 --source portal --seconds 5 --out /tmp/punktfunk-m0.h265
    ffprobe /tmp/punktfunk-m0.h265
EOF

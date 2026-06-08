#!/usr/bin/env bash
# Launch headless Sway on the NVIDIA VM with a private D-Bus session so the ScreenCast
# portal activates. Prints the WAYLAND_DISPLAY (default wayland-1) to use from other shells.
#
# Prereq: scripts/bootstrap-ubuntu.sh has run and nvidia-drm.modeset=Y (reboot if not).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$HERE/env.sh"
mkdir -p "$XDG_RUNTIME_DIR" 2>/dev/null || true
chmod 700 "$XDG_RUNTIME_DIR" 2>/dev/null || true

echo "starting headless Sway (renderer=$WLR_RENDERER). From another shell on this user:"
echo "    export XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR WAYLAND_DISPLAY=wayland-1"
echo "    swaymsg -t get_outputs        # expect HEADLESS-1"
echo

# --unsupported-gpu is mandatory on Sway 1.9 with the proprietary NVIDIA driver.
exec dbus-run-session -- sway --unsupported-gpu

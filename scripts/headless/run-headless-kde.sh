#!/usr/bin/env bash
# Headless KDE Plasma session for the punktfunk host (no KMS scanout → kwin --virtual).
#
# Brings up the full desktop, not just the compositor, and waits for it to be *actually
# ready* before starting the portals/plasma — no blind `sleep`. A bare session is missing
# pieces a full Plasma login would start, so this also: exports the Xwayland DISPLAY (X11 apps
# like Steam need it), starts the polkit authentication agent (Discover/PackageKit need it to
# authorize installs), and supervises plasmashell (it draws wallpaper + panels; a crash must
# not leave the desktop gone). The env matters: without XDG_MENU_PREFIX=plasma- the launcher
# resolves ${XDG_MENU_PREFIX}applications.menu → "applications.menu", which doesn't exist on KDE
# installs (it ships plasma-applications.menu) — plasmashell runs fine but the menu is EMPTY.
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
# the socket existing). Prefer a source-tree build (dev box), then a packaged install on PATH
# (the RPM/.deb put it at /usr/bin — there is no $ROOT/target there, and the old cargo-run
# fallback has no Cargo.toml either, so without this the probe never succeeds and the session
# restart-loops), then `cargo run` as a last resort.
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ -x "$ROOT/target/release/punktfunk-host" ]]; then
    PROBE=("$ROOT/target/release/punktfunk-host" probe-compositor)
elif [[ -x "$ROOT/target/debug/punktfunk-host" ]]; then
    PROBE=("$ROOT/target/debug/punktfunk-host" probe-compositor)
elif command -v punktfunk-host >/dev/null 2>&1; then
    PROBE=(punktfunk-host probe-compositor)
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

# Detect the Xwayland display KWin started and export it. Without DISPLAY, X11 apps launched in
# this session (Steam, many games/launchers) fail with "can't open display" even though Xwayland
# is running — KWin sets DISPLAY only for its own children, not for apps launched via the
# plasma menu / D-Bus activation. KWin brings Xwayland up a moment after itself; poll for it.
DISPLAY_NUM=""
for _ in $(seq 1 20); do
    # `|| true`: under `set -euo pipefail`, pgrep/grep exit non-zero when Xwayland isn't up yet
    # (common a few seconds into a systemd-launched boot), which would abort the WHOLE script —
    # killing KWin — on the first iteration instead of retrying. Tolerate it so the loop polls,
    # and so a session with no Xwayland at all still proceeds (DISPLAY just stays unset → warn).
    d=$(pgrep -a Xwayland 2>/dev/null | grep -oE ' :[0-9]+' | tr -d ' :' | head -1 || true)
    if [[ -n "$d" && -S "/tmp/.X11-unix/X$d" ]]; then DISPLAY_NUM="$d"; break; fi
    sleep 0.25
done
if [[ -n "$DISPLAY_NUM" ]]; then
    export DISPLAY=":$DISPLAY_NUM"
    echo "Xwayland on $DISPLAY"
else
    echo "WARN: no Xwayland display detected — X11 apps (Steam) won't open a display" >&2
fi

# Only NOW restart the portals, and against the correct env: the xdg-desktop-portal chain
# binds the compositor that existed when it started, so a stale portal points at a dead
# socket and RemoteDesktop/EIS (input injection) times out. Import the session env into the
# systemd/D-Bus activation environment FIRST (the missing piece — the Sway script does this;
# without it the restarted portal can inherit an empty WAYLAND_DISPLAY) — including DISPLAY so
# D-Bus-activated X11 apps inherit it — then restart.
systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP DBUS_SESSION_BUS_ADDRESS XDG_RUNTIME_DIR ${DISPLAY:+DISPLAY} 2>/dev/null || true
dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP DBUS_SESSION_BUS_ADDRESS ${DISPLAY:+DISPLAY} 2>/dev/null || true
# --no-block: queue the restart and return immediately. A synchronous try-restart of the
# portal chain blocks bring-up ~30s (xdg-desktop-portal is Type=dbus and waits for its bus
# name); the portal only needs to be ready before the FIRST client streams (seconds later,
# user-driven), not before plasmashell starts.
systemctl --user --no-block try-restart plasma-xdg-desktop-portal-kde.service xdg-desktop-portal-kde.service xdg-desktop-portal.service 2>/dev/null || true

# Polkit authentication agent: without it, Discover / PackageKit can't get authorization to
# install packages (polkitd is the policy engine; the *agent* is the GUI prompt). A full Plasma
# session starts it; our bare session must do it explicitly. Force the Qt Wayland platform — it
# exits immediately if it can't pick one. comm is truncated to 15 chars ("polkit-kde-auth").
POLKIT_AGENT=/usr/lib/x86_64-linux-gnu/libexec/polkit-kde-authentication-agent-1
if [[ -x "$POLKIT_AGENT" ]] && ! pgrep -x polkit-kde-auth >/dev/null; then
    QT_QPA_PLATFORM=wayland setsid "$POLKIT_AGENT" \
        >"${TMPDIR:-/tmp}/punktfunk-polkit-agent.log" 2>&1 &
fi

kbuildsycoca6 >/dev/null 2>&1 || true # rebuild the menu cache under the correct env

# Supervise plasmashell: it draws the desktop (wallpaper + panels). It can crash (a transient
# GPU/QML hiccup), and a single unsupervised `plasmashell &` then leaves the streamed session
# with only app windows — no desktop, no wallpaper, no launcher. Restart it for as long as KWin
# lives, so the desktop self-heals.
( while kill -0 "$KWIN_PID" 2>/dev/null; do
      plasmashell
      echo "plasmashell exited — restarting in 2s" >&2
      sleep 2
  done ) &

echo "headless KDE up on $WAYLAND_DISPLAY ($RES), kwin pid $KWIN_PID (log: $KWIN_LOG)"
wait "$KWIN_PID"

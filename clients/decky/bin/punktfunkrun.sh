#!/usr/bin/env bash
# punktfunk stream runner — the target of the hidden non-Steam shortcut the plugin creates.
#
# WHY A WRAPPER SCRIPT (load-bearing, from MoonDeck's hard-won knowledge): the stream client
# must be a descendant of the process Steam launches via `reaper`, or gamescope never gives
# its window focus/fullscreen in Gaming Mode (gamescope detects the "current app" by AppID,
# which only attaches to reaper's descendants — see gamescope#484). So the Decky plugin
# launches THIS script through SteamClient.Apps.RunGame; the script then execs the flatpak
# client, which inherits the shortcut's AppID and is focused. Launching the flatpak directly
# from the (root) Decky backend produces an unfocused, invisible window.
#
# Per-session parameters arrive as environment variables, set as the shortcut's Steam launch
# options by the plugin (SteamClient.Apps.SetAppLaunchOptions), so ONE generic shortcut serves
# every host (and every pinned game):
#   PF_HOST   host[:port] to connect to            (required for streaming; optional for browse)
#   PF_LAUNCH library id to launch on connect       (optional, e.g. steam:570 — pinned games)
#   PF_BROWSE non-empty = open the gamepad library  (optional; --browse instead of --connect)
#   PF_MGMT   management-API port for --browse       (optional; client defaults to 47990)
#   PF_CONNECT_TIMEOUT  connect budget in seconds    (optional; the plugin stretches it after
#                        firing Wake-on-LAN so the connect survives the host's resume)
#   PF_APPID  flatpak app id                        (default io.unom.Punktfunk)
#   PF_FLATPAK  override the flatpak binary path     (default: `flatpak` on PATH)
#   PF_CLIENT_BIN  absolute path of a NATIVE client  (optional; set by the plugin when it
#                  resolved a non-flatpak install — then the client is exec'd directly and
#                  PF_APPID/PF_FLATPAK are unused)
#
# Values are plain tokens (the plugin validates launch ids to space/quote-free ASCII before
# they ever reach Steam launch options). An older flatpak without --launch/--browse ignores
# the unknown flags harmlessly (hand-scanned argv): PF_LAUNCH degrades to the plain desktop
# session, PF_BROWSE to the client's hosts page.
#
# Runs as the `deck` user (Steam launched it), so the --user flatpak install is visible and
# WAYLAND_DISPLAY / XDG_RUNTIME_DIR are already correct for gamescope.
#
# NO EXEC BIT REQUIRED: the Steam shortcut's exe is `/bin/sh` and this script rides behind
# `%command%` as an argument (see src/steam.ts). Decky extracts plugin zips without preserving
# permission bits and ~/homebrew/plugins is root-owned (the unprivileged plugin backend can't
# chmod), so the launch path must never depend on +x. Keep this script POSIX-sh clean.
set -u

APPID="${PF_APPID:-io.unom.Punktfunk}"
FLATPAK="${PF_FLATPAK:-flatpak}"

# The client is not always the flatpak: a sysext, a .deb/.rpm, an AUR build or a nix profile
# installs a native `punktfunk-client`, and the plugin passes its absolute path here when that
# is what it resolved. Both kinds take the same argv and share ~/.config/punktfunk, so the only
# difference is the prefix in front of it.
#
# exec so the client IS the game process — when it exits, Steam ends the "game" and Gaming Mode
# reclaims focus automatically (no manual refocus needed).
run_client() {
    if [ -n "${PF_CLIENT_BIN:-}" ]; then
        exec "$PF_CLIENT_BIN" "$@"
    fi
    exec "$FLATPAK" run --arch=x86_64 "$APPID" "$@"
}

# What we are about to run, for the log line each branch prints.
CLIENT_LABEL="${PF_CLIENT_BIN:-$APPID}"

# --fullscreen: present the stream chrome-less and fullscreen (the client also auto-detects the
# Deck/gamescope env, and ignores the flag harmlessly on older builds that predate it).
if [ -n "${PF_BROWSE:-}" ]; then
    # The gamepad UI. BARE `--browse` (no PF_HOST) opens the console home — the self-contained
    # host picker + pairing + settings, gamepad-navigable — which is what the stateless, visible
    # library shortcut launches. `--browse <host>` opens straight into that host's library (the
    # per-host "open on screen" action). A streams a game, session end returns here, B quits.
    if [ -z "${PF_HOST:-}" ]; then
        echo "punktfunkrun: gamepad UI $CLIENT_LABEL --browse (console home)" >&2
        run_client --browse --fullscreen
    fi
    echo "punktfunkrun: library $CLIENT_LABEL --browse $PF_HOST" >&2
    if [ -n "${PF_MGMT:-}" ]; then
        run_client --browse "$PF_HOST" --mgmt "$PF_MGMT" --fullscreen
    fi
    run_client --browse "$PF_HOST" --fullscreen
fi

# Streaming modes need a host (browse above is the only host-less path).
if [ -z "${PF_HOST:-}" ]; then
    echo "punktfunkrun: PF_HOST is not set (the plugin sets it as a launch option)" >&2
    exit 2
fi
# Trailing args shared by both streaming execs. A stretched connect budget rides along when the
# plugin set one (it just fired Wake-on-LAN, so the host may still be resuming); an older flatpak
# without --connect-timeout ignores the flag harmlessly (hand-scanned argv).
set -- --fullscreen
if [ -n "${PF_CONNECT_TIMEOUT:-}" ]; then
    set -- --connect-timeout "$PF_CONNECT_TIMEOUT" "$@"
fi
if [ -n "${PF_LAUNCH:-}" ]; then
    # A pinned game: the id rides the session Hello and the host launches that title.
    echo "punktfunkrun: streaming $CLIENT_LABEL --connect $PF_HOST --launch $PF_LAUNCH" >&2
    run_client --connect "$PF_HOST" --launch "$PF_LAUNCH" "$@"
fi
echo "punktfunkrun: streaming $CLIENT_LABEL --connect $PF_HOST" >&2
run_client --connect "$PF_HOST" "$@"

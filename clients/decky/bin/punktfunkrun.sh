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
#   PF_APPID  flatpak app id                        (default io.unom.Punktfunk)
#   PF_FLATPAK  override the flatpak binary path     (default: `flatpak` on PATH)
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

# exec so the flatpak client IS the game process — when it exits, Steam ends the "game" and
# Gaming Mode reclaims focus automatically (no manual refocus needed).
# --fullscreen: present the stream chrome-less and fullscreen (the client also auto-detects the
# Deck/gamescope env, and ignores the flag harmlessly on older builds that predate it).
if [ -n "${PF_BROWSE:-}" ]; then
    # The gamepad UI. BARE `--browse` (no PF_HOST) opens the console home — the self-contained
    # host picker + pairing + settings, gamepad-navigable — which is what the stateless, visible
    # library shortcut launches. `--browse <host>` opens straight into that host's library (the
    # per-host "open on screen" action). A streams a game, session end returns here, B quits.
    if [ -z "${PF_HOST:-}" ]; then
        echo "punktfunkrun: gamepad UI $APPID --browse (console home)" >&2
        exec "$FLATPAK" run --arch=x86_64 "$APPID" --browse --fullscreen
    fi
    echo "punktfunkrun: library $APPID --browse $PF_HOST" >&2
    if [ -n "${PF_MGMT:-}" ]; then
        exec "$FLATPAK" run --arch=x86_64 "$APPID" --browse "$PF_HOST" --mgmt "$PF_MGMT" --fullscreen
    fi
    exec "$FLATPAK" run --arch=x86_64 "$APPID" --browse "$PF_HOST" --fullscreen
fi

# Streaming modes need a host (browse above is the only host-less path).
if [ -z "${PF_HOST:-}" ]; then
    echo "punktfunkrun: PF_HOST is not set (the plugin sets it as a launch option)" >&2
    exit 2
fi
if [ -n "${PF_LAUNCH:-}" ]; then
    # A pinned game: the id rides the session Hello and the host launches that title.
    echo "punktfunkrun: streaming $APPID --connect $PF_HOST --launch $PF_LAUNCH" >&2
    exec "$FLATPAK" run --arch=x86_64 "$APPID" --connect "$PF_HOST" --launch "$PF_LAUNCH" --fullscreen
fi
echo "punktfunkrun: streaming $APPID --connect $PF_HOST" >&2
exec "$FLATPAK" run --arch=x86_64 "$APPID" --connect "$PF_HOST" --fullscreen

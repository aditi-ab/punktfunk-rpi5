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
# every host:
#   PF_HOST   host[:port] to connect to            (required)
#   PF_APPID  flatpak app id                        (default io.unom.Punktfunk)
#   PF_FLATPAK  override the flatpak binary path     (default: `flatpak` on PATH)
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

if [ -z "${PF_HOST:-}" ]; then
    echo "punktfunkrun: PF_HOST is not set (the plugin sets it as a launch option)" >&2
    exit 2
fi

echo "punktfunkrun: streaming $APPID --connect $PF_HOST" >&2
# exec so the flatpak client IS the game process — when it exits, Steam ends the "game" and
# Gaming Mode reclaims focus automatically (no manual refocus needed).
# --fullscreen: present the stream chrome-less and fullscreen (the client also auto-detects the
# Deck/gamescope env, and ignores the flag harmlessly on older builds that predate it).
exec "$FLATPAK" run --arch=x86_64 "$APPID" --connect "$PF_HOST" --fullscreen

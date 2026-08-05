#!/bin/sh
# First-run setup for the punktfunk web console (run by punktfunk-web-init.service as the user):
# generate the login password once, in the streaming user's config dir, and surface it to the
# journal. The mgmt token is NOT created here — the host owns it (~/.config/punktfunk/mgmt-token).
set -eu

DIR="${XDG_CONFIG_HOME:-$HOME/.config}/punktfunk"
mkdir -p "$DIR"
chmod 700 "$DIR" 2>/dev/null || true
PWFILE="$DIR/web-password"

if [ ! -s "$PWFILE" ]; then
    # URL/shell-safe password (no /+= so it's a clean EnvironmentFile value).
    PW=$(head -c 18 /dev/urandom | base64 | tr -d '/+=' | cut -c1-20)
    (umask 077; printf 'PUNKTFUNK_UI_PASSWORD=%s\n' "$PW" > "$PWFILE")
    chmod 600 "$PWFILE" 2>/dev/null || true
    # Do NOT echo the password itself. Anything this script prints is captured by systemd into
    # the PERSISTENT journal, which on Debian/Ubuntu is readable by the `adm` and
    # `systemd-journal` groups — so printing it published a 0600 secret to every member of them,
    # permanently, and the .deb postinst then documented `journalctl` as the way to read it
    # (2026-08-05 review L-18). Point at the file instead: it is the same one command, it is
    # correctly 0600, and it stays readable only by the user who owns the console.
    echo "punktfunk web console login password generated."
    echo "Read it with:  cut -d= -f2- $PWFILE"
    echo "(then open https://<host-ip>:47992 and log in)"
fi

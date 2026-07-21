#!/usr/bin/env bash
# punktfunk — Steam Deck HOST update: rebuild from the current source + restart the services.
# Run on the Deck after pulling/rsyncing new source. Pairings, config, and the web login persist.
#
#   bash scripts/steamdeck/update.sh           # rebuild host (+web if installed) and restart
#   bash scripts/steamdeck/update.sh --pull    # `git pull` first (if the source is a git checkout)
#
set -euo pipefail
log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

SRC="${PUNKTFUNK_SRC:-$HOME/punktfunk}"
BOX="${PUNKTFUNK_BOX:-pf2}"
TARGET_DIR="$SRC/target-steamos"
[ -d "$SRC/crates/punktfunk-host" ] || die "no punktfunk source at $SRC (set PUNKTFUNK_SRC)"
WEB=0; [ -f "$HOME/.config/systemd/user/punktfunk-web.service" ] && WEB=1

if [ "${1:-}" = "--pull" ]; then
    if [ -d "$SRC/.git" ]; then log "git pull"; git -C "$SRC" pull --ff-only; ok "pulled"; else die "$SRC is not a git checkout — rsync new source then run without --pull"; fi
fi

log "Rebuilding host (release)"
# vulkan-encode matches the packaged builds (deb/arch) — see install.sh.
distrobox enter "$BOX" -- bash -lc "set -e; export PATH=\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR='$TARGET_DIR'; cd '$SRC' && cargo build -r -p punktfunk-host --features punktfunk-host/vulkan-encode"
ok "host rebuilt"
if [ "$WEB" = 1 ]; then
    log "Rebuilding web console"
    distrobox enter "$BOX" -- bash -lc "set -e; export PATH=\$HOME/.bun/bin:\$PATH; cd '$SRC/web' && bun install --frozen-lockfile && bun run build"
    ok "web rebuilt"
fi

# Retrofit config that install.sh now writes but older installs predate (both idempotent):
# RADV_PERFTEST — Van Gogh RADV still gates VK_KHR_video_encode_* behind it; without it the
# Vulkan backend can't open and sessions silently fall back to libav VAAPI. The KWin .desktop —
# KWin only grants the restricted capture/input globals to the exe a .desktop authorizes.
HOST_ENV="$HOME/.config/punktfunk/host.env"
if [ -f "$HOST_ENV" ] && ! grep -q '^RADV_PERFTEST=' "$HOST_ENV"; then
    printf '\n# Van Gogh RADV gates VK_KHR_video_encode_* behind this (Vulkan Video encode).\nRADV_PERFTEST=video_encode\n' >> "$HOST_ENV"
    ok "host.env: added RADV_PERFTEST=video_encode"
fi
mkdir -p "$HOME/.local/share/applications"
sed "s|^Exec=.*|Exec=$TARGET_DIR/release/punktfunk-host|" "$SRC/packaging/linux/io.unom.Punktfunk.Host.desktop" \
    > "$HOME/.local/share/applications/io.unom.Punktfunk.Host.desktop"
ok "KWin desktop-capture authorization refreshed"

log "Restarting services"
systemctl --user restart punktfunk-host.service
ok "punktfunk-host restarted"
if [ "$WEB" = 1 ]; then systemctl --user restart punktfunk-web.service; ok "punktfunk-web restarted"; fi
echo
log "Updated. Status: systemctl --user status punktfunk-host"

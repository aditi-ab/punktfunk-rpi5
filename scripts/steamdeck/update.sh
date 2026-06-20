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
distrobox enter "$BOX" -- bash -lc "set -e; export PATH=\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR='$TARGET_DIR'; cd '$SRC' && cargo build -r -p punktfunk-host"
ok "host rebuilt"
if [ "$WEB" = 1 ]; then
    log "Rebuilding web console"
    distrobox enter "$BOX" -- bash -lc "set -e; export PATH=\$HOME/.bun/bin:\$PATH; cd '$SRC/web' && bun install --frozen-lockfile && bun run build"
    ok "web rebuilt"
fi

log "Restarting services"
systemctl --user restart punktfunk-host.service
ok "punktfunk-host restarted"
if [ "$WEB" = 1 ]; then systemctl --user restart punktfunk-web.service; ok "punktfunk-web restarted"; fi
echo
log "Updated. Status: systemctl --user status punktfunk-host"

#!/usr/bin/env bash
# Deploy the staged plugin tree (out/<name>, produced by scripts/package.sh) into a Steam
# Deck's ~/homebrew/plugins/, then restart Decky Loader.
#
# Decky's plugins dir is ROOT-OWNED (PluginLoader runs as root and manages it), so the install
# step needs sudo on the Deck. We rsync to a deck-writable /tmp first (no privilege), then
# sudo-copy into place. Out-of-band installs only appear after a loader restart.
#
# Usage:
#   DECK=deck@192.168.1.235 bash scripts/deploy.sh             # interactive sudo (prompts on the Deck)
#   DECK=deck@192.168.1.235 DECKPASS=... bash scripts/deploy.sh # non-interactive (scripted/CI)
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DECK="${DECK:?set DECK=deck@<ip>}"
NAME="$(python3 -c 'import json;print(json.load(open("'"$HERE"'/plugin.json"))["name"])')"
STAGE_LOCAL="$HERE/out/$NAME"
[ -d "$STAGE_LOCAL" ] || { echo "$STAGE_LOCAL missing — run scripts/package.sh first" >&2; exit 1; }

# 1. push to a deck-writable temp dir (deck owns its $HOME)
rsync -azp --delete -e ssh "$STAGE_LOCAL/" "$DECK:/tmp/$NAME/"

# 2. sudo-install into the root-owned plugins dir, match Decky's root:root ownership, reload
INSTALL="rm -rf /home/deck/homebrew/plugins/$NAME \
  && cp -r /tmp/$NAME /home/deck/homebrew/plugins/$NAME \
  && chown -R root:root /home/deck/homebrew/plugins/$NAME \
  && rm -rf /tmp/$NAME \
  && systemctl restart plugin_loader"

if [ -n "${DECKPASS:-}" ]; then
  ssh "$DECK" "echo '$DECKPASS' | sudo -S sh -c '$INSTALL'"
else
  echo "==> sudo on the Deck will prompt for ${DECK}'s password:"
  ssh -t "$DECK" "sudo sh -c '$INSTALL'"
fi
echo "deployed $NAME → $DECK:~/homebrew/plugins/$NAME and restarted plugin_loader"

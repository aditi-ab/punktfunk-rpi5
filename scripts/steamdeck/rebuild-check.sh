#!/usr/bin/env bash
# punktfunk — SteamOS post-OS-update self-heal (runs before punktfunk-host at session start).
#
# The host binary links SteamOS system libraries (FFmpeg, PipeWire, libva, …). A SteamOS A/B
# update that bumps a soname leaves the binary unable to load — a silently dead host until
# someone remembers to re-run update.sh. This probe is the reliability backstop:
#   * healthy binary  → exit in milliseconds (every normal boot);
#   * loader breakage → run scripts/steamdeck/update.sh (rebuild host + web + runner against
#     the new library tree, restart services). The build container, source, and cargo caches
#     all live under /home, which SteamOS updates never touch — so the rebuild is warm.
#
# Root is NOT needed: the /etc system tuning survives updates via the atomic-update keep list
# (see punktfunk-atomic-keep.conf); only the binary has to chase the OS libraries.
set -euo pipefail

# systemd user units get a minimal PATH — distrobox commonly lives in ~/.local/bin.
export PATH="$HOME/.local/bin:$PATH"
SRC="${PUNKTFUNK_SRC:-$HOME/punktfunk}"
BIN="$SRC/target-steamos/release/punktfunk-host"

if [ ! -x "$BIN" ]; then
    echo "punktfunk-host binary missing at $BIN — running a full rebuild" >&2
elif ! ldd "$BIN" 2>/dev/null | grep -q "not found"; then
    exit 0 # every library resolves — nothing to do
else
    echo "punktfunk-host no longer loads after a SteamOS update — its missing libraries:" >&2
    ldd "$BIN" 2>/dev/null | grep "not found" >&2 || true
    echo "rebuilding against the new OS tree (this takes a few minutes; streaming resumes after)" >&2
fi

exec bash "$SRC/scripts/steamdeck/update.sh"

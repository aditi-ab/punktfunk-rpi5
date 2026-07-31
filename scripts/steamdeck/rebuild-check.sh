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

NEED=0
if [ ! -x "$BIN" ]; then
    echo "punktfunk-host binary missing at $BIN — running a full rebuild" >&2
    NEED=1
elif ldd "$BIN" 2>/dev/null | grep -q "not found"; then
    echo "punktfunk-host no longer loads after a SteamOS update — its missing libraries:" >&2
    ldd "$BIN" 2>/dev/null | grep "not found" >&2 || true
    echo "rebuilding against the new OS tree (this takes a few minutes; streaming resumes after)" >&2
    NEED=1
fi

# The HDR gamescope companion chases OS libraries the same way. Probe it only when host.env pins
# it (build-gamescope.sh wires that line only while the binary works) — a break here would not
# just lose HDR, it would break gamescope session SPAWNING via the stale absolute override, so it
# rebuilds with the same urgency as the host binary.
GS_BIN="$(sed -n 's/^PUNKTFUNK_GAMESCOPE_BIN=//p' "$HOME/.config/punktfunk/host.env" 2>/dev/null | head -1)"
if [ -n "$GS_BIN" ]; then
    if [ ! -x "$GS_BIN" ] || ldd "$GS_BIN" 2>/dev/null | grep -q "not found"; then
        echo "punktfunk-gamescope no longer loads after a SteamOS update — rebuilding it" >&2
        NEED=1
    fi
fi

[ "$NEED" = 0 ] && exit 0 # everything resolves — nothing to do (every normal boot)

exec bash "$SRC/scripts/steamdeck/update.sh"

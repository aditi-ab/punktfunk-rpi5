#!/usr/bin/env bash
# Derive every client's UI icons from the Lucide masters in assets/lucide.
#
# The third sibling of gen-os-icons.sh and gen-launcher-icons.sh, and the same discipline —
# one master per icon, original viewBox, provenance in the README — but the vocabulary is
# different again: these are UI marks (back, refresh, microphone, gamepad), not brand marks,
# so they are keyed by Lucide's own icon names and every client draws them in a theme colour
# rather than a fixed brand one.
#
# Two clients need a derivative, for opposite reasons:
#
#   Rust clients   one shared path table  -> crates/pf-client-core/src/lucide.rs
#   Windows shell  PNG, 128 px, x2 colour -> clients/windows/assets/lucide{,-on}/
#
# The Skia console and the GTK shell both stroke the SHARED path data at Lucide's own width,
# so a mark cannot come out differently on the two. The WinUI shell cannot: windows-reactor
# has no vector element, and it builds its `BitmapIcon` with `ShowAsMonochrome(false)`, so a
# WinUI icon can be neither drawn from a path nor tinted at runtime. Hence a raster, baked
# twice — mid-grey for ordinary surfaces, white for accent buttons and the ring's dark discs.
#
# Idempotent. Usage: bash scripts/gen-lucide-assets.sh
set -euo pipefail

cd "$(dirname "$0")/.."

MASTERS=assets/lucide
TABLE=crates/pf-client-core/src/lucide.rs
WIN=clients/windows/assets/lucide
WIN_ON=clients/windows/assets/lucide-on

# The same mid-grey as the OS and launcher marks, for the same reason: the Windows shell has
# no theme-aware tint, so one colour has to stay legible on both the light and dark WinUI
# theme. White is its partner for the two surfaces that are dark whatever the theme — an
# accent-filled button, and the ring's discs.
WIN_GREY='#8A8F98'
WIN_WHITE='#FFFFFF'
# 128 px covers a 24 DIP icon at 400 % scaling with room to spare, and a 2 px stroke rasters
# to a handful of kB — the whole set is smaller than one launcher mark.
WIN_SIZE=128

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

command -v rsvg-convert >/dev/null 2>&1 || {
  echo "rsvg-convert not found (brew install librsvg / apt install librsvg2-bin)" >&2
  exit 1
}

mkdir -p "$WIN" "$WIN_ON"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

log "Shared path table (Skia console + GTK shell)"
python3 scripts/gen-lucide-icons.py "$MASTERS" "$TABLE"

log "Windows PNGs (${WIN_SIZE} px, grey + white)"
for src in "$MASTERS"/*.svg; do
  t=$(basename "$src" .svg)
  sed "s/currentColor/$WIN_GREY/" "$src" > "$tmp/$t.grey.svg"
  sed "s/currentColor/$WIN_WHITE/" "$src" > "$tmp/$t.white.svg"
  rsvg-convert -w "$WIN_SIZE" -h "$WIN_SIZE" -f png -o "$WIN/$t.png" "$tmp/$t.grey.svg"
  rsvg-convert -w "$WIN_SIZE" -h "$WIN_SIZE" -f png -o "$WIN_ON/$t.png" "$tmp/$t.white.svg"
done
log "  $(ls "$MASTERS"/*.svg | wc -l | tr -d ' ') icons x 2 colours"

# The generated table goes through rustfmt: `cargo fmt --all --check` is a CI gate, and a
# GENERATED file that fails it would fail the build every time someone re-ran this script.
if command -v rustfmt >/dev/null 2>&1; then
  rustfmt --edition 2021 "$TABLE"
  log "  rustfmt'd $TABLE"
else
  log "  rustfmt not found — run 'cargo fmt' before committing"
fi

echo
log "A NEW icon also has to be added to the Windows shell's shipped-token list —"
log "  clients/windows/src/app/lucide.rs"
log "  (the console's icons.rs and the GTK shell read the table above, so both pick it up)"

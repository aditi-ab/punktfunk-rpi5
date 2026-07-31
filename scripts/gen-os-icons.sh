#!/usr/bin/env bash
# Derive the per-client host-OS-icon assets from the assets/os-icons masters.
#
# The masters are monochrome `fill="currentColor"` SVGs, one per icon token of the host's
# OS-identity chain (see assets/os-icons/README.md). Three clients need a baked derivative
# because they cannot consume the master directly:
#
#   GTK shell     symbolic SVG, black fill  -> clients/linux/data/icons/scalable/actions/
#   Windows shell PNG, h=32, mid-grey       -> clients/windows/assets/os/
#   Apple clients vector PDF, black fill    -> clients/apple/.../OsIcons.xcassets/
#
# The web console, the Decky plugin and the Android client transcribe the master's path
# data inline instead — those are hand-kept, and this script prints them at the end so a
# new token can be pasted straight in.
#
# Idempotent. Usage: bash scripts/gen-os-icons.sh [token ...]   (default: every master)
set -euo pipefail

cd "$(dirname "$0")/.."

MASTERS=assets/os-icons
GTK=clients/linux/data/icons/scalable/actions
WIN=clients/windows/assets/os
APPLE=clients/apple/Sources/PunktfunkKit/Resources/OsIcons.xcassets

# The Windows shell has no vector element and no theme-aware tint, so its PNGs are baked in
# one mid-grey that stays legible on both the light and the dark WinUI theme.
WIN_GREY='#8A8F98'
WIN_HEIGHT=32

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

command -v rsvg-convert >/dev/null 2>&1 || {
  echo "rsvg-convert not found (brew install librsvg / apt install librsvg2-bin)" >&2
  exit 1
}

tokens=("$@")
if [ ${#tokens[@]} -eq 0 ]; then
  for f in "$MASTERS"/*.svg; do tokens+=("$(basename "$f" .svg)"); done
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for t in "${tokens[@]}"; do
  src="$MASTERS/$t.svg"
  [ -f "$src" ] || { echo "no master for token '$t' ($src)" >&2; exit 1; }
  log "$t"

  # GTK: the master with the fill resolved to black — Adwaita recolors a `-symbolic` icon
  # from the fill it finds, so the value only has to be a real colour, not the final one.
  sed 's/currentColor/#000000/' "$src" > "$GTK/pf-os-$t-symbolic.svg"

  # Windows: same black-to-grey substitution, rasterized at a fixed height so every mark
  # shares an optical size and keeps its own aspect ratio.
  sed "s/currentColor/$WIN_GREY/" "$src" > "$tmp/$t.grey.svg"
  rsvg-convert -h "$WIN_HEIGHT" -f png -o "$WIN/$t.png" "$tmp/$t.grey.svg"

  # Apple: a vector PDF at the master's natural size, in a template imageset — SwiftUI
  # tints it from foregroundStyle, so the baked colour is irrelevant.
  sed 's/currentColor/#000000/' "$src" > "$tmp/$t.black.svg"
  mkdir -p "$APPLE/os-$t.imageset"
  rsvg-convert -f pdf -o "$APPLE/os-$t.imageset/$t.pdf" "$tmp/$t.black.svg"
  cat > "$APPLE/os-$t.imageset/Contents.json" <<JSON
{
  "images" : [
    { "filename" : "$t.pdf", "idiom" : "universal" }
  ],
  "info" : { "author" : "xcode", "version" : 1 },
  "properties" : {
    "preserves-vector-representation" : true,
    "template-rendering-intent" : "template"
  }
}
JSON
done

echo
log "Inline path data (web/src/components/os-icon.tsx, clients/decky/src/os-icon.tsx,"
log "  clients/android/.../components/OsIcons.kt — hand-kept, paste from here)"
for t in "${tokens[@]}"; do
  python3 - "$MASTERS/$t.svg" "$t" <<'PY'
import re, sys
svg = open(sys.argv[1]).read()
box = re.search(r'viewBox="([^"]+)"', svg).group(1)
d = re.search(r'<path[^>]* d="([^"]+)"', svg).group(1)
w, h = box.split()[2:]
print(f'\n  {sys.argv[2]}: viewBox "{box}" (viewport {w} x {h})\n    {d}')
PY
done

echo
log "Remember: a NEW token also has to be added to each client's shipped-token list —"
log "  clients/linux/src/ui_hosts.rs, clients/linux/data/resources.gresource.xml,"
log "  clients/windows/src/app/os_icons.rs, clients/apple/.../PunktfunkKit/OsIcon.swift,"
log "  plus the three inline registries above."

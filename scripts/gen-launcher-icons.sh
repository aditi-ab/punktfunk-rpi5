#!/usr/bin/env bash
# Derive the per-client launcher-tile brand marks from the assets/launcher-icons masters.
#
# The sibling of gen-os-icons.sh, and deliberately a separate script rather than a flag on it:
# the two registries answer different questions (which OS is this host / which launcher does
# this tile open), are keyed by different vocabularies, and bake to different sizes. What they
# share is the discipline — monochrome `fill="currentColor"` masters, original viewBoxes, one
# file per token, provenance in the README.
#
# Four clients need a baked derivative because they cannot consume the master directly:
#
#   GTK shell      symbolic SVG, black fill  -> clients/linux/data/icons/scalable/actions/
#   Windows shell  PNG, h=128, mid-grey      -> clients/windows/assets/launchers/
#   Apple clients  vector PDF, black fill    -> clients/apple/.../LauncherIcons.xcassets/
#
# The web console, the Android client and the in-session console UI transcribe the master's
# path data inline instead — those are hand-kept, and this script prints them at the end so a
# new token can be pasted straight in.
#
# Idempotent. Usage: bash scripts/gen-launcher-icons.sh [token ...]   (default: every master)
set -euo pipefail

cd "$(dirname "$0")/.."

MASTERS=assets/launcher-icons
GTK=clients/linux/data/icons/scalable/actions
WIN=clients/windows/assets/launchers
APPLE=clients/apple/Sources/PunktfunkKit/Resources/LauncherIcons.xcassets

# Same mid-grey as the OS marks, for the same reason: the Windows shell has no vector element
# and no theme-aware tint, so one colour has to stay legible on both the light and dark WinUI
# theme. Taller than the OS marks (32) because this one fills a poster tile, not a status row.
WIN_GREY='#8A8F98'
WIN_HEIGHT=128

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

command -v rsvg-convert >/dev/null 2>&1 || {
  echo "rsvg-convert not found (brew install librsvg / apt install librsvg2-bin)" >&2
  exit 1
}

tokens=("$@")
if [ ${#tokens[@]} -eq 0 ]; then
  for f in "$MASTERS"/*.svg; do tokens+=("$(basename "$f" .svg)"); done
fi

mkdir -p "$GTK" "$WIN" "$APPLE"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for t in "${tokens[@]}"; do
  src="$MASTERS/$t.svg"
  [ -f "$src" ] || { echo "no master for token '$t' ($src)" >&2; exit 1; }
  log "$t"

  # GTK: the master with the fill resolved to black — Adwaita recolors a `-symbolic` icon
  # from the fill it finds, so the value only has to be a real colour, not the final one.
  sed 's/currentColor/#000000/' "$src" > "$GTK/pf-launcher-$t-symbolic.svg"

  # Windows: black-to-grey substitution, rasterized at a fixed height so every mark shares an
  # optical size and keeps its own aspect ratio.
  sed "s/currentColor/$WIN_GREY/" "$src" > "$tmp/$t.grey.svg"
  rsvg-convert -h "$WIN_HEIGHT" -f png -o "$WIN/$t.png" "$tmp/$t.grey.svg"

  # Apple: a vector PDF at the master's natural size, in a template imageset — SwiftUI tints
  # it from foregroundStyle, so the baked colour is irrelevant.
  sed 's/currentColor/#000000/' "$src" > "$tmp/$t.black.svg"
  mkdir -p "$APPLE/launcher-$t.imageset"
  rsvg-convert -f pdf -o "$APPLE/launcher-$t.imageset/$t.pdf" "$tmp/$t.black.svg"
  cat > "$APPLE/launcher-$t.imageset/Contents.json" <<JSON
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
log "Inline registries (web console, Android, in-session console UI)"
# Generated outright rather than printed for pasting, unlike gen-os-icons.sh: three clients x
# seven paths of up to 3 kB is a transcription error waiting to happen, and a mangled character
# is a silently wrong logo rather than a build failure.
python3 scripts/gen_launcher_icon_tables.py

# The Rust registry goes through rustfmt: `cargo fmt --all --check` is a CI gate, and a
# GENERATED file that fails it would fail the build every time someone re-ran this script.
if command -v rustfmt >/dev/null 2>&1; then
  rustfmt --edition 2021 crates/pf-console-ui/src/launcher_icons.rs
  log "  rustfmt'd crates/pf-console-ui/src/launcher_icons.rs"
else
  log "  rustfmt not found — run 'cargo fmt' before committing"
fi

echo
log "Remember: a NEW token also has to be added to each client's shipped-token list —"
log "  clients/linux/src/ui_library.rs, clients/linux/data/resources.gresource.xml,"
log "  clients/windows/src/app/launcher_icons.rs,"
log "  clients/apple/.../PunktfunkKit/LauncherIcon.swift"
log "  (the three inline registries above pick it up automatically)"
log "  — and to the plugin that emits the tile."

#!/usr/bin/env bash
# App Store screenshot driver for the Punktfunk Apple client.
#
# Launches the app in "shot mode" (PUNKTFUNK_SHOT_SCENE=<name> → one mock-populated screen,
# full-bleed; see Sources/PunktfunkClient/Screenshots/) once per scene per device, and lets the OS
# capture the REAL rendered UI:
#   • macOS  → `screencapture` of the app's borderless window.
#   • iOS/iPadOS/tvOS → a booted Simulator + `xcrun simctl io booted screenshot` (native pixels =
#                       the exact App Store size for that device).
#
# The captured pixels are exactly App Store Connect's required sizes:
#   mac        2880×1800   (a 1× display yields 1440×900 — also accepted)
#   iphone-6.9 1320×2868   (portrait)  /  2868×1320 (the landscape hero)
#   ipad-13    2064×2752   (portrait)  /  2752×2064 (the landscape hero)
#   appletv    1920×1080
#
# Requirements:
#   • macOS target: just the Swift toolchain (`swift build`) + a one-time Screen Recording grant
#     for your terminal (System Settings → Privacy & Security → Screen Recording).
#   • iOS/iPadOS/tvOS targets: full Xcode (xcodebuild + Simulators), not just Command Line Tools.
#
# Usage:
#   tools/screenshots.sh all            # every platform this machine can build
#   tools/screenshots.sh macos          # just macOS
#   tools/screenshots.sh ios ipad tvos  # specific platforms
#   OUT=~/Desktop/shots tools/screenshots.sh all
#   PUNKTFUNK_SHOT_HERO=~/frame.png tools/screenshots.sh ios   # real captured frame for the hero
#
# Keep SCENES in sync with ShotScenes.all.

set -euo pipefail

APPLE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$APPLE_DIR"

OUT="${OUT:-$APPLE_DIR/screenshots}"
BUNDLE_ID="io.unom.punktfunk"
SCENES=(01-stream 02-hosts 03-pair 04-trust 05-settings)
SETTLE="${SETTLE:-4}" # seconds to let a scene lay out before capturing

mkdir -p "$OUT"

log()  { printf '\033[1;36m[shots]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[shots]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[shots]\033[0m %s\n' "$*" >&2; exit 1; }

require_xcode() {
  xcrun --find simctl >/dev/null 2>&1 \
    || die "Full Xcode required for simulator capture (have Command Line Tools only).
       Install Xcode, then: sudo xcode-select -s /Applications/Xcode.app"
}

# ---------------------------------------------------------------------------- macOS

shoot_macos() {
  log "macOS — building (swift build -c release)…"
  swift build -c release >/dev/null
  local bin=".build/release/PunktfunkClient"
  [ -x "$bin" ] || die "build produced no $bin"

  for scene in "${SCENES[@]}"; do
    local logf; logf="$(mktemp)"
    PUNKTFUNK_SHOT_SCENE="$scene" "$bin" >"$logf" 2>&1 &
    local pid=$!
    # Wait for the window to exist and the scene to settle.
    local win=""
    for _ in $(seq 1 50); do
      win="$(grep -o 'PF_SHOT_WINDOW=[0-9]*' "$logf" | head -1 | cut -d= -f2 || true)"
      [ -n "$win" ] && grep -q PF_SHOT_READY "$logf" && break
      sleep 0.2
    done
    if [ -z "$win" ]; then
      kill -9 "$pid" 2>/dev/null || true
      warn "macOS/$scene: app never reported a window — skipping"; cat "$logf" >&2; continue
    fi
    local dest="$OUT/mac-$scene.png"
    if screencapture -x -o -l"$win" "$dest" 2>/dev/null && [ -s "$dest" ]; then
      log "macOS/$scene → $dest ($(pixels "$dest"))"
    else
      warn "macOS/$scene: screencapture failed — grant your terminal Screen Recording permission
       (System Settings → Privacy & Security → Screen Recording), then re-run."
    fi
    kill -9 "$pid" 2>/dev/null || true
    rm -f "$logf"
  done
}

# ------------------------------------------------------------------ iOS / iPadOS / tvOS

# $1 device-name regex  $2 scheme  $3 sdk  $4 file prefix  $5 runtime-grep
shoot_sim() {
  require_xcode
  local match="$1" scheme="$2" sdk="$3" prefix="$4" runtime="$5"

  local udid
  udid="$(xcrun simctl list devices available | grep -E "$match" | grep -oE '[0-9A-F-]{36}' | head -1 || true)"
  [ -n "$udid" ] || die "$prefix: no available Simulator matching /$match/.
       Create one in Xcode → Settings → Components, or: xcrun simctl create …"
  log "$prefix — Simulator $udid"
  xcrun simctl boot "$udid" 2>/dev/null || true
  xcrun simctl bootstatus "$udid" -b >/dev/null 2>&1 || true

  log "$prefix — building ($scheme)…"
  local dd; dd="$(mktemp -d)"
  xcodebuild -project Punktfunk.xcodeproj -scheme "$scheme" -configuration Debug \
    -sdk "$sdk" -destination "id=$udid" -derivedDataPath "$dd" \
    CODE_SIGNING_ALLOWED=NO build >/dev/null \
    || die "$prefix: xcodebuild failed"
  local app; app="$(find "$dd/Build/Products" -maxdepth 2 -name '*.app' -type d | head -1)"
  [ -n "$app" ] || die "$prefix: no .app built"
  xcrun simctl install "$udid" "$app"

  for scene in "${SCENES[@]}"; do
    xcrun simctl terminate "$udid" "$BUNDLE_ID" 2>/dev/null || true
    SIMCTL_CHILD_PUNKTFUNK_SHOT_SCENE="$scene" \
      ${PUNKTFUNK_SHOT_HERO:+SIMCTL_CHILD_PUNKTFUNK_SHOT_HERO="$PUNKTFUNK_SHOT_HERO"} \
      xcrun simctl launch "$udid" "$BUNDLE_ID" >/dev/null
    sleep "$SETTLE"
    local dest="$OUT/$prefix-$scene.png"
    xcrun simctl io "$udid" screenshot "$dest" >/dev/null
    log "$prefix/$scene → $dest ($(pixels "$dest"))"
  done
  xcrun simctl terminate "$udid" "$BUNDLE_ID" 2>/dev/null || true
  rm -rf "$dd"
}

pixels() { sips -g pixelWidth -g pixelHeight "$1" 2>/dev/null | awk '/pixel/{print $2}' | paste -sd× -; }

# ---------------------------------------------------------------------------- dispatch

[ $# -gt 0 ] || set -- all
for target in "$@"; do
  case "$target" in
    macos) shoot_macos ;;
    ios)   shoot_sim 'iPhone 16 Pro Max'   Punktfunk-iOS  iphonesimulator  iphone-6.9 iOS ;;
    ipad)  shoot_sim 'iPad Pro 13|iPad Pro .*M4|iPad Pro \(13' Punktfunk-iOS iphonesimulator ipad-13 iOS ;;
    tvos)  shoot_sim 'Apple TV'            Punktfunk-tvOS appletvsimulator appletv    tvOS ;;
    all)
      shoot_macos
      if xcrun --find simctl >/dev/null 2>&1; then
        shoot_sim 'iPhone 16 Pro Max' Punktfunk-iOS iphonesimulator iphone-6.9 iOS
        shoot_sim 'iPad Pro 13|iPad Pro .*M4|iPad Pro \(13' Punktfunk-iOS iphonesimulator ipad-13 iOS
        shoot_sim 'Apple TV' Punktfunk-tvOS appletvsimulator appletv tvOS
      else
        warn "Skipping iOS/iPadOS/tvOS — full Xcode not found (Command Line Tools only)."
      fi
      ;;
    *) die "unknown target '$target' (use: all macos ios ipad tvos)" ;;
  esac
done

log "Done. Screenshots in $OUT"
ls -1 "$OUT" 2>/dev/null || true

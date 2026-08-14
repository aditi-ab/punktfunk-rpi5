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
#   ipad-13    2064×2752   (portrait)
#   appletv    1920×1080
#
# A `.landscape` scene rotates on iPhone but NOT on iPad: an iPad app that supports multitasking
# is resizable, and iPadOS ignores `requestGeometryUpdate` orientation requests for it — the app
# follows the device, and simctl cannot rotate a simulated device. The iPad set is therefore
# portrait throughout (a valid App Store size, and uniform, which the gallery prefers). To get a
# landscape iPad hero, rotate the Simulator by hand (⌘←) and re-run just that scene.
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

# The App Store set, in listing order — the first three are what most people ever see, so they are
# the stream itself, the machines it found, and the couch/controller mode. Everything else in
# ShotScenes.all is a dev scene; capture those with `SCENES="06-gamepad-home 10-edithost" ...`.
SCENES=(${SCENES:-01-stream 02-hosts 11-library 06-gamepad-home 09e-waking-modal 05-settings 03-pair})
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

# $1 device-type regex (matches both existing device names and the device-type catalog)
# $2 scheme  $3 sdk  $4 file prefix  $5 runtime platform (iOS|tvOS — for the create fallback)
# $6 name for a device we have to create — MUST satisfy $1 (see below)
shoot_sim() {
  require_xcode
  local match="$1" scheme="$2" sdk="$3" prefix="$4" platform="$5" createname="$6"

  # Reuse an existing device of this type; else create one against the newest available runtime
  # for the platform. CI runners commonly ship a runtime but not every device (the iPhone 16 Pro
  # Max is absent on ours), so create-on-demand is what makes it reproducible.
  #
  # The created device is named after the DEVICE, not after this script, for two reasons. It used
  # to be "pf-shot-<prefix>", which `$match` never matches — so every run created another
  # simulator and none was ever reused (they piled up on the runner). And the name is user-visible:
  # `UIDevice.current.name` is what the pairing sheet prefills as this device's name, so
  # "pf-shot-iphone-6.9" was rendered into an App Store screenshot.
  local udid
  udid="$(xcrun simctl list devices available | grep -E "$match" | grep -oE '[0-9A-F-]{36}' | head -1 || true)"
  if [ -z "$udid" ]; then
    local devtype rt
    devtype="$(xcrun simctl list devicetypes | grep -E "$match" \
      | grep -oE 'com\.apple\.CoreSimulator\.SimDeviceType\.[A-Za-z0-9.-]+' | head -1 || true)"
    rt="$(xcrun simctl list runtimes available | grep -E "^$platform " \
      | grep -oE 'com\.apple\.CoreSimulator\.SimRuntime\.[A-Za-z0-9.-]+' | tail -1 || true)"
    if [ -n "$devtype" ] && [ -n "$rt" ]; then
      udid="$(xcrun simctl create "$createname" "$devtype" "$rt" 2>/dev/null || true)"
      [ -n "$udid" ] && log "$prefix — created Simulator \"$createname\" $udid ($devtype)"
    fi
  fi
  [ -n "$udid" ] || die "$prefix: no Simulator matching /$match/, and none could be created
       (needs a $platform runtime + a matching device type — check 'xcrun simctl list')."
  log "$prefix — Simulator $udid"
  xcrun simctl boot "$udid" 2>/dev/null || true
  xcrun simctl bootstatus "$udid" -b >/dev/null 2>&1 || true
  # Every scene is a dark-mode scene. The in-app `.environment(\.colorScheme, .dark)` override
  # does NOT cross a presentation boundary — a `.sheet` gets its own environment and follows the
  # DEVICE appearance — so the pairing sheet came out light grey over the dark app. Set the
  # simulator itself to dark and the whole hierarchy, presentations included, agrees.
  xcrun simctl ui "$udid" appearance dark >/dev/null 2>&1 || true

  log "$prefix — building ($scheme)…"
  # PF_SHOT_DERIVED_DATA (optional): a STABLE DerivedData root, so repeat runs reuse the
  # incremental build instead of cold-building into a throwaway tmpdir — CI pins this
  # (apple.yml); local runs keep the self-cleaning mktemp default.
  local dd; dd="${PF_SHOT_DERIVED_DATA:-$(mktemp -d)}"; mkdir -p "$dd"
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
    ios)   shoot_sim 'iPhone 16 Pro Max'   Punktfunk-iOS  iphonesimulator  iphone-6.9 iOS 'iPhone 16 Pro Max' ;;
    ipad)  shoot_sim 'iPad Pro 13|iPad Pro .*M4|iPad Pro \(13' Punktfunk-iOS iphonesimulator ipad-13 iOS 'iPad Pro 13-inch (M4)' ;;
    tvos)  shoot_sim 'Apple TV'            Punktfunk-tvOS appletvsimulator appletv    tvOS 'Apple TV 4K' ;;
    all)
      shoot_macos
      if xcrun --find simctl >/dev/null 2>&1; then
        shoot_sim 'iPhone 16 Pro Max' Punktfunk-iOS iphonesimulator iphone-6.9 iOS 'iPhone 16 Pro Max'
        shoot_sim 'iPad Pro 13|iPad Pro .*M4|iPad Pro \(13' Punktfunk-iOS iphonesimulator ipad-13 iOS 'iPad Pro 13-inch (M4)'
        shoot_sim 'Apple TV' Punktfunk-tvOS appletvsimulator appletv tvOS 'Apple TV 4K'
      else
        warn "Skipping iOS/iPadOS/tvOS — full Xcode not found (Command Line Tools only)."
      fi
      ;;
    *) die "unknown target '$target' (use: all macos ios ipad tvos)" ;;
  esac
done

log "Done. Screenshots in $OUT"
ls -1 "$OUT" 2>/dev/null || true

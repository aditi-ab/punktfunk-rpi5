#!/usr/bin/env bash
# Build the punktfunk Linux client as a single-file `.flatpak` bundle.
#
# Works on the Steam Deck (org.flatpak.Builder from Flathub, user-scope, NO root) and on any
# Linux box with flatpak + flatpak-builder. The CI does the same steps (.gitea/workflows/flatpak.yml).
#
# On the Deck (one-time):
#   flatpak install --user -y flathub org.flatpak.Builder
# Then run this script from the repo root:
#   bash packaging/flatpak/build-flatpak.sh
# Output: dist/punktfunk-client-<version>.flatpak  (install with `flatpak install --user <file>`)
#
# Env knobs:
#   VERSION=...        version string for the bundle name (default: git describe / 0.0.1-dev)
#   ONLINE=1           skip offline cargo-sources.json; build with --share=network (fast local
#                      iteration, non-reproducible). Default: offline (regenerates cargo-sources).
#   BUILDER=...        override the flatpak-builder invocation (default: auto-detect host
#                      flatpak-builder, else `flatpak run org.flatpak.Builder`).
set -euo pipefail

ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

APP_ID="io.unom.Punktfunk"
MANIFEST="packaging/flatpak/io.unom.Punktfunk.yml"
VERSION="${VERSION:-$(git describe --tags --always --dirty 2>/dev/null || echo 0.0.1-dev)}"
VERSION="${VERSION#v}"
BUNDLE="dist/punktfunk-client-${VERSION}.flatpak"

# --- pick a flatpak-builder (host binary, or the org.flatpak.Builder flatpak on the Deck) ---
if [ -n "${BUILDER:-}" ]; then
  FPB=($BUILDER)
elif command -v flatpak-builder >/dev/null 2>&1; then
  FPB=(flatpak-builder)
elif flatpak info org.flatpak.Builder >/dev/null 2>&1; then
  FPB=(flatpak run org.flatpak.Builder)
else
  echo "error: need flatpak-builder. On the Deck: flatpak install --user -y flathub org.flatpak.Builder" >&2
  exit 1
fi

# --- ensure Flathub is available for the runtime/SDK/extensions ---
flatpak remote-add --user --if-not-exists flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo

# --- offline crate cache (skip with ONLINE=1) -------------------------------------------
EXTRA_ARGS=()
if [ "${ONLINE:-0}" = "1" ]; then
  echo "==> ONLINE build (cargo fetches from crates.io; non-reproducible)"
  EXTRA_ARGS+=(--build-args=--share=network)
  # The manifest references cargo-sources.json; provide an empty list so it stays valid.
  [ -f packaging/flatpak/cargo-sources.json ] || echo '[]' > packaging/flatpak/cargo-sources.json
elif [ -f packaging/flatpak/cargo-sources.json ] && [ "${FORCE_GEN:-0}" != "1" ]; then
  # Reuse a cargo-sources.json that was generated elsewhere (e.g. on a dev box with network +
  # python aiohttp/toml, then rsynced to a build host that lacks them — like the Deck). The
  # offline crate cache is a pure function of Cargo.lock, so this is reproducible. FORCE_GEN=1
  # to regenerate anyway.
  echo "==> reusing existing packaging/flatpak/cargo-sources.json (FORCE_GEN=1 to regenerate)"
else
  echo "==> generating offline cargo-sources.json from Cargo.lock"
  GEN=/tmp/flatpak-cargo-generator.py
  if [ ! -f "$GEN" ]; then
    curl -fsSL -o "$GEN" \
      https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
  fi
  # Needs python3 + aiohttp + tomlkit. On a host that lacks them (e.g. the Deck), generate on the
  # Mac / a dev box instead and rsync the result next to the manifest (reused by the branch above).
  # Prune the microsoft/windows-rs git crates first (punktfunk-client-windows only) — otherwise
  # flatpak-builder full-clones that multi-GB repo and fills the disk. See prune-windows-lock.py.
  python3 packaging/flatpak/prune-windows-lock.py Cargo.lock /tmp/Cargo.flatpak.lock
  python3 "$GEN" /tmp/Cargo.flatpak.lock -o packaging/flatpak/cargo-sources.json
fi

# --- build into a local ostree repo, then export a single-file bundle --------------------
echo "==> flatpak-builder ($APP_ID, version $VERSION)"
"${FPB[@]}" --user --force-clean --disable-rofiles-fuse \
  --install-deps-from=flathub \
  "${EXTRA_ARGS[@]}" \
  --repo="$ROOTDIR/.flatpak-repo" \
  "$ROOTDIR/.flatpak-build" "$MANIFEST"

mkdir -p dist
flatpak build-bundle "$ROOTDIR/.flatpak-repo" "$BUNDLE" "$APP_ID"
echo "built $BUNDLE"
ls -lh "$BUNDLE"
echo
echo "install:  flatpak install --user -y $BUNDLE"
echo "run:      flatpak run $APP_ID            (or:  flatpak run $APP_ID --connect host:port)"

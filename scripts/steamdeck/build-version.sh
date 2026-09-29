#!/usr/bin/env bash
# The version a Deck build reports, spelled the way its channel's feed spells it. A checkout at a
# release tag reports that release. Any other commit reports the canary base CI uses (latest
# release, minor + 1: scripts/ci/pf-version.sh) plus the commit, so the console tells one rebuild
# from the next. Prints nothing outside a git checkout; the build then uses the Cargo version.
#
#   bash scripts/steamdeck/build-version.sh <source dir>
set -euo pipefail
SRC="${1:?usage: build-version.sh <source dir>}"
SHA="$(git -C "$SRC" rev-parse --short HEAD 2>/dev/null)" || exit 0
TAG="$(git -C "$SRC" tag --points-at HEAD | grep -xE 'v[0-9]+\.[0-9]+\.[0-9]+' | sort -V | tail -n1 || true)"
# Local tags only: the post-OS-update rebuild check runs this offline.
eval "$(PF_VERSION_TAGS="$(git -C "$SRC" tag -l 'v*')" GITHUB_REF="${TAG:+refs/tags/$TAG}" \
    GITHUB_REF_NAME="$TAG" GITHUB_ENV='' bash "$SRC/scripts/ci/pf-version.sh")"
if [ -n "$TAG" ]; then
    echo "$PF_BASE"
else
    echo "$PF_BASE+g$SHA"
fi

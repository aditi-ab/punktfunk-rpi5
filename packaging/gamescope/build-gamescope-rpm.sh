#!/usr/bin/env bash
# Package an already-built punktfunk-gamescope binary as an RPM, for the Gitea RPM registry.
#
# WHY this exists: before it, the only ways to get punktfunk-gamescope were the Bazzite/Atomic
# sysext, the Arch package, the SteamOS installer, a NixOS option — or building gamescope from
# source yourself. A traditional Fedora-family box (Nobara, plain Fedora, Nobara-derived HTPCs)
# had no packaged route at all, which is how a field report ended up on a stock gamescope streaming
# a session that told every game the display was 60 Hz.
#
# The binary is NOT built here; CI builds it once per Fedora major and caches it
# (.gitea/workflows/rpm.yml). See punktfunk-gamescope.spec's header for why repacking beats
# rebuilding.
#
# Usage:
#   bash packaging/gamescope/build-gamescope-rpm.sh \
#     --binary gs-cache/punktfunk-gamescope \
#     [--version 3.16.25] [--release 1] [--outdir dist]
#
# Output: <outdir>/punktfunk-gamescope-<version>-<release>.<arch>.rpm
set -euo pipefail

BINARY=""
# Default the version to the upstream gamescope the pinned revision describes as, suffixed with the
# patch-set revision — same shape as the Arch package's `pkgver`, so the two channels read alike.
VERSION=""
RELEASE="1"
OUTDIR="dist"

while [ $# -gt 0 ]; do
  case "$1" in
    --binary)  BINARY="${2:?--binary needs a path}"; shift 2 ;;
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --release) RELEASE="${2:?--release needs a value}"; shift 2 ;;
    --outdir)  OUTDIR="${2:?--outdir needs a value}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$BINARY" ] || { echo "ERROR: --binary is required" >&2; exit 2; }
[ -x "$BINARY" ] || { echo "ERROR: $BINARY is not an executable file" >&2; exit 1; }

ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

# Derive the version from the binary itself when not told: it is the only source that cannot drift
# from what is actually being packaged. `gamescope version 3.16.25-1-g8c676c3+pfhdr4 (gcc …)` →
# `3.16.25` + the marker. RPM versions may not contain `-`, hence the trailing `.pfhdrN` form.
BANNER="$("$BINARY" --version 2>&1 | head -1)"
case "$BANNER" in
  *'+pfhdr'*) ;;
  *) echo "ERROR: $BINARY has no +pfhdr marker — it is not a punktfunk gamescope build" >&2
     echo "       banner: $BANNER" >&2
     exit 1 ;;
esac
PFHDR="$(printf '%s\n' "$BANNER" | grep -o '+pfhdr[0-9]\+' | head -1 | tr -d '+')"
if [ -z "$VERSION" ]; then
  UPSTREAM="$(printf '%s\n' "$BANNER" | grep -o '[0-9]\+\.[0-9]\+\.[0-9]\+' | head -1)"
  [ -n "$UPSTREAM" ] || { echo "ERROR: no X.Y.Z version in banner: $BANNER" >&2; exit 1; }
  VERSION="${UPSTREAM}.${PFHDR}"
fi

echo "==> packaging $BINARY as punktfunk-gamescope-${VERSION}-${RELEASE}"
echo "    banner: $BANNER"

TOP="$(mktemp -d)"
trap 'rm -rf "$TOP"' EXIT
mkdir -p "$TOP"/{SOURCES,SPECS,BUILD,BUILDROOT,RPMS,SRPMS}
install -m0755 "$BINARY" "$TOP/SOURCES/punktfunk-gamescope"

mkdir -p "$OUTDIR"
rpmbuild \
  --define "_topdir $TOP" \
  --define "pf_version $VERSION" \
  --define "pf_release $RELEASE" \
  -bb packaging/gamescope/punktfunk-gamescope.spec

find "$TOP/RPMS" -name '*.rpm' -exec cp -v {} "$OUTDIR/" \;
echo "==> wrote $(find "$OUTDIR" -name 'punktfunk-gamescope-*.rpm' -newer "$TOP" -print -quit 2>/dev/null || echo "$OUTDIR"/punktfunk-gamescope-*.rpm)"

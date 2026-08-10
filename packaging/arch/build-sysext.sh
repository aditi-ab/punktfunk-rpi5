#!/usr/bin/env bash
# Wrap a built punktfunk pacman package into a systemd-sysext image — the update-survivable way to
# add it to an immutable Arch-derived distro (SteamOS 3): the .raw overlays /usr read-only from the
# writable /var/lib/extensions/, so it persists across A/B OS updates with no `steamos-readonly
# disable`. Works for either split package — on a Steam Deck you'd wrap the CLIENT. Needs
# `bsdtar`/`tar`, `squashfs-tools` (mksquashfs).
#
# Usage:  bash build-sysext.sh [--gamescope <punktfunk-gamescope-*.pkg.tar.zst>] \
#                              <punktfunk-{host,client}-*.pkg.tar.zst>
# Output: <pkgname>.raw   (e.g. punktfunk-client.raw)
#
# --gamescope folds the HDR-capable gamescope companion package (packaging/gamescope) into a HOST
# image as /usr/bin/punktfunk-gamescope — what lets the gamescope backend stream 10-bit BT.2020 PQ
# instead of 8-bit SDR (the host prefers that name on PATH and attempts HDR by default). Mirrors
# the Bazzite image's fold-in, including the honesty check: the binary is verified by executing
# its `+pfhdr` banner, never trusted by filename. Omit it and the image is exactly what it was —
# the host then stays SDR on that backend, by design.
#
# Capabilities in the image: NEVER on usr/bin/punktfunk-host, `cap_sys_nice=ep` on
# usr/bin/punktfunk-encode-worker (best-effort), and none on punktfunk-gamescope.
#
# ⚠ Capabilities are NOT lost on the way in — that was this comment's earlier claim and it is
# false: mksquashfs records security.capability, and the published Bazzite 0.26.0-1 image really
# did carry `cap_sys_nice=ep` on usr/bin/punktfunk-host. The host is left uncapped on purpose. A
# capability on the HOST binary makes it unidentifiable to KWin (which resolves a client's
# /proc/<pid>/exe to match it against a .desktop, and cannot read it for a capability-carrying
# process) and kills every Desktop-mode session.
#
# ⚠ And it is NOT enough to leave it out here: pacman scriptlets never run for a sysext, so the
# `setcap` in punktfunk-host.install cannot reach this image either way. The encode worker is
# therefore capped on the staging tree below — this is the only place a sysext can acquire it — and
# both halves of the matrix are asserted before mksquashfs, exactly as
# packaging/bazzite/build-sysext.sh does. `punktfunk-gamescope` is a compositor, not a KWin client,
# so it is unaffected by the host rule and simply runs without a capability here, pacing slightly
# worse.
set -euo pipefail

GAMESCOPE=""
if [ "${1:-}" = "--gamescope" ]; then
  GAMESCOPE="${2:?--gamescope needs a punktfunk-gamescope package}"; shift 2
fi
# No braces in the message: a literal `}` inside ${1:?...} terminates the expansion early and
# corrupts $PKG (the tail of the message gets appended to the value — a real field bug).
PKG="${1:?usage: build-sysext.sh [--gamescope <pkg>] <punktfunk-host|client pkg.tar.zst>}"
[ -f "$PKG" ] || { echo "no such package: $PKG" >&2; exit 1; }
# Derive the package name from the file (pkgname is everything before the -<version>).
NAME="$(basename "$PKG" | sed -E 's/-[0-9].*//')"
[ -n "$NAME" ] || { echo "could not derive package name from $PKG" >&2; exit 1; }
if [ -n "$GAMESCOPE" ] && [ "$NAME" != "punktfunk-host" ]; then
  echo "--gamescope only makes sense for a punktfunk-host image (got: $NAME)" >&2; exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# A pacman package is a (zstd) tarball; a sysext only carries /usr (the host /etc, /var are the
# system's). Extract just usr/ from the payload.
if command -v bsdtar >/dev/null 2>&1; then
  bsdtar -C "$STAGE" -xf "$PKG" usr
else
  tar -C "$STAGE" -xf "$PKG" usr
fi

# The HDR gamescope companion (see --gamescope in the header). Verified by its banner marker
# rather than trusted by filename: an unpatched gamescope shipped under this name would make the
# host promise HDR it cannot deliver, and the punktfunk/1 Welcome cannot take that back
# mid-session. Executing the staged binary needs a build box the binary runs on (the Arch CI
# container qualifies; it built it).
if [ -n "$GAMESCOPE" ]; then
  [ -f "$GAMESCOPE" ] || { echo "no such package: $GAMESCOPE" >&2; exit 1; }
  if command -v bsdtar >/dev/null 2>&1; then
    bsdtar -C "$STAGE" -xf "$GAMESCOPE" usr
  else
    tar -C "$STAGE" -xf "$GAMESCOPE" usr
  fi
  GS_BIN="$STAGE/usr/bin/punktfunk-gamescope"
  [ -x "$GS_BIN" ] || { echo "$GAMESCOPE did not provide usr/bin/punktfunk-gamescope" >&2; exit 1; }
  "$GS_BIN" --version 2>&1 | grep -q '+pfhdr' || {
    echo "$GAMESCOPE's binary has no +pfhdr marker — it is not a punktfunk HDR build" >&2; exit 1; }
  echo "folded in $("$GS_BIN" --version 2>&1 | head -1)"
fi

# The marker systemd-sysext requires to merge the image. ID=_any merges onto ANY host os-release
# (SteamOS, Arch, Bazzite); ARCHITECTURE pins it to x86-64 so it's never merged on the wrong arch.
install -d "$STAGE/usr/lib/extension-release.d"
cat > "$STAGE/usr/lib/extension-release.d/extension-release.$NAME" <<EOF
ID=_any
ARCHITECTURE=x86-64
EOF

# CAP_SYS_NICE on the encode worker (see the header). A pacman payload carries no capabilities and
# no scriptlet ever runs for a sysext, so without this the SteamOS image ships the lever inert —
# on the box with the smallest GPU shared between game and encode. Needs CAP_SETFCAP, i.e. root or
# fakeroot; a plain-user build simply ships without it, which is a pacing loss and nothing more.
#
# `getcap` on an uncapped file exits 0 and prints nothing, so an empty read is unambiguous; the
# output form differs across libcap versions ("path cap_sys_nice=ep" since ~2.36, "path =
# cap_sys_nice+ep" before), hence the normalizer.
_pf_caps_of() {
  local raw; raw="$(getcap "$1" 2>/dev/null || true)"
  [ -n "$raw" ] || { printf ''; return 0; }
  printf '%s' "${raw#* }" | sed -e 's/^= *//' -e 's/+/=/' -e 's/[[:space:]]*$//'
}

# BEFORE granting: refuse a capability that arrived from somewhere else. The setcap below would
# overwrite it and ship a correct-looking image while the surprise went unreported everywhere else.
# Order matters: assert first, then grant, or the "anything else" arm can never fire.
if command -v getcap >/dev/null 2>&1 && [ -f "$STAGE/usr/bin/punktfunk-encode-worker" ]; then
  arrived_caps="$(_pf_caps_of "$STAGE/usr/bin/punktfunk-encode-worker")"
  case "$arrived_caps" in
    ''|cap_sys_nice=ep) : ;;
    *)
      echo "ERROR: staged usr/bin/punktfunk-encode-worker ARRIVED carrying '$arrived_caps'." >&2
      echo "       A pacman payload carries no capabilities, so something else granted it — find" >&2
      echo "       out what, because it is doing the same on the plain package path, unchecked." >&2
      exit 1 ;;
  esac
fi

if [ -f "$STAGE/usr/bin/punktfunk-encode-worker" ]; then
  if setcap 'cap_sys_nice=ep' "$STAGE/usr/bin/punktfunk-encode-worker" 2>/dev/null; then
    echo "granted CAP_SYS_NICE to usr/bin/punktfunk-encode-worker (GPU-priority lever active)"
  else
    echo "WARNING: could not setcap CAP_SYS_NICE on usr/bin/punktfunk-encode-worker (need" >&2
    echo "         root/CAP_SETFCAP) — the image ships without it and PyroWave encodes at" >&2
    echo "         default GPU priority." >&2
  fi
fi

# Assert the final matrix before it is sealed into a read-only squashfs: host EMPTY (hard fail),
# worker exactly cap_sys_nice=ep or nothing at all (missing is fine — the grant is best-effort).
if command -v getcap >/dev/null 2>&1; then
  if [ -f "$STAGE/usr/bin/punktfunk-host" ]; then
    staged_caps="$(_pf_caps_of "$STAGE/usr/bin/punktfunk-host")"
    if [ -n "$staged_caps" ]; then
      echo "ERROR: staged usr/bin/punktfunk-host carries capabilities: $staged_caps" >&2
      echo "       A capability makes the host unidentifiable to KWin and breaks every Desktop-mode" >&2
      echo "       session on a merged image, which cannot be repaired on the box (read-only /usr)." >&2
      echo "       The GPU-priority capability belongs on usr/bin/punktfunk-encode-worker, never here." >&2
      exit 1
    fi
  fi
  if [ -f "$STAGE/usr/bin/punktfunk-encode-worker" ]; then
    worker_caps="$(_pf_caps_of "$STAGE/usr/bin/punktfunk-encode-worker")"
    case "$worker_caps" in
      '')              echo "note: usr/bin/punktfunk-encode-worker ships uncapped — PyroWave encodes at default GPU priority" ;;
      cap_sys_nice=ep) : ;;
      *)
        echo "ERROR: staged usr/bin/punktfunk-encode-worker carries '$worker_caps'," >&2
        echo "       expected exactly 'cap_sys_nice=ep' (or nothing at all)." >&2
        echo "       Refusing to bake an unexpected capability into a read-only image." >&2
        exit 1 ;;
    esac
  fi
fi

OUT="$NAME.raw"
rm -f "$OUT"
mksquashfs "$STAGE" "$OUT" -all-root -noappend -quiet
echo "built $OUT"
echo "  install:  sudo cp $OUT /var/lib/extensions/ && sudo systemctl enable --now systemd-sysext"
if [ "$NAME" = "punktfunk-host" ]; then
  echo "  then:     systemctl --user enable --now punktfunk-host"
else
  echo "  then:     run 'punktfunk-client' (or let the Decky plugin launch it)"
fi

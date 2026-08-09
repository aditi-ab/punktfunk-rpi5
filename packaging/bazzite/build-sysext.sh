#!/usr/bin/env bash
# Build the punktfunk systemd-sysext image for Bazzite / Fedora Atomic from the built RPMs —
# the no-layering install path (rpm-ostree layering slows every update and can block upgrades;
# a sysext never enters an rpm-ostree transaction). The .raw overlays /usr read-only from
# /var/lib/extensions/, survives OS updates, and is toggled/updated without a reboot.
#
# Counterpart to ../arch/build-sysext.sh (which wraps a pacman package for SteamOS). This one
# wraps the Fedora RPMs (punktfunk + punktfunk-web) and additionally:
#   * relocates the RPMs' /etc payload to /usr/share/punktfunk/etc/ (a sysext carries ONLY /usr;
#     punktfunk-sysext(8) copies these into the real /etc on install),
#   * bakes SELinux labels in as squashfs pseudo-xattrs, computed with matchpathcon from the
#     build container's targeted policy. Without them every file is unlabeled_t at runtime:
#     fine for the user session + systemd --user units (unconfined), but system daemons are
#     DENIED — udev couldn't read 60-punktfunk.rules and systemd-sysctl couldn't read the
#     sysctl drop-in (validated live on Bazzite 43, SELinux enforcing, 2026-07-04),
#   * pins compatibility via ID=fedora + VERSION_ID: merges on Bazzite/Silverblue/Aurora of the
#     SAME Fedora major (ID_LIKE matching, systemd >= 256) and is REFUSED after a major rebase
#     instead of running soname-broken binaries (`punktfunk-sysext update` then re-resolves),
#   * embeds the punktfunk-sysext helper so an installed box can update itself.
#
# Build in the matching Fedora container (ci/fedora*-rpm.Dockerfile) — matchpathcon needs the
# Fedora targeted policy (libselinux-utils + selinux-policy-targeted), and the RPMs are
# soname-coupled to their base anyway. Needs: rpm2cpio, cpio, mksquashfs (>= 4.6), matchpathcon.
#
# Usage:
#   bash build-sysext.sh --version-id 43 --out dist/punktfunk-0.7.1-1-x86-64.raw \
#        [--gamescope path/to/punktfunk-gamescope] \
#        dist/punktfunk-0.7.1-1.fc43.x86_64.rpm dist/punktfunk-web-0.7.1-1.fc43.noarch.rpm
#
# --gamescope folds in a prebuilt HDR-capable gamescope (packaging/gamescope) as
# /usr/bin/punktfunk-gamescope, which is what lets the gamescope backend stream 10-bit BT.2020 PQ.
# It is NOT built here: it is a C++ meson build with gamescope's whole dependency set, so CI builds
# it in the same Fedora container beforehand (`bash packaging/gamescope/build-punktfunk-gamescope.sh
# --destdir stage --prefix /usr`) and passes the resulting binary in. Omit it and the image is
# exactly what it was — the host then stays SDR on that backend, by design.
#
# The installed image MUST be named punktfunk.raw (the embedded extension-release marker is
# extension-release.punktfunk; systemd-sysext requires marker == image name) — the feed carries
# versioned filenames and punktfunk-sysext installs to the fixed name.
set -euo pipefail

VERSION_ID="" OUT="" GAMESCOPE="" RPMS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --version-id) VERSION_ID="${2:?}"; shift 2 ;;
    --out)        OUT="${2:?}"; shift 2 ;;
    --gamescope)  GAMESCOPE="${2:?}"; shift 2 ;;
    *)            RPMS+=("$1"); shift ;;
  esac
done
[ -n "$VERSION_ID" ] || { echo "missing --version-id <fedora major, e.g. 43>" >&2; exit 1; }
[ -n "$OUT" ] || { echo "missing --out <image.raw>" >&2; exit 1; }
[ "${#RPMS[@]}" -gt 0 ] || { echo "no RPMs given" >&2; exit 1; }
for tool in rpm2cpio cpio mksquashfs matchpathcon; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

HERE="$(cd "$(dirname "$0")" && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# SYSEXT_VERSION_ID from the punktfunk RPM (V-R without the dist tag): what
# `punktfunk-sysext status` reports as the installed version.
PF_VR=""
SEEN_NAMES=" "
for rpm in "${RPMS[@]}"; do
  [ -f "$rpm" ] || { echo "no such RPM: $rpm" >&2; exit 1; }
  name="$(rpm -qp --qf '%{NAME}' "$rpm" 2>/dev/null)"
  # Two RPMs of the same NAME (e.g. a stale noarch next to the current x86_64 from a sloppy
  # download glob) silently shadow each other's files — refuse instead of building a chimera.
  case "$SEEN_NAMES" in *" $name "*) echo "duplicate RPM name '$name' in inputs — pass exactly one RPM per package" >&2; exit 1 ;; esac
  SEEN_NAMES="$SEEN_NAMES$name "
  if [ "$name" = punktfunk ]; then
    PF_VR="$(rpm -qp --qf '%{VERSION}-%{RELEASE}' "$rpm" 2>/dev/null)"
    PF_VR="${PF_VR%.fc*}"
  fi
  rpm2cpio "$rpm" | ( cd "$STAGE" && cpio -idmu --quiet )
done
[ -n "$PF_VR" ] || { echo "the punktfunk (host) RPM must be among the inputs" >&2; exit 1; }

# A sysext carries only /usr. Relocate the RPMs' /etc payload (gamescope-session drop-in, tray
# autostart entry) under /usr/share/punktfunk/etc/ — punktfunk-sysext copies it into /etc.
if [ -d "$STAGE/etc" ]; then
  mkdir -p "$STAGE/usr/share/punktfunk/etc"
  cp -a "$STAGE/etc/." "$STAGE/usr/share/punktfunk/etc/"
  rm -rf "${STAGE:?}/etc"
fi
rm -rf "${STAGE:?}/var"   # rpm ghosts etc. — nothing outside /usr may remain

# The HDR-capable gamescope, when one was built (see --gamescope in the header). Verified by its
# banner marker rather than trusted by filename: an unpatched gamescope shipped under this name
# would make the host promise HDR it cannot deliver, and the punktfunk/1 Welcome cannot take that
# back mid-session.
if [ -n "$GAMESCOPE" ]; then
  [ -x "$GAMESCOPE" ] || { echo "no such executable: $GAMESCOPE" >&2; exit 1; }
  "$GAMESCOPE" --version 2>&1 | grep -q '+pfhdr' || {
    echo "$GAMESCOPE has no +pfhdr marker — it is not a punktfunk HDR build" >&2; exit 1; }
  install -Dm0755 "$GAMESCOPE" "$STAGE/usr/bin/punktfunk-gamescope"
fi

# Enable the plugin/script runner for every user, by baking its `[Install] WantedBy=default.target`
# symlink straight into the image.
#
# A sysext carries only /usr, and RPM scriptlets never run from one — so the `systemctl --global
# enable` the .rpm/.deb do at install time has no equivalent here, and without this the runner would
# ship present-but-off on exactly the platform (Bazzite / Fedora Atomic) where an operator is least
# likely to go hunting for it. The game-library scanners are plugins now (design D9), so an
# unenabled runner means an empty library.
#
# Opt-out is unchanged and still wins: `systemctl --user mask punktfunk-scripting` in the user's own
# ~/.config/systemd/user takes precedence over anything under /usr.
if [ -f "$STAGE/usr/lib/systemd/user/punktfunk-scripting.service" ]; then
  install -d "$STAGE/usr/lib/systemd/user/default.target.wants"
  ln -sf ../punktfunk-scripting.service \
    "$STAGE/usr/lib/systemd/user/default.target.wants/punktfunk-scripting.service"
fi

# Self-update: the helper rides inside the image.
install -Dm0755 "$HERE/punktfunk-sysext.sh" "$STAGE/usr/bin/punktfunk-sysext"

# Compatibility marker. ID=fedora matches Bazzite & friends through os-release ID_LIKE;
# VERSION_ID makes a major-rebased host refuse the old ABI instead of merging it.
install -d "$STAGE/usr/lib/extension-release.d"
cat > "$STAGE/usr/lib/extension-release.d/extension-release.punktfunk" <<EOF
ID=fedora
VERSION_ID=$VERSION_ID
ARCHITECTURE=x86-64
SYSEXT_ID=punktfunk
SYSEXT_VERSION_ID=$PF_VR
EXTENSION_RELOAD_MANAGER=1
EOF

# NO CAP_SYS_NICE in the image — and an assertion that none crept back in.
#
# 0.26.0-1 setcap'd the staged binary here for the GPU-priority lever. mksquashfs records
# security.capability, so the capability really did ship: verified by mounting the published
# punktfunk-0.26.0-1-x86-64.raw, where `getcap usr/bin/punktfunk-host` reports `cap_sys_nice=ep`.
# That broke desktop streaming on every Bazzite KDE box, field-reported as
# "KWin does not expose zkde_screencast_unstable_v1 to this client".
#
# KWin advertises its restricted protocols (zkde_screencast_unstable_v1 for the virtual output,
# org_kde_kwin_fake_input for input) only to a client it can IDENTIFY, by resolving that client's
# /proc/<pid>/exe and matching it against an installed .desktop's Exec= — the image ships
# usr/share/applications/io.unom.Punktfunk.Host.desktop for exactly that. The kernel refuses that
# readlink to any reader whose effective set is not a superset of the target's PERMITTED set
# (cap_ptrace_access_check), and KWin holds no capabilities. So a capability in this image makes the
# host unidentifiable and every Desktop-mode session dies. Full matrix, including why neither
# prctl(PR_SET_DUMPABLE, 1) nor systemd AmbientCapabilities= rescues it, in
# packaging/arch/punktfunk-host.install.
#
# A merged sysext's /usr is a read-only squashfs, so this cannot be repaired on the box — the image
# is the only place it can be got right. Assert it rather than trust it: the RPM payload arrives via
# `rpm2cpio | cpio`, which carries no capabilities today, but the spec is one `%caps()` away from
# changing that and this build would silently bake it in.
if [ -f "$STAGE/usr/bin/punktfunk-host" ] && command -v getcap >/dev/null 2>&1; then
  staged_caps="$(getcap "$STAGE/usr/bin/punktfunk-host" 2>/dev/null || true)"
  if [ -n "$staged_caps" ]; then
    echo "ERROR: staged usr/bin/punktfunk-host carries capabilities: $staged_caps" >&2
    echo "       A capability makes the host unidentifiable to KWin and breaks every Desktop-mode" >&2
    echo "       session on a merged image, which cannot be repaired on the box (read-only /usr)." >&2
    exit 1
  fi
fi

# SELinux labels as pseudo-xattrs (see header). matchpathcon resolves each target path against
# the targeted policy's file_contexts; <<none>> means "no specific entry" — skip those (the
# handful of matches all resolve to real contexts for our payload).
PSEUDO="$STAGE.pseudo"
( cd "$STAGE" && find . -mindepth 1 \( -type f -o -type d \) -printf '/%P\n' ) | sort \
  | while IFS= read -r path; do
      ctx="$(matchpathcon -n "$path" 2>/dev/null || true)"
      case "$ctx" in ''|'<<none>>') continue ;; esac
      printf '%s x security.selinux=%s\n' "$path" "$ctx"
    done > "$PSEUDO"
[ -s "$PSEUDO" ] || { echo "matchpathcon produced no labels — refusing to build an unlabeled image" >&2; exit 1; }

rm -f "$OUT"; mkdir -p "$(dirname "$OUT")"
# -xattrs-exclude drops any security.selinux the staging fs already had (would collide with the
# pseudo defs when building on an SELinux host); -all-root because cpio extracted as the CI uid.
mksquashfs "$STAGE" "$OUT" -all-root -noappend -quiet \
  -xattrs-exclude '^security.selinux' -pf "$PSEUDO"
rm -f "$PSEUDO"
echo "built $OUT (punktfunk $PF_VR, fedora $VERSION_ID, $(du -h "$OUT" | cut -f1))"
echo "  install on the box:  punktfunk-sysext install   (or --from-file $OUT)"

#!/usr/bin/env bash
# Assert that a BUILT Linux package ships every path packaging/linux/payload.txt names for it.
#
# deb, rpm and pacman each spell their payload out by hand (packaging/debian/build-*.sh,
# packaging/rpm/punktfunk.spec, packaging/arch/PKGBUILD). payload.txt is the part they share; this
# reads each artifact through assert-cap-matrix.sh's payload readers and names every listed path
# the artifact lacks. pacman has no /usr/libexec, so usr/libexec/punktfunk/ reads
# usr/lib/punktfunk/ for a .pkg.tar.*, except in seats, which keeps /usr/libexec.
#
# Usage:
#   scripts/ci/assert-payload-parity.sh <artifact> [<artifact> ...]
#   scripts/ci/assert-payload-parity.sh --self-test     # pure bash, runs anywhere
#
# An artifact that ships no package's first row (web, scripting, debuginfo) is reported and
# skipped. One that cannot be read fails.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=assert-cap-matrix.sh
. "$HERE/assert-cap-matrix.sh"
PAYLOAD="$HERE/../../packaging/linux/payload.txt"

# paths <package> <artifact name>: payload.txt's paths for <package>, read through the pacman
# relocation when the name is a pacman package.
paths() {
  local relocate='s#^usr/libexec/punktfunk/#usr/lib/punktfunk/#'
  case "$2" in *.pkg.tar.*) [ "$1" != seats ] || relocate='' ;; *) relocate='' ;; esac
  awk -v p="$1" '$1 == p { print $2 }' "$PAYLOAD" | sed -e "$relocate"
}

# missing_paths <package> <artifact name> <listing>: the paths the listing lacks.
missing_paths() {
  paths "$1" "$2" | grep -vxF -f <(printf '%s\n' "$3") || true
}

check_parity() {
  local artifact="$1" label listing pkg missing seen=0 rc=0
  label="$(basename "$artifact")"
  case "$label" in
    *.deb|*.rpm|*.pkg.tar.zst|*.pkg.tar.xz) ;;
    *) err "$label: not a .deb, .rpm or pacman package"; return 1 ;;
  esac
  listing="$(payload_listing "$artifact")"
  require_listing "$label" "$listing" || return 1
  for pkg in $(awk '/^[a-z]/ { print $1 }' "$PAYLOAD" | sort -u); do
    grep -qxF "$(paths "$pkg" "$label" | head -n 1)" <<<"$listing" || continue
    seen=1
    missing="$(missing_paths "$pkg" "$label" "$listing")"
    if [ -n "$missing" ]; then
      err "$label: the $pkg package lacks paths every format ships (packaging/linux/payload.txt):"
      printf '%s\n' "$missing" | sed 's/^/    /' >&2
      rc=1
    else
      note "OK  $label: $pkg payload matches packaging/linux/payload.txt"
    fi
  done
  [ "$seen" = 1 ] || note "--  $label: no package payload.txt names, skipping"
  return "$rc"
}

# Every row states the paths it MUST report missing, so a checker that can no longer see a gap
# fails here first.
self_test() {
  local failures=0 host t
  host="$(awk '$1 == "host" { print $2 }' "$PAYLOAD")"
  [ -n "$host" ] || { err "self-test: payload.txt names no host paths"; return 1; }
  _case() {  # _case <label> <artifact name> <listing> <want missing>
    local got; got="$(missing_paths host "$2" "$3")"
    if [ "$got" = "$4" ]; then printf '  ok    %s\n' "$1"
    else printf '  FAIL  %s: missing=%s (wanted %s)\n' "$1" "${got:-<none>}" "${4:-<none>}"; failures=$((failures + 1)); fi
  }
  _case "complete deb" x.deb "$host" ""
  _case "rpm without its sysctl" x.rpm "$(grep -vx usr/lib/sysctl.d/99-punktfunk-net.conf <<<"$host")" \
    usr/lib/sysctl.d/99-punktfunk-net.conf
  _case "pacman reads libexec as lib" x.pkg.tar.zst \
    "$(sed 's#^usr/libexec/punktfunk/#usr/lib/punktfunk/#' <<<"$host")" ""
  _case "pacman with libexec unrelocated" x.pkg.tar.zst "$host" \
    "$(grep '^usr/libexec/punktfunk/' <<<"$host" | sed 's#^usr/libexec/#usr/lib/#')"
  local seats; seats="$(awk '$1 == "seats" { print $2 }' "$PAYLOAD")"
  if [ -n "$(missing_paths seats x.pkg.tar.zst "$seats")" ]; then
    printf '  FAIL  %s\n' "pacman keeps libexec for seats"; failures=$((failures + 1))
  else
    printf '  ok    %s\n' "pacman keeps libexec for seats"
  fi

  # The pacman reader, through a real archive: "./" and directory entries must not hide a path.
  t="$(mktemp -d)"
  mkdir -p "$t/s/usr/bin"; : > "$t/s/usr/bin/punktfunk-host"
  (cd "$t/s" && tar -cf "$t/x.pkg.tar.zst" ./usr)
  if payload_listing "$t/x.pkg.tar.zst" | grep -qx usr/bin/punktfunk-host; then
    printf '  ok    %s\n' "reader strips ./ from a tar listing"
  else
    printf '  FAIL  %s\n' "reader strips ./ from a tar listing"; failures=$((failures + 1))
  fi
  rm -rf "$t"

  if [ "$failures" != 0 ]; then err "self-test: $failures case(s) wrong"; return 1; fi
  note "self-test: all cases behaved as specified"
}

main() {
  [ $# -gt 0 ] || { echo "usage: $0 <artifact> [...] | --self-test" >&2; return 2; }
  if [ "$1" = "--self-test" ]; then self_test; return $?; fi
  local artifact rc=0
  for artifact in "$@"; do
    [ -e "$artifact" ] || { err "no such artifact: $artifact"; rc=1; continue; }
    check_parity "$artifact" || rc=1
  done
  return "$rc"
}

main "$@"

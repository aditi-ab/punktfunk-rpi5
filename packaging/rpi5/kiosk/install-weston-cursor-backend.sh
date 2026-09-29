#!/usr/bin/env bash
# Install the verified ARM64 module into an offline Raspberry Pi OS root.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=${1:?Usage: install-weston-cursor-backend.sh ROOT [MODULE]}
module=${2:-$repo/weston-14.0.2-rpi5-cursor-drm-backend.so}
expected=9de49e163c8396e4de8a9dc6b7d747e978cada844553c1a951d8fa2707e7f91f
printf '%s  %s\n' "$expected" "$module" | sha256sum -c -
for package in weston libweston-14-0; do
    version=$(dpkg-query --admindir="$root/var/lib/dpkg" -W -f='${Version}' "$package")
    [[ "$version" == 14.0.2-1 ]] || { echo "Unsupported $package version: $version" >&2; exit 1; }
done
target="$root/usr/lib/aarch64-linux-gnu/libweston-14/drm-backend.so"
[[ -f "$target" ]] || { echo 'Missing original Weston DRM module' >&2; exit 1; }
if [[ ! -f "$target.punktfunk-original" ]]; then
    cp -a "$target" "$target.punktfunk-original"
fi
install -m 0755 "$module" "$target"
install -D -m 0644 "$repo/runtime/weston-14-disable-cursor-plane.patch" \
    "$root/usr/share/doc/weston/punktfunk-disable-cursor-plane.patch"
printf '%s\n' "$expected" > "$root/usr/share/doc/weston/punktfunk-drm-backend.sha256"

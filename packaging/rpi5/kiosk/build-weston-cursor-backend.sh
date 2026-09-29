#!/usr/bin/env bash
# Native ARM64 build of the Raspberry Pi OS Weston 14 cursor workaround.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
work=${1:?Usage: build-weston-cursor-backend.sh NEW_WORK_DIRECTORY}
[[ $(uname -m) == aarch64 ]] || { echo 'Build on ARM64 Raspberry Pi OS Trixie.' >&2; exit 1; }
[[ ! -e "$work" ]] || { echo 'Use a new build directory.' >&2; exit 1; }
mkdir -p "$work"
cd "$work"
curl -fL https://gitlab.freedesktop.org/wayland/weston/-/archive/14.0.2/weston-14.0.2.tar.gz -o source.tar.gz
echo '633f4e0f232ad150300c95ffcbc646fedf1349487bf389dbd2045fa69013d6e2  source.tar.gz' | sha256sum -c -
tar -xzf source.tar.gz
patch -d weston-14.0.2 -p1 < "$repo/runtime/weston-14-disable-cursor-plane.patch"
meson setup build weston-14.0.2 --buildtype=release \
    -Dbackend-drm-screencast-vaapi=false -Dbackend-headless=false \
    -Dbackend-pipewire=false -Dbackend-rdp=false -Dbackend-vnc=false \
    -Dbackend-wayland=false -Dbackend-x11=false -Dscreenshare=false \
    -Dxwayland=false -Dsystemd=false -Dremoting=false -Dpipewire=false \
    -Dshell-desktop=false -Dshell-fullscreen=false -Dshell-ivi=false \
    -Dshell-kiosk=true -Dimage-jpeg=false -Dimage-webp=false \
    -Dtools=[] -Ddemo-clients=false -Dsimple-clients=[] -Dtests=false -Ddoc=false
ninja -C build -j3 libweston/backend-drm/drm-backend.so
ldd -r build/libweston/backend-drm/drm-backend.so > linkage.txt
if grep -E 'not found|undefined symbol' linkage.txt; then exit 1; fi
sha256sum build/libweston/backend-drm/drm-backend.so

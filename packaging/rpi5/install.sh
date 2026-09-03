#!/bin/sh

set -eu

source_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
prefix=${PUNKTFUNK_INSTALL_PREFIX:-/opt/punktfunk-rpi5}
link_dir=${PUNKTFUNK_LINK_DIR:-/usr/local/bin}

if [ "$(id -u)" -ne 0 ]; then
    printf 'Run this installer as root, for example: sudo ./install.sh\n' >&2
    exit 1
fi

install -d -m 0755 "${prefix}" "${prefix}/lib" "${link_dir}"
install -m 0755 "${source_dir}/punktfunk" "${source_dir}/punktfunk-session" "${prefix}/"
cp -a "${source_dir}/lib/." "${prefix}/lib/"
install -m 0644 \
    "${source_dir}/README.md" \
    "${source_dir}/LICENSE-MIT" \
    "${source_dir}/LICENSE-APACHE" \
    "${source_dir}/FFMPEG-LICENSE-LGPL-2.1.txt" \
    "${source_dir}/build-version" \
    "${source_dir}/fork-commit" \
    "${source_dir}/rpi-ffmpeg-commit" \
    "${source_dir}/source-repository" \
    "${prefix}/"
ln -sfn "${prefix}/punktfunk" "${link_dir}/punktfunk"
ln -sfn "${prefix}/punktfunk-session" "${link_dir}/punktfunk-session"
printf 'Installed Punktfunk RPi5 in %s\n' "${prefix}"

#!/usr/bin/env bash

set -Eeuo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
tag=${1:?Usage: build-release.sh TAG [OUTPUT_DIRECTORY]}
output=$(realpath -m "${2:-${repo_root}/dist}")
ffmpeg_ref=${RPI_FFMPEG_REF:-f60aae17021abae85b1ba9759e8b2da33f723902}

case "${tag}" in
    v[0-9]*-rpi5.*) ;;
    *) printf 'Unsupported Raspberry Pi release tag: %s\n' "${tag}" >&2; exit 2 ;;
esac

if [[ $(uname -m) != aarch64 ]]; then
    printf 'Release builds must run natively on aarch64, not %s\n' "$(uname -m)" >&2
    exit 2
fi

git -C "${repo_root}" rev-parse --verify "${tag}^{commit}" >/dev/null
release_commit=$(git -C "${repo_root}" rev-parse "${tag}^{commit}")
work=$(mktemp -d "${TMPDIR:-/tmp}/punktfunk-rpi5-release.XXXXXX")
cleanup() { rm -rf -- "${work}"; }
trap cleanup EXIT

source_tree=${work}/source
ffmpeg_source=${work}/rpi-ffmpeg
ffmpeg_prefix=${work}/ffmpeg-prefix
bundle_name="punktfunk-${tag#v}-linux-arm64"
bundle=${work}/${bundle_name}
target_dir=${CARGO_TARGET_DIR:-${repo_root}/target/rpi5-release}

mkdir -p "${source_tree}" "${ffmpeg_prefix}" "${bundle}/lib" "${output}"
git -C "${repo_root}" archive "${tag}" | tar -x -C "${source_tree}"

git clone --filter=blob:none https://github.com/jc-kynesim/rpi-ffmpeg.git "${ffmpeg_source}"
git -C "${ffmpeg_source}" checkout --detach "${ffmpeg_ref}"

(
    cd "${ffmpeg_source}"
    ./configure \
        --prefix="${ffmpeg_prefix}" \
        --disable-static \
        --enable-shared \
        --disable-doc \
        --disable-debug \
        --disable-everything \
        --enable-ffmpeg \
        --enable-avcodec \
        --enable-avformat \
        --enable-avutil \
        --enable-swscale \
        --enable-decoder=hevc \
        --enable-encoder=wrapped_avframe \
        --enable-parser=hevc \
        --enable-demuxer=hevc \
        --enable-protocol=file \
        --enable-muxer=null \
        --enable-filter=null \
        --enable-libdrm \
        --enable-sand \
        --enable-v4l2-request \
        --enable-hwaccel=hevc_v4l2request
    make -j"$(nproc)"
    make install
)

(
    cd "${source_tree}"
    export CARGO_TARGET_DIR="${target_dir}"
    export LIBCLANG_PATH=${LIBCLANG_PATH:-$(llvm-config --libdir)}
    export PKG_CONFIG_PATH="${ffmpeg_prefix}/lib/pkgconfig"
    export LD_LIBRARY_PATH="${ffmpeg_prefix}/lib${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"
    export PUNKTFUNK_BUILD_VERSION="${tag#v}"
    export CARGO_PROFILE_RELEASE_LTO=false
    export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=8
    cargo build --locked --release -p punktfunk-cli --no-default-features
    cargo build --locked --release \
        -p punktfunk-client-session \
        --no-default-features \
        --features ui
)

install -m 0755 "${target_dir}/release/punktfunk" "${bundle}/punktfunk"
install -m 0755 "${target_dir}/release/punktfunk-session" "${bundle}/punktfunk-session"
cp -a \
    "${ffmpeg_prefix}"/lib/libavcodec.so* \
    "${ffmpeg_prefix}"/lib/libavdevice.so* \
    "${ffmpeg_prefix}"/lib/libavfilter.so* \
    "${ffmpeg_prefix}"/lib/libavformat.so* \
    "${ffmpeg_prefix}"/lib/libavutil.so* \
    "${ffmpeg_prefix}"/lib/libswresample.so* \
    "${ffmpeg_prefix}"/lib/libswscale.so* \
    "${bundle}/lib/"
install -m 0644 "${target_dir}/release/libSDL3.so.0" "${bundle}/lib/libSDL3.so.0"

SDL3_LIBRARY="${bundle}/lib/libSDL3.so.0" python3 <<'PY'
import ctypes
import os

sdl = ctypes.CDLL(os.environ["SDL3_LIBRARY"])
sdl.SDL_GetNumVideoDrivers.restype = ctypes.c_int
sdl.SDL_GetVideoDriver.argtypes = [ctypes.c_int]
sdl.SDL_GetVideoDriver.restype = ctypes.c_char_p
drivers = {
    sdl.SDL_GetVideoDriver(index).decode("utf-8")
    for index in range(sdl.SDL_GetNumVideoDrivers())
}
if "wayland" not in drivers:
    raise SystemExit(f"release SDL3 has no Wayland video driver: {sorted(drivers)}")
PY

patchelf --set-rpath "\$ORIGIN/lib" "${bundle}/punktfunk" "${bundle}/punktfunk-session"
install -m 0755 "${repo_root}/packaging/rpi5/install.sh" "${bundle}/install.sh"
install -m 0644 "${repo_root}/packaging/rpi5/README.bundle.md" "${bundle}/README.md"
install -m 0644 "${repo_root}/LICENSE-MIT" "${repo_root}/LICENSE-APACHE" "${bundle}/"
install -m 0644 "${ffmpeg_source}/COPYING.LGPLv2.1" "${bundle}/FFMPEG-LICENSE-LGPL-2.1.txt"
printf '%s\n' "${tag#v}" >"${bundle}/build-version"
printf '%s\n' "${release_commit}" >"${bundle}/fork-commit"
printf '%s\n' "${ffmpeg_ref}" >"${bundle}/rpi-ffmpeg-commit"
printf '%s\n' 'https://github.com/aditi-ab/punktfunk-rpi5' >"${bundle}/source-repository"

LD_LIBRARY_PATH="${bundle}/lib" "${bundle}/punktfunk" --help >/dev/null
if ldd "${bundle}/punktfunk" "${bundle}/punktfunk-session" | grep -q 'not found'; then
    ldd "${bundle}/punktfunk" "${bundle}/punktfunk-session" >&2
    exit 1
fi

archive=${output}/${bundle_name}.tar.gz
tar --sort=name --mtime="@$(git -C "${repo_root}" show -s --format=%ct "${tag}")" \
    --owner=0 --group=0 --numeric-owner -C "${work}" -czf "${archive}" "${bundle_name}"
(
    cd "${output}"
    sha256sum "$(basename "${archive}")" >"$(basename "${archive}").sha256"
)
printf 'Built %s\n' "${archive}"

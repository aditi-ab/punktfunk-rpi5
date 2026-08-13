# LTS builder for the punktfunk HOST .deb — Ubuntu 24.04 (noble), the current Ubuntu LTS.
#
# WHY THIS EXISTS (see packaging/debian/README.md → "Ubuntu 24.04 LTS"):
# The default builder (ci/rust-ci.Dockerfile) is Ubuntu 26.04, so the host .deb it produces bakes
# in a glibc 2.41 floor and a hard `Depends: libavcodec62, …` (FFmpeg 8). Ubuntu 24.04 LTS ships
# glibc 2.39 and FFmpeg 6.1 (libavcodec60), so that .deb is uninstallable there — apt reports the
# deps as "too recent". Building the host on 24.04 instead lowers the glibc floor to 2.39 (the
# binary then runs on 24.04 → 26.04), and the ONE library 24.04 is too old for — FFmpeg — is built
# from source here and BUNDLED into the .deb (packaging/debian/build-deb.sh, BUNDLE_FFMPEG=1), so
# the package no longer depends on the distro's libav* at all. Everything else the host links
# (PipeWire, Wayland, xkbcommon, GL/EGL/GBM, Vulkan; opus is vendored via cmake) is soname-compatible
# on 24.04, so this ONE universal host .deb replaces the 26.04-built one for every Ubuntu user.
#
# libcuda is deliberately NOT provided: the host dlopen's libcuda.so.1 at runtime (pf-zerocopy /
# pf-encode) and never link-imports it, so — unlike the full-workspace rust-ci image, which builds
# tests that DO link a cuda stub — this host-only build needs no NVIDIA driver package. NVENC/EGL
# come from whatever driver the target runs, out of band.
#
# Rebuilt+pushed by .gitea/workflows/docker.yml (matrix: punktfunk-rust-ci-noble); consumed by the
# `build-publish-host` job in .gitea/workflows/deb.yml. Bootstrap: like rust-ci, the first deb.yml
# run after this image is added uses the image from a PRIOR docker.yml push — seed it once manually
# (docker build -f ci/rust-ci-noble.Dockerfile -t … ci && docker push) before the host job can run.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    # toolchain + bindgen; nodejs runs the JS actions (checkout/cache); unzip for the rustup installer's deps
    build-essential clang libclang-dev pkg-config cmake git curl ca-certificates nodejs unzip \
    # mold: link-phase accelerator (sccache cannot cache linking). This image links the release
    # host + encode worker on every deb.yml run. Wired via cargo-config-mold.toml below.
    mold \
    # .deb assembly: dpkg-shlibdeps/dpkg-deb; patchelf repoints the binary's rpath at the bundled FFmpeg
    dpkg-dev patchelf \
    # FFmpeg 8 build deps: nasm (asm), VAAPI (libva/libdrm) so the built libav* keep the AMD/Intel
    # encode backend the host auto-selects; zlib (libavformat). NVENC needs only headers (below), dlopen'd.
    nasm libva-dev libdrm-dev zlib1g-dev \
    # host link deps present on 24.04 with sonames compatible up to 26.04
    libpipewire-0.3-dev libwayland-dev libxkbcommon-dev \
    libgl-dev libegl-dev libgbm-dev libvulkan-dev \
    && rm -rf /var/lib/apt/lists/*

# --- FFmpeg 8 from source -> /opt/ffmpeg (shared libs + .pc files) ----------------------------------
# libavcodec.so.62, matching the 26.04 line's soname so the host behaves identically. This is an
# LGPL build (no --enable-gpl / --enable-nonfree) so bundling the .so's into an MIT/Apache .deb stays
# license-clean — LGPL's relink clause is satisfied by dynamic linking, and the only encoders the host
# calls (h264/hevc/av1 _nvenc + _vaapi, plus scale_vaapi/hwmap filters; software H.264 fallback is the
# BSD-2 openh264 crate, NOT FFmpeg libx264) are all LGPL-compatible.
# Sourced from the official FFmpeg GitHub mirror by release tag, NOT ffmpeg.org: the CI build network
# can't reach ffmpeg.org (curl times out) but reaches github.com fine. The `nX.Y` tag pins the version
# (n8.0 -> libavcodec 62); bump it to move FFmpeg. Immutable-tag clone, so no separate checksum needed.
#
# STAYING ON 8.0 THROUGH THE 2026-08-08 FFmpeg-9 BUMP IS DELIBERATE. `ffmpeg-next` moved to 9, but a
# crate major is a CEILING (ffmpeg-sys-next 9 spans libavcodec 56..63), so an 8.0 tree still compiles
# — and this .deb is the one package with NO exposure to the soname break that motivated the bump: it
# BUNDLES these libs into /usr/lib/punktfunk-host behind an rpath and strips the libav* sonames from
# its Depends, so nothing the user's apt does can move them underneath it. Bumping this tag would
# re-qualify the encode stack for every Ubuntu user and buy none of them anything, so it is its own
# change — and it drags NVHDR_TAG and the soname assertion below along with it.
ARG FFMPEG_TAG=n8.0
# nv-codec-headers must MATCH the FFmpeg version: its `master` is NVENC SDK 13, which renamed
# NV_ENC_CLOCK_TIMESTAMP_SET.countingType -> countingTypeLSB and won't compile against FFmpeg 8.0's
# nvenc.c. Pin the last SDK-12 tag (has the field FFmpeg 8.0 expects). Bump alongside FFMPEG_TAG.
ARG NVHDR_TAG=n12.2.72.0
RUN set -eux; \
    # nv-codec-headers: the NVENC/NVDEC headers FFmpeg's --enable-nvenc needs (headers only, no lib —
    # the driver is dlopen'd at runtime). Installs ffnvcodec.pc under /usr/local/lib/pkgconfig.
    git clone --depth 1 --branch "$NVHDR_TAG" https://github.com/FFmpeg/nv-codec-headers.git /tmp/nvhdr; \
    make -C /tmp/nvhdr install PREFIX=/usr/local; \
    git clone --depth 1 --branch "$FFMPEG_TAG" https://github.com/FFmpeg/FFmpeg.git /tmp/ffmpeg; \
    cd /tmp/ffmpeg; \
    PKG_CONFIG_PATH=/usr/local/lib/pkgconfig ./configure \
      --prefix=/opt/ffmpeg \
      --enable-shared --disable-static \
      --disable-doc --disable-programs --disable-debug \
      --enable-nvenc --enable-vaapi \
      --extra-cflags=-I/usr/local/include --extra-ldflags=-L/usr/local/lib; \
    make -j"$(nproc)"; make install; \
    cd /; rm -rf /tmp/ffmpeg /tmp/nvhdr; \
    # sanity: the soname we expect to bundle (libavcodec.so.62 on FFmpeg 8)
    test -e /opt/ffmpeg/lib/libavcodec.so.62

# ffmpeg-sys-next discovers FFmpeg via pkg-config; point it at the bundled build. PKG_CONFIG_PATH is
# PREPENDED to pkg-config's default dirs (not a replacement — that's PKG_CONFIG_LIBDIR), so PipeWire /
# Wayland / libva / … still resolve from the system. FFMPEG_PREFIX is read by build-deb.sh's bundler.
ENV PKG_CONFIG_PATH=/opt/ffmpeg/lib/pkgconfig \
    FFMPEG_PREFIX=/opt/ffmpeg

# Toolchain shared across CI users (jobs may run as different uids).
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal \
        --component rustfmt,clippy \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME" \
    && rustc --version && cargo clippy --version && cargo fmt --version

# Shared compile cache: jobs set RUSTC_WRAPPER=sccache (backend = RustFS S3 on the LAN,
# see .gitea/workflows — the env lives there so dev use of this image stays uncached).
# musl build: one static binary serves the Ubuntu and Fedora images alike.
ARG SCCACHE_VERSION=0.10.0
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar -xz --wildcards --strip-components=1 -C /usr/local/bin '*/sccache' \
    && sccache --version

# Link x86_64 with mold — see cargo-config-mold.toml's header for the rustflags traps, and
# rust-ci.Dockerfile for why the `mold --version` assertion sits next to the COPY.
# ⚠ This does NOT touch the from-source FFmpeg built above: that is a plain ./configure && make in
# an earlier layer, linked by GNU ld exactly as before. Only cargo's links move to mold.
COPY cargo-config-mold.toml /usr/local/cargo/config.toml
RUN mold --version && test -r /usr/local/cargo/config.toml

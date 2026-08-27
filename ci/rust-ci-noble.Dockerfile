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
# (n8.0 -> libavcodec 62); bump it to move FFmpeg — together with the commit SHA it is pinned to below.
#
# STAYING ON 8.0 THROUGH THE 2026-08-08 FFmpeg-9 BUMP IS DELIBERATE. `ffmpeg-next` moved to 9, but a
# crate major is a CEILING (ffmpeg-sys-next 9 spans libavcodec 56..63), so an 8.0 tree still compiles
# — and this .deb is the one package with NO exposure to the soname break that motivated the bump: it
# BUNDLES these libs into /usr/lib/punktfunk-host behind an rpath and strips the libav* sonames from
# its Depends, so nothing the user's apt does can move them underneath it. Bumping this tag would
# re-qualify the encode stack for every Ubuntu user and buy none of them anything, so it is its own
# change — and it drags NVHDR_TAG and the soname assertion below along with it.
ARG FFMPEG_TAG=n8.0
# The COMMIT that tag points at. A git tag is MUTABLE — upstream can move one, and unlike a branch
# nobody would notice — and these .so's are BUNDLED into the host .deb every Ubuntu user installs.
# The clone below asserts HEAD against this, so a moved tag fails the build loudly instead of
# shipping. Same shape as the bun/sccache sha256 pins: a mismatch stops the build, it does not
# silently "fix" itself. Bump alongside FFMPEG_TAG:
#   git ls-remote --tags https://github.com/FFmpeg/FFmpeg.git 'refs/tags/<new-tag>^{}'
# Take the `^{}` line: these are ANNOTATED tags, so the bare ref is the tag OBJECT and the peeled
# `^{}` is the commit — the commit is what a clone leaves at HEAD, and what this compares against.
ARG FFMPEG_SHA=140fd653aed8cad774f991ba083e2d01e86420c7
# nv-codec-headers must MATCH the FFmpeg version: its `master` is NVENC SDK 13, which renamed
# NV_ENC_CLOCK_TIMESTAMP_SET.countingType -> countingTypeLSB and won't compile against FFmpeg 8.0's
# nvenc.c. Pin the last SDK-12 tag (has the field FFmpeg 8.0 expects). Bump alongside FFMPEG_TAG.
ARG NVHDR_TAG=n12.2.72.0
# Commit for NVHDR_TAG, asserted after checkout — see FFMPEG_SHA above for why and how to bump:
#   git ls-remote --tags https://github.com/FFmpeg/nv-codec-headers.git 'refs/tags/<new-tag>^{}'
ARG NVHDR_SHA=c69278340ab1d5559c7d7bf0edf615dc33ddbba7
RUN set -eux; \
    # nv-codec-headers: the NVENC/NVDEC headers FFmpeg's --enable-nvenc needs (headers only, no lib —
    # the driver is dlopen'd at runtime). Installs ffnvcodec.pc under /usr/local/lib/pkgconfig.
    git clone --depth 1 --branch "$NVHDR_TAG" https://github.com/FFmpeg/nv-codec-headers.git /tmp/nvhdr; \
    test "$(git -C /tmp/nvhdr rev-parse HEAD)" = "$NVHDR_SHA" \
      || { echo "error: nv-codec-headers $NVHDR_TAG is not $NVHDR_SHA — tag moved upstream" >&2; exit 1; }; \
    make -C /tmp/nvhdr install PREFIX=/usr/local; \
    git clone --depth 1 --branch "$FFMPEG_TAG" https://github.com/FFmpeg/FFmpeg.git /tmp/ffmpeg; \
    test "$(git -C /tmp/ffmpeg rev-parse HEAD)" = "$FFMPEG_SHA" \
      || { echo "error: FFmpeg $FFMPEG_TAG is not $FFMPEG_SHA — tag moved upstream" >&2; exit 1; }; \
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
# Checked by SHA-256, like the bun pin: sccache is RUSTC_WRAPPER, so it sits in front of every
# rustc invocation that produces a SHIPPED binary. Bump SCCACHE_VERSION and SCCACHE_SHA together —
# upstream publishes the sum as <asset>.tar.gz.sha256 next to the release asset.
ARG SCCACHE_VERSION=0.10.0
ARG SCCACHE_SHA=1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b
RUN curl -fsSL -o /tmp/sccache.tar.gz \
      "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    && echo "${SCCACHE_SHA}  /tmp/sccache.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/sccache.tar.gz --wildcards --strip-components=1 -C /usr/local/bin '*/sccache' \
    && rm -f /tmp/sccache.tar.gz \
    && sccache --version

# Link x86_64 with mold — see cargo-config-mold.toml's header for the rustflags traps, and
# rust-ci.Dockerfile for why the `mold --version` assertion sits next to the COPY.
# ⚠ This does NOT touch the from-source FFmpeg built above: that is a plain ./configure && make in
# an earlier layer, linked by GNU ld exactly as before. Only cargo's links move to mold.
COPY cargo-config-mold.toml /usr/local/cargo/config.toml
RUN mold --version && test -r /usr/local/cargo/config.toml

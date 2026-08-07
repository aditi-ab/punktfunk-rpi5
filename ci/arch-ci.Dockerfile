# Arch CI builder: base-devel + every dependency arch.yml's two makepkg legs used to
# pacman-install per run (~1 GB of mirror traffic each time) + bun + sccache + nodejs
# (JS actions exec node INSIDE the job container — the same lesson as android-ci).
# Content-keyed and rebuilt only when the ci/ tree changes (docker.yml `builders`).
#
#   docker build -f ci/arch-ci.Dockerfile -t punktfunk-arch-ci ci
#
# ROLLING-RELEASE TRADEOFF, on purpose: packages now build against the Arch snapshot
# from the last image rebuild instead of a fresh -Syu per run. That is the same staleness
# the gamescope cache already embraces ("a stale binary against newer system libs is the
# same risk the distro's own package carries between rebuilds"), and any ci/ edit — or
# bumping the date in this line (refreshed: 2026-07-29) — re-keys and re-snapshots it.
FROM docker.io/library/archlinux:base-devel

# One transaction: the main build/runtime deps (first list) + the gamescope companion's
# deps (second list) — both copied verbatim from what arch.yml installed in-job, where
# they now no-op as `--needed` guards.
# vulkan-headers rides the first list only because arch.yml's copy does; the package it actually
# serves is the gamescope companion (packaging/gamescope/PKGBUILD makedepends). punktfunk itself
# needs no system Vulkan headers — pyrowave-sys bindgens its own vendored copy and ash dlopens the
# loader — but arch.yml builds gamescope with `makepkg -d`, so an absent makedepend would not be
# reported as a missing dependency, only as a compile failure. Keep it.
RUN pacman -Syu --noconfirm --needed \
        git nodejs rust clang cmake ninja nasm pkgconf python vulkan-headers \
        gtk4 libadwaita sdl3 ffmpeg pipewire wayland libxkbcommon opus libei \
        mesa libglvnd unzip libarchive \
        glslang libcap libdrm libinput libx11 libxcomposite libxdamage libxext \
        libxmu libxrender libxres libxtst libxxf86vm libavif libdecor \
        hwdata luajit seatd sdl2-compat vulkan-icd-loader \
        xcb-util-errors xcb-util-wm xorg-xwayland \
        meson glm wayland-protocols benchmark libxcursor \
    && pacman -Scc --noconfirm

# bun builds the punktfunk-web console + the punktfunk-scripting runner AND is vendored
# as their runtime (PF_WITH_WEB=1 / PF_WITH_SCRIPTING=1); it's AUR-only on Arch, so
# bootstrap the official binary — once, here, instead of per run.
RUN curl -fsSL https://bun.sh/install | bash \
    && install -m0755 /root/.bun/bin/bun /usr/local/bin/bun \
    && rm -rf /root/.bun \
    && bun --version

# Shared compile cache: jobs set RUSTC_WRAPPER=sccache (backend = RustFS S3 on the LAN,
# see .gitea/workflows — the env lives there so dev use of this image stays uncached).
ARG SCCACHE_VERSION=0.10.0
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar -xz --wildcards --strip-components=1 -C /usr/local/bin '*/sccache' \
    && sccache --version

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
# bumping the date in this line (refreshed: 2026-08-08) — re-keys and re-snapshots it.
#
# ⚠ That staleness has a sharp edge, and 2026-08-08 is why the date above moved: this snapshot is
# what decides which FFmpeg the HOST links, and arch.yml deliberately runs no -Syu, so the builder
# stayed frozen on ffmpeg 8 (libavcodec 62) even after Arch shipped 2:9.0-5 (libavcodec 63) to
# every user. A canary built from the old snapshot therefore CANNOT satisfy the soname dep that
# packaging/arch/PKGBUILD now derives from the link (libavcodec.so=62-64 against a box that has
# 63-64), so it would simply refuse to install rather than start. Re-keying this image is the step
# that makes the ffmpeg-9 bump actually reach the package — a Cargo.toml bump alone does nothing
# here. Whenever Arch moves to an FFmpeg major, bump the date in the same commit.
#
# ⚠ AND KNOW WHY THAT WAS NOT ENOUGH: bumping this date only helps once docker.yml has actually
# republished the image, and nothing sequences the two workflows. v0.25.0 was tagged four minutes
# after the ffmpeg-9 merge, so the release build still pulled the FFmpeg-8 `:latest` and published
# a punktfunk-host that no up-to-date Arch box could install — which blocks the user's ENTIRE
# `pacman -Syu`, not just our package. arch.yml therefore no longer trusts this image on that one
# axis: it compares the builder's libav sonames against the repos before building (and `-Syu`s
# itself if they differ), and refuses to publish anything a pristine-db `pacman -U --print` says
# is unsatisfiable. This file staying current is still the CHEAP path — those guards are the
# backstop, not the plan.
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
        # mold: link-phase accelerator (sccache cannot cache linking). makepkg links the release
        # host, client, worker and tray on every arch.yml run. Wired via cargo-config-mold.toml
        # below. It does NOT affect the gamescope companion leg — that is meson + its own linker,
        # and its `-static-libstdc++` link is untouched.
        mold \
        # bun builds the punktfunk-web console + the punktfunk-scripting runner AND is vendored as
        # their runtime (PF_WITH_WEB=1 / PF_WITH_SCRIPTING=1) — so these bytes end up inside the
        # package arch.yml signs and publishes. Arch ships bun in [extra], so take the
        # pacman-signed package (pacman verifies package signatures by default) instead of piping
        # bun.sh's installer into root's shell, which would be upstream code choosing them. Same
        # call as arch.yml's bootstrap guard. It rides THIS transaction rather than a later layer
        # on purpose: -Syu refreshes the db in the same step that installs, so a cache-hit rebuild
        # can never resolve bun against a stale snapshot the mirrors no longer carry.
        bun \
    && pacman -Scc --noconfirm \
    && bun --version

# Shared compile cache: jobs set RUSTC_WRAPPER=sccache (backend = RustFS S3 on the LAN,
# see .gitea/workflows — the env lives there so dev use of this image stays uncached).
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

# CARGO_HOME is declared here only so this image agrees with what arch.yml already sets at job
# level (and so `cargo` finds the config below when the image is used by hand). The workflow still
# passes CARGO_HOME explicitly across the `sudo -u builder env …` boundary, which strips ambient
# env — that is why the C/C++ sccache wiring has to be re-exported there by name while THIS file,
# being a file, crosses the boundary for free.
ENV CARGO_HOME=/usr/local/cargo
RUN mkdir -p /usr/local/cargo && chmod -R a+w /usr/local/cargo

# Link x86_64 with mold — see cargo-config-mold.toml's header for the rustflags traps, and
# rust-ci.Dockerfile for why the `mold --version` assertion sits next to the COPY.
COPY cargo-config-mold.toml /usr/local/cargo/config.toml
RUN mold --version && test -r /usr/local/cargo/config.toml

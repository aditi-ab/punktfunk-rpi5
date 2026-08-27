# CI builder for the punktfunk RPM. The Fedora version is parameterized so one Dockerfile
# serves every target whose ffmpeg soname must match: Fedora 43 == Bazzite's base (group
# "bazzite"), Fedora 44 == the Fedora KDE spin (group "fedora-44"). The RPM's auto-generated
# library Requires (e.g. libavcodec.so.NN) pin to exactly what the chosen base — and thus the
# target — ships. Used by .gitea/workflows/rpm.yml; built+pushed by .gitea/workflows/docker.yml.
#
#   docker build --build-arg FEDORA_VERSION=43 -f ci/fedora-rpm.Dockerfile -t punktfunk-fedora-rpm ci
#   docker build --build-arg FEDORA_VERSION=44 -f ci/fedora-rpm.Dockerfile -t punktfunk-fedora44-rpm ci
#
# Mirrors ci/rust-ci.Dockerfile (the Ubuntu workspace builder) for the rpmbuild side.
ARG FEDORA_VERSION=43
FROM fedora:${FEDORA_VERSION}

# RPM Fusion (free + nonfree) provides the NVENC-capable ffmpeg-devel the host links against.
RUN dnf -y install \
      "https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm" \
      "https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$(rpm -E %fedora).noarch.rpm" \
  && dnf -y install \
      # rpmbuild + source-tarball tooling; nodejs runs the Gitea Actions JS (checkout/cache) only
      # — the punktfunk-web console builds AND runs on bun (installed below); unzip extracts the
      # pinned bun zip.
      rpm-build rpmdevtools systemd-rpm-macros git tar gzip nodejs unzip \
      # build toolchain + bindgen
      gcc gcc-c++ clang clang-devel cmake nasm pkgconf-pkg-config curl ca-certificates \
      # mold: link-phase accelerator (sccache cannot cache linking). This image links the release
      # host, client, worker and tray on every rpm.yml run, TWICE per push (f43 + f44). Wired via
      # cargo-config-mold.toml below. Note the linker DRIVER is unchanged — still gcc, so Fedora's
      # default `-Wl,--build-id` still reaches the link and rpmbuild's debuginfo extraction (which
      # hard-requires a build-id) behaves exactly as before; mold implements --build-id natively.
      mold \
      # ffmpeg (NVENC), capture/audio/display link deps
      ffmpeg-devel pipewire-devel wayland-devel libxkbcommon-devel opus-devel \
      mesa-libGL-devel mesa-libgbm-devel \
      # punktfunk-client link deps (GTK4 shell + SDL3 gamepads)
      gtk4-devel libadwaita-devel SDL3-devel \
      # No vulkan-headers: nothing in the workspace compiles against the system Vulkan headers
      # (pyrowave-sys bindgens its own vendored copy; host and client both reach Vulkan through
      # ash, which dlopens the loader), and packaging/rpm/punktfunk.spec BuildRequires none.
      # rpm.yml's HDR gamescope leg needs them and pulls them with `dnf builddep gamescope`.
  && dnf clean all

# bun — both the BUILD tool and the RUNTIME for the punktfunk-web console (`bun run build` -> the
# Nitro `bun`-preset .output, served by `Bun.serve` with TLS — HTTP/1.1 over TLS). The
# RPM vendors THIS bun binary. Not in Fedora repos; install the official standalone binary to a
# system PATH dir so the rpmbuild `%build`/`%install` (run as any uid) find it.
#
# A PINNED release asset, checked by SHA-256 — never `curl https://bun.sh/install | bash`. The spec
# VENDORS this very binary into punktfunk-web, so the installer would be upstream code choosing
# bytes rpm.yml then signs with RPM_GPG_PRIVATE_KEY. ONE bun across the repo: same version, asset
# and sum as rpm.yml, deb.yml and rust-ci.Dockerfile — bump BUN_VERSION and BUN_SHA together (the
# sums are in the release's SHASUMS256.txt). `-baseline` on purpose: it needs no AVX2, so the bun
# we ship starts on every x86-64 box — something the auto-detecting installer never promised, since
# it reads the BUILDER's CPU, not the user's.
ARG BUN_VERSION=1.3.14
ARG BUN_SHA=a063908ae08b7852ca10939bbdc6ceed3ddabce8fb9402dce83d65d73b36e6c7
RUN curl -fsSL -o /tmp/bun.zip \
      "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-x64-baseline.zip" \
    && echo "${BUN_SHA}  /tmp/bun.zip" | sha256sum -c - \
    && unzip -q -o -j /tmp/bun.zip '*/bun' -d /tmp \
    && install -m0755 /tmp/bun /usr/local/bin/bun \
    && rm -f /tmp/bun.zip /tmp/bun \
    && bun --version

# libcuda link stub — the zerocopy path links a fixed set of cuXxx driver symbols, but CI has
# no GPU and never RUNS CUDA. Rather than drag in the NVIDIA userspace stack, synthesize a stub
# libcuda.so.1 that just defines those symbols (the SAME approach the Ubuntu image takes with the
# real driver lib, minus the driver). On Bazzite the real driver provides libcuda.so.1 at runtime.
# The symbol list is `nm -D --undefined-only` of the built host binary; a new cu* call would fail
# the link with a clear "undefined reference", flagging this list to update.
RUN set -eux; : > /tmp/cuda_stub.c; \
    for s in cuCtxCreate_v2 cuCtxSetCurrent cuCtxSynchronize cuDestroyExternalMemory \
             cuDeviceGet cuExternalMemoryGetMappedBuffer cuGraphicsGLRegisterImage \
             cuGraphicsMapResources cuGraphicsSubResourceGetMappedArray cuGraphicsUnmapResources \
             cuGraphicsUnregisterResource cuImportExternalMemory cuInit cuMemAllocPitch_v2 \
             cuMemcpy2D_v2 cuMemFree_v2; do \
      echo "int $s(void){return 0;}" >> /tmp/cuda_stub.c; \
    done; \
    gcc -shared -fPIC -Wl,-soname,libcuda.so.1 -o /usr/lib64/libcuda.so.1 /tmp/cuda_stub.c; \
    ln -sf libcuda.so.1 /usr/lib64/libcuda.so; \
    rm -f /tmp/cuda_stub.c; ldconfig; test -e /usr/lib64/libcuda.so

# Rustup (not Fedora's packaged rust) so rust-toolchain.toml's pinned channel resolves, matching
# the Ubuntu builder. Shared location so jobs running as any uid can use it.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME" \
    && rustc --version && cargo --version

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
COPY cargo-config-mold.toml /usr/local/cargo/config.toml
RUN mold --version && test -r /usr/local/cargo/config.toml

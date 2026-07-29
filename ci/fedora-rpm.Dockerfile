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
      # — the punktfunk-web console builds AND runs on bun (installed below); unzip is for the bun
      # installer.
      rpm-build rpmdevtools systemd-rpm-macros git tar gzip nodejs unzip \
      # build toolchain + bindgen
      gcc gcc-c++ clang clang-devel cmake nasm pkgconf-pkg-config curl ca-certificates \
      # ffmpeg (NVENC), capture/audio/display link deps
      ffmpeg-devel pipewire-devel wayland-devel libxkbcommon-devel opus-devel \
      mesa-libGL-devel mesa-libgbm-devel \
      # punktfunk-client link deps (GTK4 shell + SDL3 gamepads)
      gtk4-devel libadwaita-devel SDL3-devel \
      # pf-ffvk bindgen over libavutil/hwcontext_vulkan.h needs <vulkan/vulkan.h>
      vulkan-headers \
  && dnf clean all

# bun — both the BUILD tool and the RUNTIME for the punktfunk-web console (`bun run build` -> the
# Nitro `bun`-preset .output, served by `Bun.serve` with TLS — HTTP/1.1 over TLS). The
# RPM vendors THIS bun binary. Not in Fedora repos; install the official standalone binary to a
# system PATH dir so the rpmbuild `%build`/`%install` (run as any uid) find it.
RUN curl -fsSL https://bun.sh/install | bash \
    && install -m0755 /root/.bun/bin/bun /usr/local/bin/bun \
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
ARG SCCACHE_VERSION=0.10.0
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar -xz --wildcards --strip-components=1 -C /usr/local/bin '*/sccache' \
    && sccache --version

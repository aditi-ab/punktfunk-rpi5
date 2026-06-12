# CI builder for the Rust workspace — Ubuntu 26.04 to match the dev/host boxes
# (FFmpeg 8 / libavcodec 62, PipeWire 1.6). Used by .gitea/workflows/ci.yml as the job
# container; rebuilt+pushed by .gitea/workflows/docker.yml.
#
#   docker build -f ci/rust-ci.Dockerfile -t punktfunk-rust-ci ci
#
# The workspace links real system libs at build time (CLAUDE.md "Pinned crate facts"):
# FFmpeg, PipeWire, Opus, GL/EGL/GBM — and libcuda, which has no real driver here; the
# zerocopy path only needs the symbols at link time, so a driver userspace package plus a
# libcuda.so -> libcuda.so.1 symlink stands in for it (CI never executes the CUDA path).
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    # toolchain + bindgen; nodejs runs the JS actions (checkout/cache) inside this container
    build-essential clang libclang-dev pkg-config cmake git curl ca-certificates nodejs \
    # ffmpeg-next 8 (system FFmpeg 8 / libavcodec 62 on 26.04)
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev \
    libavdevice-dev \
    # capture / audio / display stacks (+xkbcommon for the wlr input backend)
    libpipewire-0.3-dev libopus-dev libwayland-dev libxkbcommon-dev \
    # zerocopy link deps (GL via libglvnd, EGL, GBM)
    libgl-dev libegl-dev libgbm-dev \
    # punktfunk-client-linux (GTK4/libadwaita shell, SDL3 gamepads)
    libgtk-4-dev libadwaita-1-dev libsdl3-dev \
    && rm -rf /var/lib/apt/lists/*

# libcuda link stub: the NVIDIA userspace library (no kernel module needed) provides
# every cuXxx symbol. On 26.04 the package already ships the libcuda.so dev symlink;
# -sf keeps this idempotent if a future package drops it again.
RUN apt-get update \
    && apt-get install -y --no-install-recommends libnvidia-compute-580-server \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf libcuda.so.1 /usr/lib/x86_64-linux-gnu/libcuda.so \
    && test -e /usr/lib/x86_64-linux-gnu/libcuda.so.1

# Toolchain shared across CI users (jobs may run as different uids).
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal \
        --component rustfmt,clippy \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME" \
    && rustc --version && cargo clippy --version && cargo fmt --version

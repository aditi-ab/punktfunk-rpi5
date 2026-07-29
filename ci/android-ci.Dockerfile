# Android CI builder: JDK 21 + Android SDK/NDK/CMake + pinned Rust with the three shipping
# Android targets + cargo-ndk + sccache. Everything android.yml used to download per run
# (~3 GB of NDK + SDK packages from Google, plus a from-source cargo-ndk build) is baked
# here instead; the image is content-keyed and rebuilt only when the ci/ tree changes
# (docker.yml `builders`).
#
#   docker build -f ci/android-ci.Dockerfile -t punktfunk-android-ci ci
#
# Version pins mirror what android.yml installed via sdkmanager: AGP 9.3 wants JDK 17–21;
# cmake;3.22.1 because kit/build.gradle.kts prepends $ANDROID_SDK/cmake/3.22.1/bin to PATH
# for cargo-ndk's audiopus_sys (libopus) CMake build; platforms;android-37 is deliberately
# absent (AGP auto-downloads it if a build ever needs it — same note as the old workflow).
FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git unzip zip python3 openjdk-21-jdk-headless \
        build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

ENV JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64

# Android SDK: cmdline-tools must land under cmdline-tools/latest for sdkmanager to
# find its own root.
ENV ANDROID_HOME=/opt/android-sdk \
    ANDROID_SDK_ROOT=/opt/android-sdk
ARG CMDLINE_TOOLS=13114758
RUN mkdir -p "$ANDROID_HOME/cmdline-tools" \
    && curl -fsSL -o /tmp/clt.zip "https://dl.google.com/android/repository/commandlinetools-linux-${CMDLINE_TOOLS}_latest.zip" \
    && unzip -q /tmp/clt.zip -d "$ANDROID_HOME/cmdline-tools" \
    && mv "$ANDROID_HOME/cmdline-tools/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest" \
    && rm /tmp/clt.zip
ENV PATH=$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH
RUN yes | sdkmanager --licenses >/dev/null \
    && sdkmanager "platform-tools" "platforms;android-36" "build-tools;37.0.0" \
        "ndk;30.0.14904198" "cmake;3.22.1" \
    && chmod -R a+rX "$ANDROID_HOME"

# Toolchain shared across CI users (jobs may run as different uids) — same shape as
# rust-ci.Dockerfile, plus the Android cross targets and cargo-ndk. The registry/git
# download caches are stripped after the cargo-ndk install: jobs restore those from the
# shared actions cache, and baking them would only bloat every pull.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal \
    && rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android \
    && cargo install cargo-ndk --locked \
    && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git" \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME" \
    && rustc --version && cargo-ndk --version

# Shared compile cache: jobs set RUSTC_WRAPPER=sccache (backend = RustFS S3 on the LAN,
# see .gitea/workflows — the env lives there so dev use of this image stays uncached).
ARG SCCACHE_VERSION=0.10.0
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar -xz --wildcards --strip-components=1 -C /usr/local/bin '*/sccache' \
    && sccache --version

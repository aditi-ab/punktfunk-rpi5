#!/bin/sh
# Ensure `sccache` is on PATH, self-healing on a runner/image that does not already carry it.
#
# WHY THIS EXISTS: this block was copy-pasted into ten jobs across six workflows (ci.yml x2,
# deb.yml x3, rpm.yml, bench.yml, apple.yml x2, the Apple release leg), in two dialects that had
# already drifted apart — the Linux copies pass `--wildcards` to GNU tar, the macOS copies must NOT
# (bsdtar globs by default and rejects the flag). One copy per platform, here, so a version bump or
# a mirror change is one edit rather than ten.
#
# The builder images (ci/*.Dockerfile) BAKE sccache, so on Linux this is a no-op in the normal case;
# it stays because a job runs against the image from the PREVIOUS push (docker.yml's bootstrap lag),
# and the macOS runner is a persistent host with no image at all.
#
# POSIX sh on purpose: Gitea's act_runner executes a step's `run:` under `sh -e` (dash) inside the
# Linux job containers — no bashisms, no process substitution (see the shader-gate note in ci.yml
# for what that cost the last time someone assumed bash).
#
# Usage:  sh scripts/ci/ensure-sccache.sh
set -e

# Keep in step with the ARG SCCACHE_VERSION in ci/*.Dockerfile — the images bake this same version,
# and a job that heals to a DIFFERENT one would quietly split the shared cache's key universe in two
# (sccache's cache keys are not versioned across incompatible releases).
SCCACHE_VERSION="${SCCACHE_VERSION:-0.10.0}"

if command -v sccache >/dev/null 2>&1; then
    sccache --version
    exit 0
fi

BASE="https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}"

case "$(uname -s)" in
    Darwin)
        # The macOS runner is a LaunchAgent in the user's Aqua session, not root — install into the
        # user prefix. ~/.local/bin is already on the runner daemon's PATH; GITHUB_PATH is
        # belt-and-braces for the steps that follow in THIS job.
        DEST="$HOME/.local/bin"
        mkdir -p "$DEST"
        case "$(uname -m)" in
            arm64|aarch64) ARCH=aarch64-apple-darwin ;;
            *)             ARCH=x86_64-apple-darwin ;;
        esac
        # bsdtar globs by default and does not accept --wildcards.
        curl -fsSL "$BASE/sccache-v${SCCACHE_VERSION}-${ARCH}.tar.gz" \
            | tar -xz --strip-components=1 -C "$DEST" '*/sccache'
        chmod 0755 "$DEST/sccache"
        PATH="$DEST:$PATH"
        export PATH
        if [ -n "${GITHUB_PATH:-}" ]; then
            echo "$DEST" >> "$GITHUB_PATH"
        fi
        ;;
    *)
        # Linux job containers run as root; /usr/local/bin is on PATH already, so no GITHUB_PATH
        # dance is needed. The musl build is static — one binary serves the Ubuntu, Fedora and Arch
        # images alike.
        DEST=/usr/local/bin
        curl -fsSL "$BASE/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
            | tar -xz --wildcards --strip-components=1 -C "$DEST" '*/sccache'
        chmod 0755 "$DEST/sccache"
        ;;
esac

sccache --version

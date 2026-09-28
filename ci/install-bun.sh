#!/bin/sh
# Install the bun pinned in ci/bun.env to /usr/local/bin, checked by SHA-256.
#
# Never `curl https://bun.sh/install | bash`: the packages vendor this binary, so an installer
# would be upstream code choosing bytes a signing job then publishes. `-baseline` needs no AVX2,
# so the bun we ship starts on every x86-64 box, not only on ones like the builder.
#
# The builder images run this at build time. A job runs it again because it may be on the
# previous image; with the pinned bun already on PATH it only prints the version.
#
# Usage: sh ci/install-bun.sh   (needs curl, unzip and sha256sum)
set -eu
. "$(dirname "$0")/bun.env"

if [ "$(bun --version 2>/dev/null || true)" != "$BUN_VERSION" ]; then
  curl -fsSL -o /tmp/bun.zip \
    "https://github.com/oven-sh/bun/releases/download/bun-v$BUN_VERSION/bun-linux-x64-baseline.zip"
  echo "$BUN_SHA  /tmp/bun.zip" | sha256sum -c -
  unzip -q -o -j /tmp/bun.zip '*/bun' -d /tmp
  install -m0755 /tmp/bun /usr/local/bin/bun
  rm -f /tmp/bun.zip /tmp/bun
fi
bun --version

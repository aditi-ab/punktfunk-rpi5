#!/usr/bin/env bash
# Regenerate THIRD-PARTY-NOTICES.txt for the Rust workspace.
#
# Prefers `cargo about` (full, network-augmented license harvest; see about.toml) and falls back to
# the dependency-free offline generator (scripts/gen-third-party-notices.py, reads the cargo registry
# cache). Run this when the dependency tree changes; CI also runs it before packaging.
#
# Usage: scripts/gen-third-party-notices.sh [output-file]
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:-THIRD-PARTY-NOTICES.txt}"

if command -v cargo-about >/dev/null 2>&1; then
    echo "==> cargo about generate -> $OUT" >&2
    cargo about generate about.hbs --output-file "$OUT"
else
    echo "==> cargo-about not installed; using offline fallback" >&2
    echo "    (install the full generator with: cargo install cargo-about)" >&2
    python3 scripts/gen-third-party-notices.py --out "$OUT"
fi
echo "==> wrote $OUT" >&2

# Regenerate the per-client in-tree copies. EVERY client has one now, because every client SHOWS
# it: the mobile apps bundle theirs as a resource/asset for their Acknowledgements screen, and the
# two desktop shells `include_str!` theirs onto their Licenses page (the MSIX and the client .deb
# ship the file as well).
#
# These are GENERATED, not copied. They used to be the workspace-wide file, which attributed to
# every client every crate anything in this repo links: FFmpeg, the NVENC SDK, GTK4, windows-rs.
# The Apple app links ONE Rust crate (punktfunk-core, through PunktfunkCore.xcframework — see
# scripts/build-xcframework.sh) and Android links the JNI bridge over it; everything else in those
# apps is Swift/Kotlin and platform frameworks.
#
# M10 — the client's FFmpeg excision — is what turned the same untidiness on the DESKTOP copies
# into a false statement a user can see: the shells print an `ffmpeg-next 8.1.0 — WTFPL` line and
# the full FFmpeg licence text three screens under a card saying no FFmpeg is bundled. So they are
# scoped too, each to the binaries its package actually installs — the shell, the session streamer,
# the headless CLI, and on Linux the update helper (`pf-update` ships as pf-update-client).
#
# The ROOT file stays workspace-wide on purpose: the HOST ships out of it, and the host does still
# link FFmpeg.
#
# Only the offline generator can scope a file (cargo-about renders the whole workspace), so these
# always go through it — the root file above still prefers cargo-about when installed.
if [ "$OUT" = "THIRD-PARTY-NOTICES.txt" ]; then
    # <in-tree path> <workspace members whose closure it must state>
    while read -r dest packages; do
        [ -n "$dest" ] || continue
        [ -d "$(dirname "$dest")" ] || continue
        python3 scripts/gen-third-party-notices.py --packages "$packages" --out "$dest"
        echo "==> generated $dest ($packages closure)" >&2
    done <<'CLIENTS'
clients/apple/Sources/PunktfunkKit/Resources/THIRD-PARTY-NOTICES.txt punktfunk-core
clients/android/app/src/main/assets/THIRD-PARTY-NOTICES.txt punktfunk-client-android
clients/linux/THIRD-PARTY-NOTICES.txt punktfunk-client-linux,punktfunk-client-session,punktfunk-cli,pf-update
clients/windows/THIRD-PARTY-NOTICES.txt punktfunk-client-windows,punktfunk-client-session,punktfunk-cli
CLIENTS
fi

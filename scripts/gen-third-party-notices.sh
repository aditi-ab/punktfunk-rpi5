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

# Keep the per-client in-tree copies in sync (the GUI apps bundle these as resources/assets and
# show them on their Acknowledgements / Open-source-licenses screen). The Linux/Windows Rust clients
# embed the root file directly via include_str!, so they need no copy.
if [ "$OUT" = "THIRD-PARTY-NOTICES.txt" ]; then
    for dest in \
        clients/apple/Sources/PunktfunkKit/Resources/THIRD-PARTY-NOTICES.txt \
        clients/android/app/src/main/assets/THIRD-PARTY-NOTICES.txt; do
        if [ -d "$(dirname "$dest")" ]; then
            cp "$OUT" "$dest"
            echo "==> synced $dest" >&2
        fi
    done
fi

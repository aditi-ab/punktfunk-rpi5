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

#!/usr/bin/env bash
# Assemble the Decky plugin into the canonical store/sideload layout:
#
#   out/punktfunk-v<version>.zip   ->  punktfunk/{dist/index.js,main.py,plugin.json,
#                                                  package.json,decky.pyi,LICENSE,README.md}
#   out/punktfunk/                 (the same tree, unzipped — rsync this with scripts/deploy.sh)
#
# Decky extracts the zip with --strip-components=1, so the single top-level dir MUST equal
# plugin.json "name". Run after `pnpm build` (or use `pnpm run package`). Host-agnostic: needs
# only bash, python3 and zip.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"

[ -f dist/index.js ] || { echo "dist/index.js missing — run 'pnpm build' first" >&2; exit 1; }
[ -f LICENSE ]       || { echo "LICENSE missing (required by the Decky store)" >&2; exit 1; }

NAME="$(python3 -c 'import json;print(json.load(open("plugin.json"))["name"])')"
VER="$(python3 -c 'import json;print(json.load(open("package.json"))["version"])')"

STAGE="$(mktemp -d)"
DEST="$STAGE/$NAME"
mkdir -p "$DEST/dist" "$DEST/bin"
cp dist/index.js "$DEST/dist/index.js"          # ship the bundle only, not the sourcemap
cp main.py plugin.json package.json LICENSE "$DEST/"
# The stream-launch wrapper (target of the Steam shortcut) — must stay executable.
cp bin/punktfunkrun.sh "$DEST/bin/punktfunkrun.sh"
chmod 0755 "$DEST/bin/punktfunkrun.sh"
[ -f decky.pyi ]  && cp decky.pyi  "$DEST/"
[ -f README.md ]  && cp README.md  "$DEST/"

OUT="$HERE/out"
mkdir -p "$OUT"
ZIP="$OUT/${NAME}-v${VER}.zip"
rm -f "$ZIP"
( cd "$STAGE" && zip -r -X "$ZIP" "$NAME" >/dev/null )
# Leave an unzipped staging tree for the rsync/sudo deploy path (scripts/deploy.sh).
rm -rf "$OUT/$NAME" && cp -r "$DEST" "$OUT/$NAME"
rm -rf "$STAGE"

echo "built  $ZIP"
echo "staged $OUT/$NAME  (deploy with: DECK=deck@<ip> bash scripts/deploy.sh)"

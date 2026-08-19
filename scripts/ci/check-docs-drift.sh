#!/bin/sh
# Docs drift gates (docs-and-onboarding-overhaul WP1). The docs rotted through four unchecked
# duplication channels; these are the cheap textual halves of closing them. No cargo, no bun
# install — pure git grep, so the gate runs on every push in seconds (the deep half, regenerating
# the OpenAPI spec from the host binary, lives in ci.yml's `rust` job where the build already
# exists).
#
#   1. docs-site/public/openapi.json must be a byte-for-byte copy of api/openapi.json. The
#      snapshot is a manual `cp` (docs-site/README.md); it sat stale on main for weeks once,
#      publishing an /api reference missing the whole self-update surface.
#   2. Every PUNKTFUNK_* variable the docs mention must still exist somewhere in the tree
#      (docs describing a removed knob is drift a reader pays for). Historical records —
#      docs/releases/, CHANGELOG.md — don't count as existence: a knob that lives only in old
#      release notes is gone.
#   3. Ratchet: the count of PUNKTFUNK_* env vars in code that the docs never mention must not
#      grow. 300+ internal/debug knobs are deliberately undocumented today, so this can't be a
#      hard list — but a NEW knob must either be documented in docs-site (configuration.md or
#      the page owning its feature) or the baseline below raised in the same commit, making
#      "undocumented" a decision instead of an accident. Shrink the gap? Lower the baseline.
#   4. Every command the host-cli.md tables list must still exist as a string literal in
#      crates/punktfunk-host (same "docs describing removed things" class as gate 2).
#   5. data/platforms.json (the single source for install/port facts that docs, the website
#      download page and the guided installer consume) must parse, and docs-site/src/data/
#      platforms.json — the snapshot the <Install/> and <Ports/> MDX components render from
#      (the docs Docker build context is docs-site/ alone) — must be a byte copy of it, same
#      rule as gate 1.
#
# Textual gates, so textual limits: gate 2/3 match token spelling, not env reads — a var name in
# a code comment counts as "exists", and a quoted constant that isn't an env var counts toward
# the ratchet. Both err toward false calm on removal and a one-line baseline bump on addition,
# which is the cheap side to be wrong on.

set -u
LC_ALL=C
export LC_ALL
cd "$(dirname "$0")/../.." || exit 2

fail=0
tmp="${TMPDIR:-/tmp}/docs-drift.$$"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT

# ---------------------------------------------------------------- gate 1: openapi snapshot
if [ "$(cksum < api/openapi.json)" != "$(cksum < docs-site/public/openapi.json)" ]; then
    echo "::error::docs-site/public/openapi.json is not a copy of api/openapi.json — re-sync it:"
    echo "  cp api/openapi.json docs-site/public/openapi.json"
    fail=1
fi

# ---------------------------------------------------------------- gate 2: docs env vars exist
git grep -ohE 'PUNKTFUNK_[A-Z0-9_]+' -- docs-site/content | sort -u > "$tmp/docs-vars"
while IFS= read -r var; do
    if ! git grep -qF "$var" -- ':!docs-site' ':!docs/releases' ':!CHANGELOG.md'; then
        echo "::error::docs-site documents $var but nothing outside the docs mentions it — the knob was removed or renamed; fix the docs page"
        fail=1
    fi
done < "$tmp/docs-vars"

# ---------------------------------------------------------------- gate 3: undocumented ratchet
# Quoted occurrences only: env reads are quoted string literals; bare identifiers are
# Rust constants / C symbols, not knobs. The committed baseline enumerates today's undocumented
# set so a violation names exactly the new knob.
baseline=scripts/ci/docs-undocumented-env-baseline.txt
sort -u "$baseline" > "$tmp/baseline"   # re-sort under our LC_ALL=C, whatever locale wrote it
git grep -ohE '"PUNKTFUNK_[A-Z0-9_]+"' -- ':!docs-site' ':!*.md' | tr -d '"' | sort -u > "$tmp/code-vars"
comm -23 "$tmp/code-vars" "$tmp/docs-vars" > "$tmp/undocumented"
comm -23 "$tmp/undocumented" "$tmp/baseline" > "$tmp/new-undocumented"
if [ -s "$tmp/new-undocumented" ]; then
    echo "::error::new PUNKTFUNK_* vars are neither documented in docs-site nor in the baseline:"
    sed 's/^/  /' "$tmp/new-undocumented"
    echo "Document each in docs-site (configuration.md or the page owning the feature), or — for a deliberately internal knob — add it to $baseline in the same commit."
    fail=1
fi
comm -13 "$tmp/undocumented" "$tmp/baseline" > "$tmp/stale-baseline"
if [ -s "$tmp/stale-baseline" ]; then
    echo "baseline entries no longer undocumented (removed or now documented) — prune them from $baseline:"
    sed 's/^/  /' "$tmp/stale-baseline"
fi

# ---------------------------------------------------------------- gate 4: documented CLI exists
# First table cell of every row in host-cli.md: subcommands, sub-actions and flags. Multi-word
# cells (flag + argument) are skipped — they don't map to one string literal.
grep -E '^\|' docs-site/content/docs/host-cli.md | awk -F'|' '{print $2}' \
    | grep -oE '`[a-z0-9-]+`|`--[a-z-]+`' | tr -d '`' | sort -u > "$tmp/cli-cmds"
while IFS= read -r cmd; do
    if ! git grep -qF "\"$cmd\"" -- crates/punktfunk-host; then
        echo "::error::host-cli.md documents \`$cmd\` but crates/punktfunk-host has no \"$cmd\" literal — removed or renamed; fix the docs page"
        fail=1
    fi
done < "$tmp/cli-cmds"

# ---------------------------------------------------------------- gate 5: platforms.json parses
# Explicit try/exit: `bun -e` (1.3.x) exits 0 on an uncaught JSON.parse throw, so relying on
# the default uncaught-exception exit code silently disarms the gate.
json_check='try{JSON.parse(require("fs").readFileSync("data/platforms.json","utf8"))}catch(e){console.error(e.message);process.exit(1)}'
if [ ! -f data/platforms.json ]; then
    echo "::error::data/platforms.json is missing"
    fail=1
elif command -v bun >/dev/null 2>&1; then
    bun -e "$json_check" || { echo "::error::data/platforms.json is not valid JSON"; fail=1; }
elif command -v node >/dev/null 2>&1; then
    node -e "$json_check" || { echo "::error::data/platforms.json is not valid JSON"; fail=1; }
fi
if [ "$(cksum < data/platforms.json)" != "$(cksum < docs-site/src/data/platforms.json 2>/dev/null)" ]; then
    echo "::error::docs-site/src/data/platforms.json is not a copy of data/platforms.json — re-sync it:"
    echo "  cp data/platforms.json docs-site/src/data/platforms.json"
    fail=1
fi

exit "$fail"

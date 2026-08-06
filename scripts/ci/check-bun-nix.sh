#!/bin/sh
# Drift gate for the generated bun2nix lockfile expressions (web/bun.nix, sdk/bun.nix).
#
# `bun.nix` is a DERIVED file: bun2nix is a pure function of `bun.lock` (it reads the lockfile text
# and emits one `fetchurl` per package, keyed by the lockfile's own integrity hashes — see
# packaging/nix/README.md). Nothing but the lockfile goes in, so any disagreement between the two
# committed files is drift, and it is always mechanically fixable.
#
# Why this exists: moving the bun packages to bun2nix (1db8f763) removed the *aggregate deps hash*
# that used to go stale, but not the second, quieter way a derived file rots. `bun.nix` regenerates
# only from a local `bun install` that runs lifecycle scripts (web's `postinstall`, the SDK's
# `prepare`). It does NOT regenerate on:
#
#   * `bun install --ignore-scripts` — which is what EVERY bun install in CI uses (ci.yml,
#     web-screenshots.yml, windows-host.yml, sdk-publish.yml), because web's `postinstall` shells
#     out to a `bun` on PATH that CI's portable bun isn't;
#   * a merge or rebase — git merges `bun.lock` and `bun.nix` as two unrelated files, so a branch
#     that generated `bun.nix` before picking up someone else's lockfile change silently commits
#     the pair out of step;
#   * a lockfile edited or re-resolved by hand.
#
# That second case is not hypothetical: it is how `web/bun.nix` shipped on main carrying
# brace-expansion@5.0.7 (plus two nested entries the override had already collapsed) while
# `web/bun.lock` said 5.0.8 — the `^5.0.8` override from ec9aa415 landed in the lockfile, the
# bun2nix branch had generated `bun.nix` off the pre-override lock, and the merge kept both. The
# Nix build fetches node_modules strictly from `bun.nix`, so the offline `bun install` inside the
# derivation is then asked for a tarball the store cache does not contain and `punktfunk-web` fails
# to build — with a "package not found" that names npm, not the lockfile that actually drifted.
#
# The gate also enforces the version pin the flake and README only *state*: `bun.nix` has no schema
# stability across bun2nix releases, so the flake input ref and BOTH npm devDependencies must name
# the same exact version. Nothing checked that before; a half-moved pin regenerates the file with a
# generator the flake does not use.
#
# The list of packages to check is read out of packaging/nix/packages.nix (its `bunNix = src + …`
# lines) rather than hardcoded here, so a third bun package is covered the day it is added — and an
# empty list is a hard error, because a gate that checks nothing passes exactly like a clean tree.
#
# Usage:
#   scripts/ci/check-bun-nix.sh          # verify; non-zero on drift (CI)
#   scripts/ci/check-bun-nix.sh --fix    # regenerate the committed files in place
set -eu

FIX=0
if [ $# -gt 0 ]; then
    case "$1" in
        --fix) FIX=1 ;;
        *) echo "usage: $0 [--fix]" >&2; exit 2 ;;
    esac
fi

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
PACKAGES_NIX="$ROOT/packaging/nix/packages.nix"
FLAKE="$ROOT/flake.nix"

command -v bun >/dev/null 2>&1 || {
    echo "check-bun-nix: bun is not on PATH (needed to run bun2nix and to read package.json)" >&2
    exit 1
}
[ -f "$PACKAGES_NIX" ] || { echo "check-bun-nix: no $PACKAGES_NIX" >&2; exit 1; }
[ -f "$FLAKE" ] || { echo "check-bun-nix: no $FLAKE" >&2; exit 1; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# --- the pinned bun2nix version -------------------------------------------------------------------
# flake.nix:  url = "github:nix-community/bun2nix?ref=2.1.2";
PINNED=$(sed -n 's/.*github:nix-community\/bun2nix?ref=\([^"]*\)".*/\1/p' "$FLAKE" | head -1)
[ -n "$PINNED" ] || {
    echo "check-bun-nix: could not read the bun2nix input ref out of $FLAKE." >&2
    echo "Expected a line like: url = \"github:nix-community/bun2nix?ref=<version>\";" >&2
    exit 1
}

# --- which packages carry a generated bun.nix -----------------------------------------------------
# packages.nix:  bunDeps = bun2nix.fetchBunDeps { bunNix = src + "/web/bun.nix"; };
sed -n 's/.*bunNix *= *src *+ *"\/\(.*\)\/bun\.nix".*/\1/p' "$PACKAGES_NIX" | sort -u > "$TMP/roots"
if [ ! -s "$TMP/roots" ]; then
    echo "check-bun-nix: found no \`bunNix = src + \"/<dir>/bun.nix\"\` in $PACKAGES_NIX." >&2
    echo "Either the bun packages were removed (delete this gate) or the expression changed shape" >&2
    echo "and the gate silently stopped checking anything. Not passing vacuously." >&2
    exit 1
fi

fail=0
checked=0

# --- version pin agreement ------------------------------------------------------------------------
# `bun.nix` has no schema stability across bun2nix versions, so the generator the flake builds with
# and the generator `bun install` runs must be the SAME exact version (packaging/nix/README.md).
while read -r dir; do
    pkgjson="$ROOT/$dir/package.json"
    [ -f "$pkgjson" ] || { echo "check-bun-nix: no $pkgjson" >&2; fail=1; continue; }
    dev=$(bun -e "const d=require(process.argv[1]).devDependencies||{};console.log(d.bun2nix??'')" \
        "$pkgjson")
    if [ "$dev" != "$PINNED" ]; then
        echo "check-bun-nix: bun2nix version pin disagrees." >&2
        echo "  flake.nix input ref            : $PINNED" >&2
        echo "  $dir/package.json devDependency : ${dev:-<absent>}" >&2
        echo "These must be the same exact version — bun.nix has no schema stability across" >&2
        echo "bun2nix releases. Move both together, then rerun this script with --fix." >&2
        fail=1
    fi
done < "$TMP/roots"

# --- the generator ---------------------------------------------------------------------------------
# Prefer an already-installed bun2nix at the pinned version (fast, offline — the dev case); otherwise
# fetch exactly the pinned one, once, into $TMP. Never a floating `bunx bun2nix`: that would generate
# with whatever is newest, and `bun.nix` has no schema stability across releases.
BUN2NIX=""
while read -r dir; do
    cand="$ROOT/$dir/node_modules/bun2nix/index.ts"
    [ -f "$cand" ] || continue
    have=$(bun -e "console.log(require(process.argv[1]).version??'')" \
        "$ROOT/$dir/node_modules/bun2nix/package.json" 2>/dev/null || echo '')
    if [ "$have" = "$PINNED" ]; then BUN2NIX="$cand"; break; fi
done < "$TMP/roots"

if [ -z "$BUN2NIX" ]; then
    # Installed in its own scratch dir, so this never touches the repo's lockfiles or .npmrc.
    mkdir -p "$TMP/gen"
    if ! ( cd "$TMP/gen" && bun add --exact "bun2nix@$PINNED" ) > "$TMP/geninstall.log" 2>&1; then
        echo "check-bun-nix: could not install bun2nix@$PINNED" >&2
        cat "$TMP/geninstall.log" >&2
        exit 1
    fi
    BUN2NIX="$TMP/gen/node_modules/bun2nix/index.ts"
    [ -f "$BUN2NIX" ] || { echo "check-bun-nix: bun2nix@$PINNED installed but $BUN2NIX is absent" >&2; exit 1; }
fi

run_bun2nix() { # <lockfile> <outfile>
    bun "$BUN2NIX" --lock-file "$1" --output-file "$2"
}

# --- regenerate + compare ---------------------------------------------------------------------------
while read -r dir; do
    lock="$ROOT/$dir/bun.lock"
    nix="$ROOT/$dir/bun.nix"
    [ -f "$lock" ] || { echo "check-bun-nix: no $lock (packages.nix expects $dir/bun.nix)" >&2; fail=1; continue; }

    out="$TMP/$(echo "$dir" | tr '/' '_').bun.nix"
    run_bun2nix "$lock" "$out" >/dev/null

    if [ "$FIX" -eq 1 ]; then
        if [ ! -f "$nix" ] || ! cmp -s "$nix" "$out"; then
            cp "$out" "$nix"
            echo "check-bun-nix: regenerated $dir/bun.nix from $dir/bun.lock"
        else
            echo "check-bun-nix: $dir/bun.nix already in sync"
        fi
        checked=$((checked + 1))
        continue
    fi

    if [ ! -f "$nix" ]; then
        echo "check-bun-nix: $dir/bun.nix is MISSING — packages.nix fetches node_modules from it." >&2
        fail=1
        continue
    fi
    # Plain files, not `diff <(…) <(…)`: Gitea's runner executes a step's `run:` under `sh`, and
    # dash has no process substitution — it would reject the script at parse time and the gate
    # would never compare anything (exactly how the shader SPIR-V gate in ci.yml was lost).
    if cmp -s "$nix" "$out"; then
        echo "check-bun-nix: $dir/bun.nix matches $dir/bun.lock"
    else
        echo "check-bun-nix: $dir/bun.nix is STALE — it does not match $dir/bun.lock." >&2
        echo "The Nix build fetches node_modules only from bun.nix, so punktfunk's bun packages" >&2
        echo "would build against the wrong dependency set (or fail to fetch it at all)." >&2
        echo "Regenerate and commit it:  scripts/ci/check-bun-nix.sh --fix" >&2
        echo "--- diff (committed -> regenerated from bun.lock) ---" >&2
        diff -u "$nix" "$out" >&2 || true
        fail=1
    fi
    checked=$((checked + 1))
done < "$TMP/roots"

[ "$checked" -gt 0 ] || { echo "check-bun-nix: checked nothing — refusing to report success" >&2; exit 1; }
if [ "$fail" -eq 0 ] && [ "$FIX" -eq 0 ]; then
    echo "check-bun-nix: $checked bun package(s) in sync, bun2nix pinned at $PINNED everywhere"
fi
exit "$fail"

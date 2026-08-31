#!/bin/sh
# Prove the punktfunk-setup binary runs the same commands as scripts/install.sh.
#
# M1's ground rule (design/installer-v2.md, implementation plan §0): while both installers
# exist, parity is PROVEN by running both, not asserted. This drives each over the same fake
# os-release trees the drift gate uses and diffs the `+ command` lines, per family and channel.
#
# Retire this together with the sh script body at WP4.3 — at that point the binary IS the
# installer and there is nothing left to compare it against.
#
# Exit: 0 identical (modulo the two documented differences below) · 1 drift.
set -u

cd "$(dirname "$0")/../.." || exit 1
work=$(mktemp -d) || exit 1
trap 'rm -rf "$work"' EXIT
fail=0

BIN=${PUNKTFUNK_SETUP_BIN:-}
if [ -z "$BIN" ]; then
    cargo build -q -p punktfunk-setup || exit 1
    BIN=target/debug/punktfunk-setup
fi

# THE TWO KNOWN DIFFERENCES, both deliberate:
#
# 1. `punktfunk-host detect-conflicts` — the sh script shells out to it inside step 2 and echoes
#    the command in --dry-run when the host is not installed yet. The binary probes conflicts
#    once, up front, into Facts (design D3), so it has no command to echo there. Dropped from
#    the sh side before the diff.
# 2. The punktfunk group. D4 flipped its default to yes on every box; sh still asks per box and
#    answers no under --yes off a couch box. Both sides run with --no-punktfunk-group so the
#    flip does not drown out real drift — choices.rs::defaults_table is what guards the flip.
drop_known() {
    grep -v 'punktfunk-host detect-conflicts'
}

box() {
    mkdir -p "$work/$1"
    printf '%s\n' "$2" > "$work/$1/os-release"
}

box arch     'ID=arch
ID_LIKE=arch
PRETTY_NAME="Arch Linux"'
box omarchy  'ID=omarchy
ID_LIKE=arch
PRETTY_NAME="Omarchy"'
box debian   'ID=debian
VERSION_ID="13"
PRETTY_NAME="Debian GNU/Linux 13"'
box fedora   'ID=fedora
VERSION_ID="44"
PRETTY_NAME="Fedora Linux 44"'
box bazzite  'ID=bazzite
ID_LIKE="fedora"
VERSION_ID="43"
PRETTY_NAME="Bazzite"'

for family in arch omarchy debian fedora bazzite; do
    for channel in stable canary; do
        for extra in '' '--uninstall'; do
            label="$family/$channel${extra:+ $extra}"
            common="--dry-run --yes --no-punktfunk-group --channel $channel $extra"
            PUNKTFUNK_INSTALL_OS_RELEASE="$work/$family/os-release" \
                sh scripts/install.sh $common 2>&1 |
                grep '^  + ' | drop_known > "$work/sh.txt"
            PUNKTFUNK_INSTALL_OS_RELEASE="$work/$family/os-release" NO_COLOR=1 \
                "$BIN" $common 2>&1 |
                grep '^  + ' | drop_known > "$work/rs.txt"
            if diff -u "$work/sh.txt" "$work/rs.txt" > "$work/diff.txt"; then
                echo "ok   $label"
            else
                echo "::error::installer parity drift on $label (sh is -, binary is +)"
                cat "$work/diff.txt"
                fail=1
            fi
        done
    done
done

exit $fail

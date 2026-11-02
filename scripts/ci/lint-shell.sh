#!/bin/sh
# Syntax and shellcheck gate for the shell the packages build and ship: every tracked file under
# packaging/ and scripts/ that is *.sh or opens with a sh or bash shebang.
#
#   lint-shell.sh           bash -n, then shellcheck at error severity (blocking)
#   lint-shell.sh --warn    shellcheck at warning severity (advisory)
#   lint-shell.sh --list    the files, one per line
#
# check-workflow-paths.py reads --list, so the packaging-lint trigger covers what this lints.
set -eu
cd "$(dirname "$0")/../.."

tracked=$(git ls-files -- packaging scripts)
[ -n "$tracked" ] || { echo "lint-shell: no tracked files under packaging/ or scripts/" >&2; exit 2; }
# shellcheck disable=SC2086 # tracked names under these two trees hold no whitespace
files=$(LC_ALL=C awk '
    FNR == 1 && (FILENAME ~ /\.sh$/ ||
        $0 ~ /^#![[:space:]]*(\/usr\/bin\/env[[:space:]]+)?([a-z\/]*\/)?(ba)?sh([[:space:]]|$)/) {
        print FILENAME
    }
    { nextfile }' $tracked)
[ -n "$files" ] || { echo "lint-shell: found no shell files" >&2; exit 2; }

case "${1:-}" in
    --list) printf '%s\n' "$files" ;;
    --warn) printf '%s\n' "$files" | xargs shellcheck --severity=warning ;;
    '')
        printf '%s\n' "$files" | xargs -n1 bash -n
        printf '%s\n' "$files" | xargs shellcheck --severity=error
        echo "lint-shell: $(printf '%s\n' "$files" | wc -l | tr -d ' ') files clean at error severity"
        ;;
    *) echo "usage: lint-shell.sh [--list|--warn]" >&2; exit 2 ;;
esac

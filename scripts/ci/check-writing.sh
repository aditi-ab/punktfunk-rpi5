#!/bin/sh
# House-style length gates. See scripts/ci/check-writing.py and docs/writing.md.
set -u
cd "$(dirname "$0")/../.." || exit 2
if ! command -v python3 >/dev/null 2>&1; then
    echo "::error::python3 is required for scripts/ci/check-writing.sh" >&2
    exit 2
fi
exec python3 scripts/ci/check-writing.py "$@"

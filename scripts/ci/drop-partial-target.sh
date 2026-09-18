#!/bin/sh
# Remove the target/ a failed cache restore leaves behind.
#
# actions/cache reports a corrupt archive as a miss: a warning, no `cache-hit` output, and
# whatever tar extracted before it stopped still on disk. A job starts with an empty workspace,
# so a miss that left target/ is a partial extract, and every cargo step would build on it.
# A push that builds then saves a fresh entry under the same key, and restore serves the newest.
#
# POSIX sh: the runner executes `run:` under dash.
#
# Usage: sh scripts/ci/drop-partial-target.sh <cache-hit output> <cache key>
set -e
hit=$1
key=$2

if [ -n "$hit" ] || [ ! -e target ]; then
  exit 0
fi

# The job env does not name the runner; the host's docker daemon does. `--retry 0` beats the
# retrying-curl shim, which would wait 100 s on a missing socket.
host=
if [ -S /var/run/docker.sock ]; then
  host=$(curl -sf --retry 0 --max-time 5 --unix-socket /var/run/docker.sock http://docker/info \
    | sed -n 's/.*"Name":"\([^"]*\)","Labels".*/\1/p') || true
fi
echo "::warning::target cache restore for $key failed on ${host:-an unnamed runner}; removed the partial target/, this job builds cold"
rm -rf target

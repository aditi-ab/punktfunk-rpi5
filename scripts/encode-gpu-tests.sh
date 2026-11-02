#!/bin/sh
# Run pf-encode's ignored GPU tests with one box's feature set and compare each test's result
# with the pass set recorded for that box in scripts/encode-gpu-tests/<box>.txt.
#
#   sh scripts/encode-gpu-tests.sh nvidia|amd|intel [extra cargo test args, e.g. --release]
#
# The first run on a box writes its file: commit it as the pass set. A later run diffs against
# it and exits 1 only when a recorded `ok` test fails or does not report, as after a crash of the
# test binary. A test the driver fails for its own reasons is a FAILED line in the file and does
# not change the exit code. The validation layer has its own script: scripts/vk-validation-tests.sh.
set -u

box=${1:-}
[ "$#" -gt 0 ] && shift
case $box in
  nvidia) features=nvenc,vulkan-encode,pyrowave ;;
  amd | intel) features=vulkan-encode,pyrowave ;;
  *) echo "usage: sh scripts/encode-gpu-tests.sh nvidia|amd|intel [cargo test args]" >&2; exit 2 ;;
esac

recorded=$(cd "$(dirname "$0")" && pwd)/encode-gpu-tests/$box.txt
log=$(mktemp "${TMPDIR:-/tmp}/pf-encode-gpu.XXXXXX")
now=$log.results
trap 'rm -f "$log" "$now"' EXIT

echo "== cargo test -p pf-encode --features $features -- --ignored"
cargo test -p pf-encode --features "$features" "$@" -- --ignored --test-threads=1 2>&1 | tee "$log"
grep -E '^test .* \.\.\. (ok|FAILED|ignored)' "$log" | LC_ALL=C sort >"$now"
if [ ! -s "$now" ]; then
  echo "encode-gpu-tests: no test results; did the build fail?" >&2
  exit 2
fi

if [ ! -f "$recorded" ]; then
  cp "$now" "$recorded"
  echo "encode-gpu-tests: recorded the first pass set for $box: $(grep -c ' \.\.\. ok$' "$now") ok."
  echo "Commit $recorded."
  exit 0
fi

diff "$recorded" "$now" || true
lost=$(grep ' \.\.\. ok$' "$recorded" | LC_ALL=C comm -23 - "$now")
if [ -n "$lost" ]; then
  echo "encode-gpu-tests: $box passed these before and does not now:" >&2
  echo "$lost" >&2
  exit 1
fi
echo "encode-gpu-tests: $box keeps its recorded pass set (new tests and fixes are in the diff)"

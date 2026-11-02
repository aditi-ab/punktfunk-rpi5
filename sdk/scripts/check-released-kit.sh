#!/usr/bin/env bash
# The newest released plugin kit, typechecked and tested against this tree's SDK.
#
# A plugin in the field is built on a released kit, and the runner swaps the host's SDK under it
# on every host upgrade (`reconcileSharedSdk`). The in-tree kit moves with the SDK, so only a
# released one shows an export the SDK dropped.
#
#   bash sdk/scripts/check-released-kit.sh [plugin-kit-vX.Y.Z]
#
# A release whose `@punktfunk/host` peer range leaves this SDK version out never claimed it: the
# check says so and passes. That is the state between a breaking SDK bump and the kit cut on it.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
retry() { bash "$root/scripts/ci/retry.sh" 3 "$@"; }

tag="${1:-}"
if [ -z "$tag" ]; then
    tag="$(retry git ls-remote --tags --refs --sort=-v:refname origin 'plugin-kit-v*' | head -n 1 | sed 's|.*refs/tags/||')"
fi
if [ -z "$tag" ]; then
    echo "no plugin-kit-v* tag on origin" >&2
    exit 1
fi
if ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    retry git fetch --quiet --depth 1 origin "refs/tags/$tag:refs/tags/$tag"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
# `clients/shared` holds the vectors the kit's own tests replay, as that release knew them.
git archive "$tag" plugin-kit clients/shared | tar -x -C "$work"

sdk_version="$(bun -e 'console.log(require(process.argv[1]).version)' "$root/sdk/package.json")"
range="$(bun -e 'console.log(require(process.argv[1]).peerDependencies["@punktfunk/host"])' "$work/plugin-kit/package.json")"
if ! bun -e 'process.exit(Bun.semver.satisfies(process.argv[1], process.argv[2]) ? 0 : 1)' "$sdk_version" "$range"; then
    echo "$tag asks for @punktfunk/host $range and this tree is $sdk_version: no released kit claims this SDK yet"
    exit 0
fi

(cd sdk && bun run build)
# The kit's `file:../sdk` devDependency. The built SDK then replaces whatever the install linked.
ln -s "$root/sdk" "$work/sdk"
cd "$work/plugin-kit"
retry bun install --ignore-scripts
rm -rf node_modules/@punktfunk/host
mkdir -p node_modules/@punktfunk/host
cp -R "$root/sdk/dist" "$root/sdk/package.json" node_modules/@punktfunk/host/

echo "== $tag against @punktfunk/host $sdk_version at $(git -C "$root" rev-parse --short HEAD)"
bun run typecheck
bun test
# The worked example the first-party scanners are cut from: it has to load on this SDK.
bun -e 'const { default: def } = await import("./examples/lutris-plugin.ts");
if (typeof def?.name !== "string") throw new Error("the example defines no plugin");
console.log("== example plugin", def.name, "loads");'

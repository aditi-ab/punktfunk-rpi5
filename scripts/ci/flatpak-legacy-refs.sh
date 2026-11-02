#!/usr/bin/env bash
# Keep a flatpak repo's legacy-id refs in step with the new id's:
#   flatpak-legacy-refs.sh <repo> <remote> <app-id> <legacy-id> <gpg-keyid> <gpg-homedir>
#
# A legacy ref whose new counterpart (same arch and branch) exists is marked end-of-life with a
# rebase onto it, once. One whose counterpart is missing must not point anywhere: `flatpak
# update` fails on a rebase to a missing ref. Its marker is undone by resetting the ref to the
# commit before the marker, fetched from <remote> when the repo holds only the tip.
set -euo pipefail
repo=$1 remote=$2 app=$3 legacy=$4 key=$5 home=$6
here="$(cd "$(dirname "$0")" && pwd)"

for dir in "$repo/refs/heads/app/$legacy"/*/*; do
  [ -e "$dir" ] || continue
  branch=${dir##*/}
  arch=${dir%/*}
  arch=${arch##*/}
  ref="app/$legacy/$arch/$branch"
  marked=0
  if ostree --repo="$repo" show --print-metadata-key=ostree.endoflife-rebase "$ref" \
    >/dev/null 2>&1; then
    marked=1
  fi
  if [ -e "$repo/refs/heads/app/$app/$arch/$branch" ]; then
    [ "$marked" = 1 ] && continue
    flatpak build-commit-from --src-ref="$ref" --end-of-life-rebase="$legacy=$app" \
      --gpg-sign="$key" --gpg-homedir="$home" "$repo" "$ref"
    echo "flatpak-legacy-refs: marked $ref end-of-life, rebase onto app/$app/$arch/$branch"
  elif [ "$marked" = 1 ]; then
    parent=$(ostree --repo="$repo" rev-parse "$ref^")
    if ! ostree --repo="$repo" show "$parent" >/dev/null 2>&1; then
      bash "$here/retry.sh" 5 ostree --repo="$repo" pull --mirror --depth=0 "$remote" "$parent"
    fi
    ostree --repo="$repo" reset "$ref" "$parent"
    echo "flatpak-legacy-refs: unmarked $ref, app/$app/$arch/$branch is not published yet"
  fi
done

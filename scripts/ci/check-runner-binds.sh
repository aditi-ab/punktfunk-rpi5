#!/bin/sh
# Every config-dir path the plugin runner reads must be bound into its unit's empty home.
#
# `ProtectHome=tmpfs` means an unnamed path does not exist for the runner. Names come from the SDK;
# for a nested file the first component names the directory bind that keeps renames visible.
set -eu
cd "$(dirname "$0")/../.."

UNIT=scripts/punktfunk-scripting.service
NIX=packaging/nix/nixos-module.nix

names=$(grep -rhoE 'path\.join\((configDir\(\)|configDir|config), "[^"]+"' sdk/src |
	sed -E 's/.*"([^"]+)"/\1/' | sort -u)

rc=0
for n in $names; do
	for f in "$UNIT" "$NIX"; do
		grep -q "punktfunk/$n" "$f" || {
			echo "$f: the runner reads \$config/$n and this unit never binds it"
			rc=1
		}
	done
done

if [ "$rc" != 0 ]; then
	echo "add a Bind*Paths line — under the tmpfs home an unbound path is ENOENT, not a permission error"
fi
exit "$rc"

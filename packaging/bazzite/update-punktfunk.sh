#!/usr/bin/env bash
# Update the layered punktfunk packages on a Bazzite / Fedora-Atomic host.
#
# Why this exists: `rpm-ostree upgrade` upgrades the *base image* and only re-resolves
# layered packages WHEN THE BASE CHANGES. Bazzite bases can sit frozen for months (a pinned
# `:stable` tag, a paused rebase), so `rpm-ostree upgrade` keeps reporting "No updates
# available" and your layered punktfunk never moves even though newer RPMs are in the repo.
# The fix is to force rpm-ostree to re-resolve just the punktfunk layer against the latest
# repo metadata — an `--uninstall … --install …` of the same package names in one
# transaction. This script does that for whichever of punktfunk / punktfunk-web are layered.
#
# Usage:  sudo bash update-punktfunk.sh          # stage the newest; you reboot when ready
#         sudo bash update-punktfunk.sh --reboot # stage, then reboot immediately
#
# Channel note: it re-resolves against every ENABLED punktfunk repo. If both
# `punktfunk.repo` (stable) and `punktfunk-canary.repo` are enabled, canary's version sorts
# higher and WINS — the box silently tracks canary. Enable exactly the channel you want
# (set `enabled=0` in the other `/etc/yum.repos.d/punktfunk*.repo`).
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "run as root: sudo bash $0 ${*:-}" >&2
  exit 1
fi

# Which punktfunk packages are actually layered right now (host, web, or both).
mapfile -t layered < <(rpm-ostree status --json 2>/dev/null \
  | grep -oE '"punktfunk(-web)?"' | tr -d '"' | sort -u)
if [[ ${#layered[@]} -eq 0 ]]; then
  # Fall back to the rpm db if the JSON shape ever changes.
  mapfile -t layered < <(rpm -qa --qf '%{NAME}\n' 'punktfunk' 'punktfunk-web' 2>/dev/null | sort -u)
fi
if [[ ${#layered[@]} -eq 0 ]]; then
  echo "no punktfunk packages are layered — install first (see packaging/bazzite/README.md)" >&2
  exit 1
fi
echo "layered punktfunk packages: ${layered[*]}"

# Fresh repo metadata, else the re-resolve can pick a stale 'newest'.
rpm-ostree refresh-md --force >/dev/null

# Force the re-resolve: remove + re-add the same names in ONE transaction so the box is never
# left without the host, and rpm-ostree picks the newest available version.
args=()
for p in "${layered[@]}"; do args+=(--uninstall "$p"); done
for p in "${layered[@]}"; do args+=(--install "$p"); done
echo "+ rpm-ostree update ${args[*]}"
rpm-ostree update "${args[@]}"

echo
echo "Staged. The new version activates on the next boot."
if [[ "${1:-}" == "--reboot" ]]; then
  echo "rebooting now…"
  systemctl reboot
else
  echo "Reboot when ready:  systemctl reboot"
fi

#!/usr/bin/env bash
# shellcheck shell=bash
# Single source of truth for punktfunk release/canary version numbers (Linux + macOS runners).
#
# WHY THIS EXISTS: every release workflow used to hardcode a canary base version
# (`0.5.0`, `0.3`, …). Those magic numbers had to be hand-bumped on every stable release
# and nobody did — so canary channels silently fell *behind* stable (a canary showed up on
# TestFlight as 0.5.0 while 0.6.0 was already published). This script computes the base
# version DETERMINISTICALLY from the git tags instead, so there is nothing to hand-edit and
# no future agent can get it wrong.
#
# THE RULE (chosen 2026-07-03):
#   * stable  (a `vX.Y.Z` tag push) → PF_BASE = X.Y.Z (the tag, minus any -rc/+meta suffix).
#   * canary  (a main push)         → PF_BASE = <latest stable tag with minor+1, patch 0>.
#     i.e. latest release v0.6.0 → canary base 0.7.0. Cutting v0.7.0 auto-advances canary to
#     0.8.0. Canary is therefore ALWAYS exactly one minor ahead of the newest stable release.
#
# USAGE (bash workflows) — eval it, then format your channel's suffix off $PF_BASE:
#     eval "$(bash scripts/ci/pf-version.sh)"
#     case "$GITHUB_REF" in
#       refs/tags/v*) V="${GITHUB_REF_NAME#v}" ;;          # stable
#       *)            V="${PF_BASE}-ci${GITHUB_RUN_NUMBER}" ;;   # canary suffix is per-channel
#     esac
#
# It prints `KEY=VALUE` lines (eval-able) to stdout and — when $GITHUB_ENV is set — also
# appends them there for later steps. Exports:
#     PF_CHANNEL     stable | canary
#     PF_BASE        the base semver X.Y.Z (see THE RULE)
#     PF_MAJOR/MINOR/PATCH   the components of PF_BASE (numeric-version channels build
#                            `<MAJOR>.<MINOR>.<run>` from these — MSIX/decky need monotonic ints)
#     PF_STABLE_TAG  the latest stable release version the canary base was derived from (for logs)
#
# The pwsh twin scripts/ci/pf-version.ps1 implements the identical rule for the Windows runners.
set -euo pipefail

_root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/../.." && pwd)"

# actions/checkout is shallow and fetches NO tags by default — canary needs the full tag list
# to find the latest stable. Best-effort fetch; the Cargo.toml fallback below covers a fresh
# repo with no tags at all.
git -C "$_root" fetch --tags --force --quiet 2>/dev/null || true

# Latest stable release = highest strict vX.Y.Z tag (pre-releases like v0.7.0-rc1 are ignored).
_stable="$(
  git -C "$_root" tag -l 'v*' \
    | sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$/\1/p' \
    | sort -V | tail -n1
)"
if [ -z "${_stable:-}" ]; then
  # No tags yet — seed from the workspace Cargo.toml version so canary still has a base.
  _stable="$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)".*/\1/p' "$_root/Cargo.toml" | head -n1)"
fi
_stable="${_stable:-0.0.0}"

case "${GITHUB_REF:-}" in
  refs/tags/v*)
    _channel="stable"
    _base="${GITHUB_REF_NAME#v}"
    _base="${_base%%-*}"   # drop -rc / pre-release for the numeric marketing/package version
    _base="${_base%%+*}"   # drop +build metadata
    ;;
  *)
    _channel="canary"
    _maj="${_stable%%.*}"; _rest="${_stable#*.}"; _min="${_rest%%.*}"
    _base="${_maj}.$((_min + 1)).0"
    ;;
esac

_pf_major="${_base%%.*}"; _pf_rest="${_base#*.}"; _pf_minor="${_pf_rest%%.*}"; _pf_patch="${_pf_rest##*.}"

_emit() {
  printf '%s=%s\n' "$1" "$2"
  if [ -n "${GITHUB_ENV:-}" ]; then printf '%s=%s\n' "$1" "$2" >> "$GITHUB_ENV"; fi
}
_emit PF_CHANNEL    "$_channel"
_emit PF_BASE       "$_base"
_emit PF_MAJOR      "$_pf_major"
_emit PF_MINOR      "$_pf_minor"
_emit PF_PATCH      "$_pf_patch"
_emit PF_STABLE_TAG "$_stable"

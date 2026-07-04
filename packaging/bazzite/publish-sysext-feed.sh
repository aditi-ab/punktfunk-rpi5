#!/usr/bin/env bash
# Publish a punktfunk sysext image into its feed on the Gitea generic package registry —
# called by .gitea/workflows/rpm.yml after the RPM publish. A feed is one fixed URL
# (…/punktfunk-sysext/<feed>/) holding versioned .raw files plus a SHA256SUMS manifest;
# punktfunk-sysext(8) on the boxes reads SHA256SUMS to find + verify the newest image
# (the layout is also exactly what systemd-sysupdate's url-file source expects, so a
# .transfer feed can be added later without re-publishing anything).
#
# Usage: TOKEN=… [KEEP=6] bash publish-sysext-feed.sh <feed> <image.raw>
#   <feed>   e.g. f43, f43-canary, f44 (Fedora major x channel)
#   KEEP     newest images to keep in the feed; 0/unset-for-stable = keep all
# Env: REGISTRY (git.unom.io), OWNER (unom), TOKEN (write:package PAT), CURL_USER (login name)
set -euo pipefail

FEED="${1:?usage: publish-sysext-feed.sh <feed> <image.raw>}"
RAW="${2:?usage: publish-sysext-feed.sh <feed> <image.raw>}"
[ -f "$RAW" ] || { echo "no such image: $RAW" >&2; exit 1; }
REGISTRY="${REGISTRY:-git.unom.io}"
OWNER="${OWNER:-unom}"
KEEP="${KEEP:-0}"
AUTH=(--user "${CURL_USER:-enricobuehler}:${TOKEN:?TOKEN (write:package PAT) required}")
BASE="https://$REGISTRY/api/packages/$OWNER/generic/punktfunk-sysext/$FEED"

FNAME="$(basename "$RAW")"
SHA="$(sha256sum "$RAW" | cut -d' ' -f1)"

# Merge into the existing manifest: drop any prior line for this filename, append ours.
SUMS="$(mktemp)"; trap 'rm -f "$SUMS"' EXIT
curl -fsS "${AUTH[@]}" "$BASE/SHA256SUMS" 2>/dev/null | grep -v " $FNAME\$" > "$SUMS" || true
printf '%s  %s\n' "$SHA" "$FNAME" >> "$SUMS"

# Prune: keep only the newest $KEEP images (by version sort) in manifest + registry.
PRUNE=()
if [ "$KEEP" -gt 0 ]; then
  mapfile -t PRUNE < <(awk '{print $2}' "$SUMS" | sort -V | head -n -"$KEEP")
  for f in "${PRUNE[@]:-}"; do
    [ -n "$f" ] && sed -i "\| $f\$|d" "$SUMS"
  done
fi

# Upload order keeps consumers consistent: image first, then the manifest referencing it,
# then prune deletions (already absent from the manifest). Delete-before-put makes workflow
# re-runs idempotent (the registry 409s on duplicate filenames; first-publish 404s are fine).
curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/$FNAME" || true
curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$RAW" "$BASE/$FNAME"
curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/SHA256SUMS" || true
curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$SUMS" "$BASE/SHA256SUMS"
for f in "${PRUNE[@]:-}"; do
  [ -n "$f" ] && { echo "pruning $f"; curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/$f" || true; }
done
echo "published $FNAME -> $BASE ($(wc -l <"$SUMS") image(s) in the feed)"

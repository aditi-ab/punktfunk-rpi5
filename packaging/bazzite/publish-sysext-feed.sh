#!/usr/bin/env bash
# Publish a punktfunk sysext image into its feed on the Gitea generic package registry —
# called by .gitea/workflows/rpm.yml after the RPM publish. A feed is one fixed URL
# (…/punktfunk-sysext/<feed>/) holding versioned .raw files plus a SHA256SUMS manifest and its
# detached OpenPGP signature SHA256SUMS.asc; punktfunk-sysext(8) on the boxes verifies that
# signature, then uses the manifest to find + check the newest image (the layout is also exactly
# what systemd-sysupdate's url-file source expects, so a .transfer feed can be added later
# without re-publishing anything).
#
# Signing uses RPM_GPG_PRIVATE_KEY — the SAME packages@unom.io key that signs the RPMs, so boxes
# have one key to trust and we have one key to rotate. Without checksums-plus-signature the feed
# was self-certifying: SHA256SUMS sits on the same registry as the images it describes, so whoever
# could swap an image could swap its checksum in the same breath.
#
# Usage: TOKEN=… [KEEP=6] bash publish-sysext-feed.sh <feed> <image.raw>
#        TOKEN=… bash publish-sysext-feed.sh --seal <feed>
#   <feed>   e.g. f43, f43-canary, f44 (Fedora major x channel)
#   KEEP     newest images to keep in the feed; 0/unset-for-stable = keep all
#   --seal   re-sign a feed's EXISTING manifest without publishing an image. For feeds published
#            before signing existed, and after a key rotation. Idempotent.
# Env: REGISTRY (git.unom.io), OWNER (unom), TOKEN (write:package PAT), CURL_USER (login name),
#      RPM_GPG_PRIVATE_KEY (armored private key; absent => unsigned, fatal on a v* tag)
set -euo pipefail

SEAL=0
if [ "${1:-}" = --seal ]; then
  SEAL=1; FEED="${2:?usage: publish-sysext-feed.sh --seal <feed>}"; RAW=""
else
  FEED="${1:?usage: publish-sysext-feed.sh <feed> <image.raw>}"
  RAW="${2:?usage: publish-sysext-feed.sh <feed> <image.raw>}"
  [ -f "$RAW" ] || { echo "no such image: $RAW" >&2; exit 1; }
fi
REGISTRY="${REGISTRY:-git.unom.io}"
OWNER="${OWNER:-unom}"
KEEP="${KEEP:-0}"
AUTH=(--user "${CURL_USER:-enricobuehler}:${TOKEN:?TOKEN (write:package PAT) required}")
BASE="https://$REGISTRY/api/packages/$OWNER/generic/punktfunk-sysext/$FEED"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
SUMS="$WORK/SHA256SUMS"
SIG="$WORK/SHA256SUMS.asc"

# sign_manifest — detached-sign $SUMS into $SIG with RPM_GPG_PRIVATE_KEY. Prints nothing and
# returns 1 if no key is available; the caller decides whether that is survivable.
sign_manifest() {
  [ -n "${RPM_GPG_PRIVATE_KEY:-}" ] || return 1
  local home keyid baked
  home="$(mktemp -d)"; chmod 700 "$home"
  # Loopback pinentry for the same reason sign-rpms.sh needs it: no TTY in CI.
  printf 'pinentry-mode loopback\n' > "$home/gpg.conf"
  printf 'allow-loopback-pinentry\n' > "$home/gpg-agent.conf"
  printf '%s' "$RPM_GPG_PRIVATE_KEY" | GNUPGHOME="$home" gpg --batch --quiet --import
  keyid="$(GNUPGHOME="$home" gpg --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')"
  # A feed signed by a key the client script doesn't carry is a feed nobody can install from, and
  # the failure would only surface on someone's Bazzite box. Compare fingerprints here instead: the
  # baked-in public key in punktfunk-sysext.sh must be the key we are about to sign with.
  # The armor block is a shell single-quoted literal in that script, so the range's first and last
  # lines carry the `FEED_KEY='` prefix and the closing quote — strip them or gpg sees no armor at
  # all and hands back an empty fingerprint, which would look like a mismatch on every publish.
  baked="$(sed -n '/BEGIN PGP PUBLIC KEY BLOCK/,/END PGP PUBLIC KEY BLOCK/p' "$HERE/punktfunk-sysext.sh" \
           | sed "s/^FEED_KEY='//; s/'\$//" \
           | GNUPGHOME="$home" gpg --batch --quiet --with-colons --import-options show-only --import 2>/dev/null \
           | awk -F: '/^fpr:/{print $10; exit}')"
  if [ -z "$baked" ] || [ "$keyid" != "$baked" ]; then
    echo "signing key $keyid does not match the key baked into punktfunk-sysext.sh ($baked)" >&2
    echo "-> rotate BOTH (packaging/rpm/README.md), or clients cannot verify this feed." >&2
    rm -rf "$home"; return 1
  fi
  GNUPGHOME="$home" gpg --batch --yes --armor --detach-sign --local-user "$keyid" \
    --output "$SIG" "$SUMS"
  rm -rf "$home"
  echo "signed SHA256SUMS with $keyid"
}

# require_signature — on a v* tag an unsigned feed is a hard stop, exactly like sign-rpms.sh:
# clients refuse unsigned feeds, so publishing one would strand every box on the stable channel.
require_signature() {
  case "${GITHUB_REF:-}" in
    refs/tags/v*)
      echo "release build (${GITHUB_REF}) but the sysext feed could not be signed — aborting." >&2
      exit 1 ;;
  esac
  # Deliberately mode-neutral wording: in --seal mode nothing is published at all, and a log line
  # claiming otherwise is the kind of thing that costs an hour six months from now.
  echo "WARNING: $FEED left UNSIGNED (no usable RPM_GPG_PRIVATE_KEY); clients will refuse it." >&2
}

# --seal: re-sign whatever manifest the feed already has, no image, no pruning.
if [ "$SEAL" = 1 ]; then
  curl -fsS "${AUTH[@]}" -o "$SUMS" "$BASE/SHA256SUMS" \
    || { echo "no SHA256SUMS at $BASE — nothing to seal" >&2; exit 1; }
  sign_manifest || require_signature
  if [ -f "$SIG" ]; then
    curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/SHA256SUMS.asc" || true
    curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$SIG" "$BASE/SHA256SUMS.asc"
    echo "sealed $BASE ($(wc -l <"$SUMS") image(s))"
  fi
  exit 0
fi

FNAME="$(basename "$RAW")"
SHA="$(sha256sum "$RAW" | cut -d' ' -f1)"

# Merge into the existing manifest: drop any prior line for this filename, append ours.
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

# Sign the finished manifest BEFORE anything is uploaded — a signing failure on a release must
# abort while the live feed is still whole, not halfway through being replaced.
sign_manifest || require_signature

# Upload order keeps consumers consistent: image first, then the manifest referencing it, then its
# signature, then prune deletions (already absent from the manifest). A client that catches the
# window between the manifest and its signature sees an unsigned feed and refuses — it does not see
# a manifest vouched for by a stale signature, which is why the .asc goes last and never first.
# Delete-before-put makes workflow re-runs idempotent (the registry 409s on duplicate filenames;
# first-publish 404s are fine).
curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/$FNAME" || true
curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$RAW" "$BASE/$FNAME"
curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/SHA256SUMS" || true
curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$SUMS" "$BASE/SHA256SUMS"
if [ -f "$SIG" ]; then
  curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/SHA256SUMS.asc" || true
  curl -fsS -o /dev/null "${AUTH[@]}" --upload-file "$SIG" "$BASE/SHA256SUMS.asc"
fi
for f in "${PRUNE[@]:-}"; do
  [ -n "$f" ] && { echo "pruning $f"; curl -fsS -o /dev/null "${AUTH[@]}" -X DELETE "$BASE/$f" || true; }
done
echo "published $FNAME -> $BASE ($(wc -l <"$SUMS") image(s) in the feed)"

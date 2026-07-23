#!/usr/bin/env bash
# Announce a stable Punktfunk release to the Discord #releases channel.
#
# Model: release notes live in the repo at docs/releases/<tag>.md (authored during the version
# bump, BEFORE the tag is pushed — see docs/releases/README.md). CI seeds the Gitea release body
# from that file at creation, so the release is never noteless. This script is the deliberate
# "go" step (workflow_dispatch, .gitea/workflows/announce.yml): once every platform's CI is green
# it re-asserts the notes file over the live release, then posts a formatted embed to Discord.
#
# Usage:   scripts/ci/discord-announce.sh <tag>       # e.g. v0.18.0
# Env:
#   GITHUB_SERVER_URL        https://git.unom.io                 (Gitea Actions sets it)
#   GITHUB_REPOSITORY        unom/punktfunk                      (Gitea Actions sets it)
#   GITEA_TOKEN              PAT with repo write scope (from secrets.REGISTRY_TOKEN)
#   DISCORD_RELEASE_WEBHOOK  the #releases channel webhook URL   (from secrets)
#   ALLOW_PRERELEASE         "1" to permit announcing a -rc/pre-release tag (default: refuse)
#
# Requires: curl + python3 (both present on the ubuntu-24.04 runner).
set -euo pipefail

TAG="${1:?usage: discord-announce.sh <tag>}"
: "${GITHUB_SERVER_URL:?}" "${GITHUB_REPOSITORY:?}" "${GITEA_TOKEN:?}" "${DISCORD_RELEASE_WEBHOOK:?}"

# Stable-only guard: a `-` in the tag marks a pre-release (v0.2.0-rc1). Refuse unless explicitly
# allowed, so an rc dispatched by accident never pings the community.
case "$TAG" in
  *-*)
    case "$(printf '%s' "${ALLOW_PRERELEASE:-0}" | tr '[:upper:]' '[:lower:]')" in
      1|true|yes) : ;;
      *) echo "discord-announce: '$TAG' looks like a pre-release; set ALLOW_PRERELEASE=1 to override" >&2
         exit 1 ;;
    esac ;;
esac

_here="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
API="$GITHUB_SERVER_URL/api/v1/repos/$GITHUB_REPOSITORY"

# Resolve the release object created by the build workflows (id, html_url, published_at).
release_json="$(curl -fsS "$API/releases/tags/$TAG" -H "Authorization: token $GITEA_TOKEN")"
RID="$(printf '%s' "$release_json" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("id",""))')"
[ -n "$RID" ] || { echo "discord-announce: no release found for tag '$TAG' — has CI created it yet?" >&2; exit 1; }

# Re-assert the notes file over the live release (source of truth), covering an edit after the
# release object was first created. No-op if docs/releases/<tag>.md is absent.
# shellcheck source=scripts/ci/gitea-release.sh
. "$_here/gitea-release.sh"
apply_release_notes "$RID" "$TAG"

# Build and post the Discord embed. The description is the notes' lead-in (everything before the
# first `## ` section header), trimmed to a readable length, with a link to the full release page
# for the sections + downloads. Falls back to the release's own body if there is no notes file.
NOTES_PATH="$(_release_notes_path "$TAG" || true)"
payload_file="$(mktemp)"
trap 'rm -f "$payload_file"' EXIT

RELEASE_JSON="$release_json" NOTES_PATH="$NOTES_PATH" TAG="$TAG" python3 - "$payload_file" <<'PY'
import json, os, sys

rel = json.loads(os.environ["RELEASE_JSON"])
tag = os.environ["TAG"]
version = tag[1:] if tag.startswith("v") else tag
url = rel.get("html_url") or ""
published = rel.get("published_at") or rel.get("created_at") or ""

notes_path = os.environ.get("NOTES_PATH") or ""
body = ""
if notes_path and os.path.isfile(notes_path):
    with open(notes_path, encoding="utf-8") as f:
        body = f.read()
else:
    body = rel.get("body") or ""

# Lead-in = text before the first `## ` section header (the intro paragraphs); fall back to the
# whole body when the notes open straight into a section.
lead = body
idx = body.find("\n## ")
if idx != -1:
    lead = body[:idx]
lead = lead.strip()
if not lead:
    lead = body.strip()

LIMIT = 1800
if len(lead) > LIMIT:
    cut = lead.rfind("\n\n", 0, LIMIT)
    if cut < LIMIT // 2:
        cut = lead.rfind(" ", 0, LIMIT)
    if cut <= 0:
        cut = LIMIT
    lead = lead[:cut].rstrip() + " …"

description = lead
if url:
    description = (description + "\n\n" if description else "") + \
        f"**[⬇️ Download & full release notes →]({url})**"

embed = {
    "title": f"Punktfunk {version}",
    "color": 0x6C5BF3,  # --pf-brand deep violet
    "description": description,
    "footer": {"text": "Punktfunk • git.unom.io"},
}
if url:
    embed["url"] = url
if published:
    embed["timestamp"] = published

with open(sys.argv[1], "w", encoding="utf-8") as f:
    json.dump({"embeds": [embed]}, f)
PY

curl -fsS -o /dev/null -X POST "$DISCORD_RELEASE_WEBHOOK" \
  -H 'Content-Type: application/json' --data-binary @"$payload_file"
echo "discord-announce: posted Punktfunk $TAG to #releases (release $RID)"

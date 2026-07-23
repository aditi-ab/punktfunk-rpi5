# shellcheck shell=bash
# Shared Gitea Release helpers for the punktfunk CI workflows (Linux + macOS runners).
#
# Source this file, then call ensure_release / upsert_asset. It replaces the three
# copy-pasted inline blocks that used to live in release.yml / flatpak.yml / decky.yml,
# and fixes a latent bug those had: the bare asset POST returns 409 if an asset with the
# same name already exists, so re-running a workflow — or reusing the rolling `canary`
# release with stable filenames — would fail. upsert_asset deletes the old asset first.
#
# Callers run under Gitea Actions' default `bash -eo pipefail`, so a non-zero return from
# these functions aborts the step (the desired behaviour on a real failure).
#
# Env (Gitea Actions sets the first two automatically in every step):
#   GITHUB_SERVER_URL   e.g. https://git.unom.io
#   GITHUB_REPOSITORY   e.g. unom/punktfunk
#   GITEA_TOKEN         a PAT with repository (release) write scope — set from secrets.REGISTRY_TOKEN
#                       (the same PAT the package uploads use; it must carry `write:repository`,
#                       not only `write:package`, or the release-asset POST 403s)
#
# Requires: curl + python3 (python3 is already a proven dependency on every runner that
# attaches releases today — macOS, the fedora flatpak container, the node:bookworm decky
# image; the .deb runner installs it alongside its other apt deps).

_gitea_api() { printf '%s/api/v1/repos/%s' "${GITHUB_SERVER_URL:?}" "${GITHUB_REPOSITORY:?}"; }

# Tiny JSON / URL helpers. python3 reads the TOP-LEVEL "id" only, so there is no ambiguity
# with the nested author.id / assets[].id fields a string-grep would trip over.
_json_id() { python3 -c 'import json,sys;print(json.load(sys.stdin).get("id",""))' 2>/dev/null; }
_json_asset_id() {
  python3 -c 'import json,sys
want=sys.argv[1]
for a in json.load(sys.stdin):
    if a.get("name")==want:
        print(a.get("id",""));break' "$1" 2>/dev/null
}
_urlencode() { python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=""))' "$1"; }

# _release_notes_path TAG
#   Print the path of the in-repo release notes for TAG (docs/releases/<TAG>.md) IFF it exists,
#   else print nothing. This file is the single source of truth for a stable release's body
#   (authored as part of the version bump, before the tag is pushed — see docs/releases/README.md),
#   so the Gitea release is born WITH its notes instead of being PATCHed noteless-then-late.
#   Resolves relative to this script (scripts/ci/ -> repo root); canary/rc tags have no such file,
#   which is intended (they get no curated body).
_release_notes_path() {
  local root notes
  root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/../.." && pwd)"
  notes="$root/docs/releases/$1.md"
  [ -f "$notes" ] && printf '%s' "$notes"
}

# ensure_release TAG NAME PRERELEASE [TARGET_COMMITISH]
#   Idempotently create (or fetch) the release for TAG; prints its numeric id on stdout.
#   PRERELEASE is "true", "false", or "auto" — auto marks it a prerelease iff TAG carries a
#   `-` pre-release suffix (e.g. v0.2.0-rc1), so an rc never becomes the repo's "Latest" release
#   (Gitea's /releases/latest surfaces the newest non-prerelease). TARGET_COMMITISH (optional)
#   creates the git tag if it does not exist yet — for a `vX.Y.Z` release the tag already exists
#   (it is the trigger), so TARGET is omitted and create-vs-fetch hinges on the release object.
ensure_release() {
  local tag="${1:?tag}" name="${2:?name}" prerelease="${3:?prerelease}" target="${4:-}"
  local api body id
  if [ "$prerelease" = auto ]; then
    case "$tag" in *-*) prerelease=true ;; *) prerelease=false ;; esac
  fi
  api="$(_gitea_api)"
  # Build the create payload with python3 so the (multi-line, quote-bearing) release body from
  # docs/releases/<tag>.md is JSON-escaped correctly. Whichever workflow wins the create race thus
  # sets the body ATOMICALLY at creation; the losers 409 and fall through to the fetch-by-tag path
  # below (which never touches the body). No notes file (canary/rc) -> no body key -> empty body.
  local notes; notes="$(_release_notes_path "$tag")"
  body=$(TAG="$tag" NAME="$name" PRERELEASE="$prerelease" TARGET="$target" NOTES_FILE="$notes" \
    python3 - <<'PY'
import json, os
d = {"tag_name": os.environ["TAG"], "name": os.environ["NAME"],
     "prerelease": os.environ["PRERELEASE"] == "true"}
if os.environ.get("TARGET"):
    d["target_commitish"] = os.environ["TARGET"]
nf = os.environ.get("NOTES_FILE") or ""
if nf:
    with open(nf, encoding="utf-8") as f:
        d["body"] = f.read()
print(json.dumps(d))
PY
)
  # Try to create. On any failure (almost always "release already exists"), fall back to
  # fetching it by tag. Either path MUST yield an id, or we error loudly — so a 401/scope
  # problem can't masquerade as a successful no-op.
  id=$(curl -fsS -X POST "$api/releases" \
         -H "Authorization: token ${GITEA_TOKEN:?}" -H 'Content-Type: application/json' \
         -d "$body" 2>/dev/null | _json_id || true)
  if [ -z "$id" ]; then
    id=$(curl -fsS "$api/releases/tags/$tag" \
           -H "Authorization: token ${GITEA_TOKEN:?}" 2>/dev/null | _json_id || true)
  fi
  if [ -z "$id" ]; then
    echo "gitea-release: could not create or find a release for tag '$tag'" >&2
    return 1
  fi
  printf '%s' "$id"
}

# upsert_asset RELEASE_ID FILE [NAME]
#   Attach FILE to the release, replacing any existing asset of the same name first so that
#   re-runs and rolling canary re-uploads are idempotent (a plain POST 409s on a dup name).
upsert_asset() {
  local rid="${1:?release id}" file="${2:?file}" name="${3:-}"
  local api existing
  [ -n "$name" ] || name="$(basename "$file")"
  [ -f "$file" ] || { echo "gitea-release: asset file not found: $file" >&2; return 1; }
  api="$(_gitea_api)"
  existing=$(curl -fsS "$api/releases/$rid/assets" \
               -H "Authorization: token ${GITEA_TOKEN:?}" 2>/dev/null \
             | _json_asset_id "$name" || true)
  if [ -n "$existing" ]; then
    curl -fsS -o /dev/null -X DELETE "$api/releases/$rid/assets/$existing" \
      -H "Authorization: token ${GITEA_TOKEN:?}" || true
  fi
  curl -fsS -o /dev/null -X POST "$api/releases/$rid/assets?name=$(_urlencode "$name")" \
    -H "Authorization: token ${GITEA_TOKEN:?}" \
    -F "attachment=@$file"
  echo "gitea-release: uploaded '$name' -> release $rid"
}

# apply_release_notes RELEASE_ID TAG
#   Force the release body to match docs/releases/<TAG>.md (the source of truth), if that file
#   exists — a no-op otherwise. PATCHes ONLY the body, so name/prerelease/assets are preserved
#   (Gitea has no partial-update footgun here; sending just {"body":...} leaves everything else).
#   ensure_release already seeds the body at creation; this exists so the announce step can
#   re-assert the file over the live release right before publishing — covering the case where the
#   notes file was edited after the release object was first created (e.g. a tag re-point).
apply_release_notes() {
  local rid="${1:?release id}" tag="${2:?tag}" api notes payload
  notes="$(_release_notes_path "$tag")"
  [ -n "$notes" ] || { echo "gitea-release: no docs/releases/$tag.md — leaving body as-is"; return 0; }
  api="$(_gitea_api)"
  payload=$(NOTES_FILE="$notes" python3 -c \
    'import json,os;print(json.dumps({"body":open(os.environ["NOTES_FILE"],encoding="utf-8").read()}))')
  curl -fsS -o /dev/null -X PATCH "$api/releases/$rid" \
    -H "Authorization: token ${GITEA_TOKEN:?}" -H 'Content-Type: application/json' \
    -d "$payload"
  echo "gitea-release: synced release $rid body from $notes"
}

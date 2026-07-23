# Release notes

One file per **stable** release: `docs/releases/vX.Y.Z.md`. Its contents become the Gitea release
body **verbatim** and are the source of the Discord `#releases` announcement.

## Why this exists

Releases used to be created by CI with an **empty body**; the notes were pasted in by hand
afterward. That left a window where the release — and anything announcing it — carried no notes.
Now the notes are authored **before the tag is pushed**, as part of the version bump, so the
release is born complete and the announcement always has something to say.

## The flow

1. **Write the notes.** Add `docs/releases/vX.Y.Z.md` in the same commit (or PR) as the version
   bump. Copy `TEMPLATE.md` and fill it in. This file is the single source of truth for the body.
2. **Tag & push.** `git tag -a vX.Y.Z … && git push origin vX.Y.Z` fans out to the build
   workflows. Whichever one wins the create race seeds the release body from this file
   (`scripts/ci/gitea-release.sh` → `ensure_release`, and its PowerShell twin). The release page
   shows the notes immediately.
3. **Wait for green.** Let every platform's CI finish and go green.
4. **Announce.** Dispatch the `announce` workflow (`.gitea/workflows/announce.yml`) with the tag.
   It re-asserts this file over the live release (so any late edit wins) and posts an embed to
   Discord `#releases`. Pressing "go" is the quality gate — a half-built release is never
   announced. Stable-only; a `-rc` tag is refused unless `allow_prerelease=true`.

Editing the notes after the tag is fine: update this file, then re-run step 4 (or PATCH the body
via the API) — the announce step always re-syncs from the file, so the file stays authoritative
even across a tag re-point.

Canary / `-rc` builds have **no** file here on purpose: they get no curated body and are not
announced.

## Format

Match the house style (see any recent `vX.Y.Z.md`):

- Open with a **wire-compatibility** line — *"Wire-compatible with X.Y.x — existing pairings and
  clients keep working."* — plus a one-sentence fallback/negotiation note. This lead-in (all text
  before the first `##` header) is what the Discord embed shows, so make it a real summary.
- Then `## Section` headers grouping the changes, with **bold lead-in** bullets.
- Be concrete: env vars, ABI/protocol versions, on-glass-verified hardware, platform scope.

The short annotated-**tag** message stays separate and short (a headline + a paragraph); it is the
tag object's message, not this file.

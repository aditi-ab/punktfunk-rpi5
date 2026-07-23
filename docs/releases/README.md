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

## Voice & format

**Write for the people who USE Punktfunk to stream their games and desktops — not for the people who
build it.** A non-engineer should finish knowing what's new and whether it affects them; an engineer
should never be confused or forced to decode internals. (See any recent `vX.Y.Z.md` for the target.)

1. **Lead with the benefit.** Each entry = what the user can now *do*, what now *works*, or what
   stopped *going wrong* — in their words. Implementation is not the story.
2. **No internal vocabulary in the body.** No protocol/message names, code type names, hex codes or
   hardware IDs, crate/component names, or API symbols. Translate any essential detail to plain
   language. Name things users recognize (iPad, Apple Pencil, Steam Deck, Android TV, the Windows
   sign-in screen) — not subsystems.
3. **Group as New / Improved / Fixed**, each a bold one-line lead-in + a tight plain explanation.
   Skimmable. The lead-in text before the first `##` is what the Discord announcement shows, so make
   it a real, plain-language summary.
4. **Be specific and honest** — no vague "various improvements"; a reader should know exactly what
   changed.
5. **Compatibility line up top, in plain terms:** can they update one side at a time? does their
   existing setup keep working? No version numbers in the lead.
6. **All protocol / ABI / driver / embedder detail goes in ONE `## Under the hood (for developers)`
   section at the very bottom** — the only place internal names and version numbers belong, clearly
   optional. The old dense engineering style survives only there.

The short annotated-**tag** message stays separate and short (a headline + a paragraph); it is the
tag object's message, not this file.

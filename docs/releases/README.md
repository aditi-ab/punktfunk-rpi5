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

## Google Play "What's new": `whatsnew/vX.Y.Z.txt`

Play shows its own release notes on the Play Store listing and in the Play Store app, and caps
them at **500 characters per language** — the `vX.Y.Z.md` body is two orders of magnitude too
long, so it gets its own short file: `docs/releases/whatsnew/vX.Y.Z.txt`.

Write it for a **phone/TV user**, not a host operator: only what changed in the Android app is
worth their 500 characters. Plain text (Play renders no markdown), one `•` bullet per line, same
voice rules as below. Copy `whatsnew/TEMPLATE.txt`.

**A `vX.Y.Z` tag without this file fails the android job before it builds.** This is a hard gate,
not a warning, because the failure it prevents is silent: when the file is missing Play does not
show an empty "What's new" — it **carries the previous release's text onto the new version**, so
the store listing describes a build nobody is getting, and nothing surfaces that but reading the
listing. It is the same shape as the v0.22.3 notes announcing a feature that release never
contained. The gate also rejects a file byte-identical to another release's, which is that bug
reached by copy-paste instead of by omission.

The gate runs first in the job, so a miss costs a second and leaves nothing half-published —
no build, no assets on the Gitea release, nothing on Play. Two more checks sit downstream:
`play-upload.py` refuses text over the 500-char cap (printing the real count) before it uploads,
because the API only rejects oversized notes at commit, by which point the AAB is already on Play.

Canary is exempt: it has no curated notes, and Play reusing text for internal testers costs
nothing.

Same freeze rule as the notes: once the tag exists, this file is the record of what that
versionCode shipped.

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

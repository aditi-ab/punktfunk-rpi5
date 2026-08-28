# Punktfunk writing standards

House style for **commits**, **changelogs**, and **comments**.

Audited against [unom/punktfunk](https://git.unom.io/unom/punktfunk) on 28 August 2026. The engineering in that tree is careful. The writing is careful too. The problem is the medium: git log, CHANGELOG.md, and rustdoc are being asked to hold design reviews.

This document is the rulebook. The companion site shows the originals next to rewrites.

---

## 0. The one sentence

**Put each fact where someone can find it later.**

| Fact | Lives in |
| --- | --- |
| What changed, in one greppable line | Commit **subject** |
| Why it changed, in a short body | Commit **body** (and the PR if it needs a diagram) |
| What a user can do now | `docs/releases/vX.Y.Z.md` |
| What an embedder must do | `CHANGELOG.md` |
| The investigation, measurements, rejected paths | Pull request and `docs/adr/` |
| The invariant that must remain true | Comment, type, or test |

If you are writing a novel, you are in the wrong file.

---

## 1. Commits

### Shape

```
type(scope): imperative summary

Why it failed for a user. What was actually wrong.
What you changed. Wrap at 72.

Fixes #123
```

- **50 characters** is the aim for the subject. **72 is the hard cap.**
- No trailing period on the subject.
- Imperative, present tense: `keep`, `skip`, `advertise` — matching `git merge` / `git revert`.
- One logical change per commit. A subject with “and” is two commits, or one theme named as a theme.

### Types

| Type | Use |
| --- | --- |
| `feat` | User-visible capability that did not exist |
| `fix` | A bug. Put the symptom in the body |
| `docs` | Docs, comments-as-docs, release notes. No behaviour change |
| `refactor` | Same behaviour, different shape |
| `perf` | Same behaviour, cheaper. Name the metric if you have one |
| `test` | Tests only |
| `chore` | Deps, version bumps, generated files |
| `ci` | Pipelines and gates |
| `security` | Trust-boundary changes |

Scopes are subsystem names a newcomer would grep: `host`, `hyprland`, `mdns`, `abr`, `console`, `gamestream`, `android`, `web`, `core`.

### Why the current style fails

Recent subjects on main (28 Aug 2026):

| As written | Problem | Rewrite |
| --- | --- | --- |
| The retry loop stops eating the restore that re-lights the desk | Metaphor, no scope, 69 chars of plot | `fix(host/hyprland): keep topology restore across pipeline retries` |
| The advert names its address, and the client stops rolling dice on the rest | Pun, two clauses, unsearchable | `fix(mdns): advertise a primary address so clients stop guessing` |
| The streamed head can be focused on a Lua box, which is what makes it produce frames | Relative clause, slang | `fix(hyprland): focus the virtual head on Lua-configured compositors` |
| The Omarchy box says otherwise: five things the plan got wrong, measured | Lab-notebook title | `docs(omarchy): correct five host-plan assumptions from measured hardware` |
| The ticket parser proves its own segments exist | Anthropomorphic, no scope | `fix(web): narrow ticket-parser types so segments are proven present` |

The last 200 subjects average **101 characters**. Longest: **211**. At the time of the audit, CONTRIBUTING.md’s entire commit rule was “end with the Co-Authored-By trailer.”

### Body rules

- Three short paragraphs is enough: user-visible failure, actual cause, what you changed.
- Wrap at 72.
- Do not paste CI logs, soak minutes, GPU SKUs, or RFC section numbers. Link the PR or the ADR.
- No `Co-Authored-By` trailer in this repo. Attribution is off, because Gitea 1.27 promotes
  the trailer to a second participant on the commit page. Credit a co-author in prose.

### Pull requests

The forensic essay is valuable. **Put it on the PR.** The merge commit subject is the conventional subject of the work, not the essay’s headline. Gitea PR titles become merge subjects — write the PR title as a conventional commit.

---

## 2. Changelogs

Punktfunk already split the two audiences at v0.25.0. Keep that. Stop writing both files in the same voice.

### `docs/releases/vX.Y.Z.md` — people who stream

Keep the existing template:

1. Compatibility line (plain language, no ABI numbers)
2. `## TL;DR` — three to six one-line bullets
3. `## Before you update` — only if the reader must act
4. `## New` / `## Improved` / `## Fixed` / `## Security`
5. `## For developers` — one link to CHANGELOG.md at the **tag**

Voice rules already in `docs/releases/README.md` are correct. Follow them. Do not narrate lab sessions (“we watched one do exactly that on a local network, unprompted”) in the user notes. Say the default changed, and that clients that support it opt in.

### `CHANGELOG.md` — embedders, packagers, plugin authors

Keep:

- Newest first
- The **version table** (every row, including unchanged)
- Breaking changes with an action

Replace:

- Sentence-headings (“The auto-bitrate overhaul (four phases)”)
- 454-line sections (v0.32.0)
- Field-log storytelling, soak evidence, RFC chapter numbers

Use [Keep a Changelog](https://keepachangelog.com/) categories:

```markdown
## [0.32.0] — 2026-08-27

ABI 25 → 26 (additive). Wire protocol stays 2.

### Breaking
- **Bitrate means the wire budget.** `live_bitrate` is no longer encoder rate.
  FEC, framing and audio used to ride on top. Embedders that treated it as
  encoder rate must stop.

### Added
- `punktfunk_connect_opts` replaces the `connect_ex*` ladder. Every `ex` remains.

### Fixed
- Automatic bitrate treated a still picture as congestion.

### Security
- Authenticated console sessions could reach pairing without the console password.
  Pairing grants launch. The routes now re-ask.
```

### Length

| Release | Target |
| --- | --- |
| Patch | One screen |
| Minor | Two screens |
| Longer than that | Split, or link an ADR |

`CHANGELOG.md` is currently **6,519 lines** for eight versions. That is not a changelog. It is an archive of design reviews. Move the reviews to `docs/adr/` and link them.

---

## 3. Comments

### Why

Comments exist for the non-local reason: the invariant, the trap, the rejected alternative. Names and types are the what. If the next five lines already say it, delete the comment.

### Module rustdoc

A module header is a map, 8–20 lines:

1. What it is
2. The public contract
3. How to choose / pin it
4. Where deeper evidence lives

It is not a program-of-record. `crates/pf-client-core/src/video_vk_native.rs` opens with **8,940 characters** of `//!` before the first item: WP-C, M3 WP-2, M7, a 92-minute soak, an RTX 5070 Ti. That history is already in git and in `design/client-native-decode.md`. rustdoc should point there, not duplicate it.

### A comment is not a spec

The 2026-08-25 security review’s serious findings were documented promises the code had stopped keeping. One commit even named it: `the comments were the spec, and the code had drifted`.

If a boundary matters, encode it: type, test, assertion, parser. Comments explain the boundary. They do not constitute it.

### SAFETY, FFI, concurrency

Keep these exact. This is already good:

```rust
// SAFETY: the clipboard is open (the `Clip` guard); the handle returned is
// BORROWED from the clipboard and stays valid while it is open, so it is
// never freed here.
```

Do not restyle that into a narrative of how the bug was found.

### History

Dates, SKUs, soak durations, milestone codes, and “this used to” sentences go stale in the file. `git blame` and ADRs keep them honest.

### Prefer a name

`stash_topology_restore_first_wins` says what a twelve-line comment would. If you need a comment to explain a name, rename.

---

## 4. Checklist (every PR)

- [ ] Subject is `type(scope): summary`, ≤ 72 characters, imperative, no period
- [ ] Subject names a subsystem a newcomer would grep
- [ ] Body is why, not the investigation (investigation is on the PR)
- [ ] One logical change; no “and” holding two fixes together
- [ ] User-facing fact updated in `docs/releases/` or the docs-site page that owns it
- [ ] Embedder-facing fact is a bullet in CHANGELOG.md, not a new chapter
- [ ] New comments state an invariant or a trap, not a recap of the diff
- [ ] Module rustdoc still fits on one screen
- [ ] No new comment that is the only enforcement of a trust boundary

---

## 5. What this does not ask

- It does not ask anyone to write less carefully. It asks them to file the care in the right place.
- It does not ban long writing. It bans long writing in git subjects and changelog bullets.
- It does not replace `docs/releases/README.md`. That voice guide stays. This document covers the three surfaces that guide does not.

---

## 6. Adoption

Adopted 28 August 2026. Three steps landed with this file:

1. This file is `docs/writing.md`, linked from CONTRIBUTING.md and AGENTS.md.
2. CONTRIBUTING.md’s commit rule points here.
3. `.gitea/PULL_REQUEST_TEMPLATE.md` asks for a conventional PR title, because a Gitea PR
   title becomes the merge subject.

Two steps are ongoing, and deliberately not a sweep:

4. Write each **new** `CHANGELOG.md` section in Keep a Changelog form. The archive under the
   older headings stays as it is; do not rewrite it.
5. **New** module files get the short rustdoc. Do not rewrite every existing header in one
   pass — rewrite one when you are already changing that module.

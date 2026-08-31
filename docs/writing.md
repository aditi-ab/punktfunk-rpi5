# Punktfunk writing standards

House style for **commits**, **changelogs**, and **comments**.
`scripts/ci/check-writing.sh` fails a PR that breaks the caps below.

## 0. Where facts go

| Fact | Lives in |
| --- | --- |
| What changed, in one greppable line | Commit **subject** |
| Why it changed | Commit **body** (PR if it needs a diagram) |
| What a user can do now | `docs/releases/vX.Y.Z.md` |
| What an embedder must do | `CHANGELOG.md` |
| Investigation, measurements, rejected paths | Pull request and `docs/adr/` |
| Invariant that must remain true | Comment, type, or test |

---

## 1. Commits

```
type(scope): imperative summary

Why it failed for a user. What was actually wrong.
What you changed. Wrap at 72.

Fixes #123
```

- **50 characters** aim. **72 hard cap.** No trailing period.
- Imperative, present tense: `keep`, `skip`, `advertise`.
- One logical change. A subject with “and” or a semicolon is two commits.
- Body: at most three short paragraphs, **200 words**.
- Scope is a subsystem a newcomer greps: `host`, `hyprland`, `mdns`, `abr`, `console`,
  `gamestream`, `android`, `web`, `core`.
- No `Co-Authored-By`. Gitea 1.27 shows the trailer as a second committer. Credit in prose.
- Field logs, SKUs, soak minutes, rejected paths, RFC numbers: **PR body**, not the commit.

CI checks every commit on the PR: missing `type(scope):`, subject starting `The …`,
subject over 72 characters, body over 200 words, or a `Co-Authored-By` trailer fails the job.

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

Bad: `The retry loop stops eating the restore that re-lights the desk`
Good: `fix(host/hyprland): keep topology restore across pipeline retries`

The Gitea PR title is the merge subject. Write it as the conventional subject.

---

## 2. Changelogs

Two audiences. Do not write both files in the same voice.

### `docs/releases/vX.Y.Z.md` — people who stream

Existing template:

1. Compatibility line (plain language, no ABI numbers)
2. `## TL;DR` — three to six one-line bullets
3. `## Before you update` — only if the reader must act
4. `## New` / `## Improved` / `## Fixed` / `## Security`
5. `## For developers` — one link to CHANGELOG.md at the **tag**

Voice: `docs/releases/README.md`. Do not narrate a lab session. Say what the default is.

### `CHANGELOG.md` — embedders, packagers, plugin authors

Newest first. Version table (every row). Breaking changes with an action.
New sections only; do not edit older ones. User notes stay in `docs/releases/`.

[Keep a Changelog](https://keepachangelog.com/) categories:

```markdown
## [0.32.0] — 2026-08-27

ABI 25 → 26 (additive). Wire protocol stays 2.

### Breaking
- **Bitrate means the wire budget.** `live_bitrate` is no longer encoder rate.
  Embedders that treated it as encoder rate must stop.

### Added
- `punktfunk_connect_opts` replaces the `connect_ex*` ladder. Every `ex` remains.

### Fixed
- Automatic bitrate treated a still picture as congestion.

### Security
- Authenticated console sessions could reach pairing without the console password.
  Pairing grants launch. The routes now re-ask.
```

### Bullet shape

Two sentences per bullet.

1. What changed.
2. What the reader must do, if anything.

The bold lead is a noun or an API name.
Version-table Notes cells are one clause. If it needs a paragraph, it is a Breaking bullet.

Do not put in `CHANGELOG.md`:

- Metaphor
- Field measurements (SKU, soak minutes, underrun counts)
- Causation chains ("so… which means… because…")
- Why you did not take the other path
- Behaviour restorations under **Breaking** — those are **Fixed**

Those belong on the PR or in `docs/adr/` / `design/`. Link them.

### Length

| Release | Target |
| --- | --- |
| Patch | One screen |
| Minor | Two screens |
| Longer than that | Split, or link an ADR |
| Newest section (CI) | 160 lines (`scripts/ci/check-writing.sh`) |

Older sections are not counted.

---

## 3. Comments

A comment is the non-local reason: the trap, the lifetime, the unit on a magic number.
Names and types are the what. If the next five lines already say it, delete the comment.

### Caps

- `//` : at most four lines (CI fails at six).
- `//!` / `///` module map: what it is, the contract, how to pin it, where evidence lives.
  8–20 lines (CI fails at 24).
- Keep `// SAFETY:` and FFI/lifetime proofs exact.
- A comment never enforces a trust boundary — a type, a test or an assertion does.

CI counts comments this diff opened (the comment itself, or the comment above an item
whose body changed). If it fails: shorten. Do not add `writing-ok` unless the extra
lines are a SAFETY/lifetime trap.

### When you touch a function, rewrite its comment

Do not sweep the file. Do not open a comments-only PR.

1. Restates the next five lines → delete.
2. Field report (date, device, OS, log line) → delete. That is the commit body or the PR.
3. Lab nickname (`ponytail:`), second copy of the commit body → delete.
4. Lifetime / weak-ref / generation-vs-session / why this number → keep, one to three lines.

A constant’s comment is why the number, not the incident that produced it.

Bad (in the file): `Field 2026-08-28, iPad Pro / iOS 27 over Tailscale: …`
Good (in the file): `250 ms ≈ 30 refreshes at 120 Hz. A miss freezes the picture.`
Good (in the commit body): the iPad, the log lines, the reconnect.

### Module rustdoc

8–20 lines: what it is, the public contract, how to pin it, where evidence lives.
Point at `design/` / `docs/adr/`. Do not paste the investigation into `//!`.

### SAFETY

Keep proofs exact:

```rust
// SAFETY: the clipboard is open (the `Clip` guard); the handle returned is
// BORROWED from the clipboard and stays valid while it is open, so it is
// never freed here.
```

Dates, SKUs, soak durations, and “this used to” go stale. Prefer a name:
`stash_topology_restore_first_wins` over a twelve-line comment.

---

## 4. Checklist (every PR)

- [ ] Subject is `type(scope): summary`, ≤ 72 characters, imperative, no period
- [ ] Subject names a subsystem a newcomer would grep
- [ ] Body is why, not the investigation (investigation is on the PR)
- [ ] One logical change; no “and” holding two fixes together
- [ ] User-facing fact updated in `docs/releases/` or the docs-site page that owns it
- [ ] Embedder-facing fact is a bullet in CHANGELOG.md, not a new chapter
- [ ] New comments state an invariant or a trap, not a recap of the diff
- [ ] Module rustdoc still fits on one screen (CI fails a touched `//!` / `///` at 24 lines)
- [ ] Touched `//` blocks are at most four lines (CI fails at six), except SAFETY proofs
- [ ] No new comment that is the only enforcement of a trust boundary
- [ ] `scripts/ci/check-writing.sh` is green

---

## 5. Adoption

Do not rewrite old `CHANGELOG.md` sections. Do not sweep existing module rustdoc.
New sections and files follow this file. Rewrite a comment when you already open that function.

This document does not replace `docs/releases/README.md`.

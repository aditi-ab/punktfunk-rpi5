# AGENTS.md

Guidance for coding agents working in this repository.

## Writing

`scripts/ci/check-writing.sh` fails the PR. If it fails: shorten. Do not add `writing-ok`
unless the extra lines are a SAFETY/lifetime trap. Rules: `docs/writing.md`.

### Commits

- `type(scope): summary` — imperative, ≤72 characters, no trailing period, no `and`,
  no semicolon. Scope is a subsystem (`host`, `hyprland`, `abr`).
- Body: three short paragraphs max, **200 words**. Why only. No `Co-Authored-By`.
- Measurements and rejected paths go on the PR.
- Gitea PR title is the merge subject. Same shape as the commit.

Bad: `The retry loop stops eating the restore that re-lights the desk`
Good: `fix(host/hyprland): keep topology restore across pipeline retries`

### Changelog

Newest `CHANGELOG.md` section only. Two sentences per bullet: what changed, then what
the reader must do. Bold lead is a noun or an API name. Fail at 160 lines.

### Comments

Touch a function → rewrite its comment in the same diff. Do not sweep the file.

- `//` : four lines (fail at 6). `//!` / `///` : 8–20 (fail at 24).
- Keep `// SAFETY:` exact.
- Delete restatement, field reports, `ponytail:`.
- Keep lifetime, weak-ref, why this number. One to three lines.
- A comment never enforces a trust boundary.

Bad (in the file): `Field 2026-08-28, iPad Pro / iOS 27 over Tailscale: …`
Good (in the file): `250 ms ≈ 30 refreshes at 120 Hz. A miss freezes the picture.`

## Agent skills

### Issue tracker

Issues live as Gitea issues in `unom/punktfunk` on `git.unom.io`, driven by the `gitea` MCP server
(`gh`/`glab`/`tea` do not work here), and every write needs the user's go-ahead first.
See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical roles, each label string equal to its name — `needs-triage`, `needs-info`,
`ready-for-agent`, `ready-for-human`, `wontfix` — none of which exist in the tracker yet.
See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` and one `docs/adr/` at the repo root, covering the whole
workspace. See `docs/agents/domain.md`.

<!-- TITLE: type(scope): summary — imperative, ≤72 chars, no trailing period.
     Gitea turns this title into the merge subject, so it has to read as a commit.
     Types: feat fix docs refactor perf test chore ci security. See docs/writing.md. -->

<!-- What and why — the diff says how. The investigation belongs here, not in the commit. -->

**User-facing fact changed?** (an install step, a knob, a port, what a feature does, a limit)
→ the docs-site page that owns it is updated in this PR, or this is n/a. Install/repo/port facts
live in `data/platforms.json`. (CONTRIBUTING.md "Where facts live"; `docs-drift` in CI only
catches the mechanical half.)

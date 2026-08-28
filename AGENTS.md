# AGENTS.md

Guidance for coding agents working in this repository.

## Writing standards

Read `docs/writing.md` before you write a commit message, a `CHANGELOG.md` entry, or a comment.
It is the house style for all three, and §4 is a per-PR checklist. The short version:

- Commit subject is `type(scope): summary` — imperative, **72-character hard cap**, no trailing
  period, one logical change. No `Co-Authored-By` trailer.
- The commit body is *why*, wrapped at 72. The investigation, the measurements and the rejected
  paths go on the pull request, never in the message.
- Write the Gitea PR title as a conventional commit; Gitea makes it the merge subject.
- New `CHANGELOG.md` sections use Keep a Changelog categories. Leave the older sections alone.
- A comment states an invariant or a trap. A comment never enforces a trust boundary — a type,
  a test or an assertion does.

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

# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the
codebase.

This is a **single-context** repo: one `CONTEXT.md` and one `docs/adr/` at the root, covering the
whole workspace.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root.
- **`docs/adr/`** — read ADRs that touch the area you're about to work in.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest
creating them upfront. The `/domain-modeling` skill (reached via `/grill-with-docs` and
`/improve-codebase-architecture`) creates them lazily when terms or decisions actually get resolved.

Neither file exists yet — that is expected, and not something to fix pre-emptively.

## File structure

```
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0001-....md
│   └── 0002-....md
├── crates/          ← Rust workspace members (host, capture, encode, decode, presenter, …)
├── clients/         ← per-platform clients (android, apple, linux, cli, decky, …)
├── web/             ← web console
├── sdk/
└── plugin-kit/
```

The code is split across many crates and client platforms, but they serve one domain — a host
captures, encodes, and streams a session to a client that decodes and presents it. Keep the
glossary unified across them rather than splitting per directory. If a genuinely separate domain
appears later, switch to a root `CONTEXT-MAP.md` pointing at per-context `CONTEXT.md` files and
update this file.

`docs/` already holds release notes (`docs/releases/`) — those are not domain docs, and ADRs sit
alongside them in `docs/adr/`, not inside them.

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a
test name), use the term as defined in `CONTEXT.md`. Don't drift to synonyms the glossary
explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal — either you're inventing
language the project doesn't use (reconsider) or there's a real gap (note it for
`/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0007 (event-sourced orders) — but worth reopening because…_

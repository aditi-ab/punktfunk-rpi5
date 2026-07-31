# punktfunk-docs

The Punktfunk documentation site: [Fumadocs](https://fumadocs.dev) on
[TanStack Start](https://tanstack.com/start) (Vite + Nitro/bun preset).

Content lives in [`content/docs/`](content/docs) as `.md`/`.mdx`. This site is the source of truth
for the **user-facing** guides; design rationale lives in the internal punktfunk-planning repo.

## API reference

`/api` renders the host's **management REST API** as an interactive
[Scalar](https://github.com/scalar/scalar) reference (linked from the top nav, the docs
sidebar, and the landing page). It reads [`public/openapi.json`](public/openapi.json) — a
**snapshot** of the repo's generated spec. Refresh it after a management-API change:

```sh
# from the repo root — regenerate the spec, then copy the snapshot in:
cargo run -p punktfunk-host -- openapi > api/openapi.json
cp api/openapi.json docs-site/public/openapi.json
```

Nothing in CI diffs the two, so the snapshot goes stale silently — that manual `cp` is the only
thing keeping them in sync. Before publishing docs, check that they match:

```bash
diff <(jq -S . api/openapi.json) <(jq -S . docs-site/public/openapi.json)
```

That should print nothing. Right now it doesn't: the committed snapshot predates the
`/api/v1/update/check`, `/api/v1/update/apply` and `/api/v1/update/status` endpoints, so the
published `/api` reference is missing the host self-update surface — re-copy it.

## Develop

```sh
bun install
bun run dev        # http://localhost:3001  (docs at /docs)
```

CI gates every change on `bun run build` followed by `bun run lint` (the TypeScript typecheck), in
that order — the build emits the `.source` typegen the typecheck imports. Run both before you push.

## Build & serve

```sh
bun run build
bun run start      # serves .output/ via Bun
```

## Layout

```
source.config.ts          Fumadocs MDX collection (content/docs)
content/docs/             the docs content (.md/.mdx) + meta.json nav
src/
  routes/
    __root.tsx            RootProvider + html shell
    index.tsx            landing page
    docs/$.tsx           catch-all docs renderer (Fumadocs DocsLayout)
    api/index.tsx        Scalar API reference (reads public/openapi.json)
    api/search.ts        Orama search endpoint
  lib/source.ts          Fumadocs loader over the generated collection
  lib/layout.shared.tsx  shared nav chrome
  components/mdx.tsx      MDX component map
  styles/app.css          Tailwind 4 + Fumadocs preset
```

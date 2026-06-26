# punktfunk-docs

The punktfunk documentation site: [Fumadocs](https://fumadocs.dev) on
[TanStack Start](https://tanstack.com/start) (Vite + Nitro/bun preset).

Content lives in [`content/docs/`](content/docs) as `.md`/`.mdx`. Several pages are imported
verbatim from the repo's `docs/` design notes (with added frontmatter); edit those there or
here as the docs site becomes the source of truth.

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

## Develop

```sh
bun install
bun run dev        # http://localhost:3001  (docs at /docs)
```

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

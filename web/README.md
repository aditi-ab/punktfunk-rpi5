# punktfunk web — management console

The browser UI for the punktfunk host's **management REST API** (`crates/punktfunk-host/src/mgmt.rs`,
OpenAPI at `docs/api/openapi.json`). It shows live status, host capabilities, paired
clients, the pairing-PIN flow, and session controls.

Stack: **TanStack Start** (Vite, SPA mode) · **React Query** via **orval** codegen from the
OpenAPI spec · **shadcn/ui** (Tailwind v4) · **Paraglide** i18n (en/de).

## Develop

```sh
# from web/
pnpm install              # runs `prepare` → codegen (orval + paraglide)
pnpm dev                  # http://localhost:3000

# The dev server proxies /api → http://127.0.0.1:47990 (the host's management API).
# Point it elsewhere with PUNKTFUNK_MGMT_URL=http://<host>:47990 pnpm dev
```

Start a host with the management API up:

```sh
# from the repo root — `serve` brings up the GameStream control plane + the mgmt API:
WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE \
  cargo run -rp punktfunk-host -- serve
# loopback :47990, no token (a token is mandatory for non-loopback binds).
```

If the host runs with `--mgmt-token`, set it under **Settings → API token** (stored in
`localStorage`, sent as `Authorization: Bearer …` by the orval fetcher).

## Codegen

Generated code is **not committed** (gitignored) — it's reproduced from sources:

- `pnpm codegen` — regenerate the API client (orval) + i18n runtime (paraglide). Runs
  automatically on `pnpm install` (`prepare`) and before `dev`/`build` (`pre*` for orval;
  the Vite plugin compiles paraglide on dev/build).
- After a management-API change, regenerate the spec on the Rust side first:
  `cargo run -p punktfunk-host -- openapi > docs/api/openapi.json`, then `pnpm api:gen`.

## Layout

```
src/
  routes/            file-based routes (index=dashboard, host, clients, pairing, settings)
  components/
    app-shell.tsx    sidebar nav + language switcher
    ui/              shadcn/ui primitives (button, card, table, …)
    query-state.tsx  loading/error wrapper (incl. 401 → "set a token")
  api/
    fetcher.ts       orval mutator: base URL, bearer token, JSON, throwing ApiError
    gen/             GENERATED react-query hooks + models (orval)
  lib/i18n.ts        reactive Paraglide locale hook
  paraglide/         GENERATED i18n runtime (paraglide)
messages/{en,de}.json   translation sources
```

## Build

```sh
pnpm build            # client + SSR bundles under dist/
pnpm lint             # tsc --noEmit
```

A future step serves the built assets from the host itself (same origin as the API);
today it's a standalone dev/preview app against the loopback management port.

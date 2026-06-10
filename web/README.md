# punktfunk web — management console

The browser UI for the punktfunk host's **management REST API** (`crates/punktfunk-host/src/mgmt.rs`,
OpenAPI at `docs/api/openapi.json`). It shows live status, host capabilities, paired
clients, the pairing-PIN flow, and session controls.

Stack: **TanStack Start** (full SSR) on **Bun** via **Nitro v2** (`bun` preset) · **React
Query** through **orval** codegen from the OpenAPI spec · **shadcn/ui** (Tailwind v4) ·
**Paraglide** i18n (en/de). Package manager + runtime: **Bun**.

## Develop

```sh
# from web/  — Bun is the toolchain (https://bun.sh)
bun install               # runs `prepare` → codegen (orval + paraglide)
bun run dev               # http://localhost:3000

# The dev server proxies /api → http://127.0.0.1:47990 (the host's management API).
# Point it elsewhere: PUNKTFUNK_MGMT_URL=http://<host>:47990 bun run dev
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

## Build & run (Nitro + Bun)

```sh
bun run build             # → .output/  (Nitro server, `bun` preset, + .output/public assets)
PORT=3000 HOST=0.0.0.0 bun run start    # = bun run .output/server/index.mjs
bun run lint              # tsc --noEmit
```

The built **Nitro Bun server** SSR-renders the app and proxies `/api/**` to the management
host (a Nitro `routeRules` proxy → `PUNKTFUNK_MGMT_URL`, default `127.0.0.1:47990`), so the
browser stays same-origin (bearer token rides along, no CORS). Run it on the same box as
the host; it serves the console on `:3000` (or `$PORT`).

> Toolchain notes (load-bearing): TanStack Start's `start-plugin-core` peer-requires
> **Vite ≥ 7** — on Vite 6 the build's prerender/post-build hook silently doesn't run.
> `@vitejs/plugin-react` must match Vite (v5 ↔ Vite 7, v6 ↔ Vite 8); it's **required even
> for dev** (TanStack Start's dev mode needs the React Refresh runtime, else a blank
> screen). Nitro is the server target — without it `vite build` only emits client+SSR
> bundles, no deployable server. The Nitro `bun` preset makes `.output/server/index.mjs`
> Bun-runnable.

## Codegen

Generated code is **not committed** (gitignored) — reproduced from sources:

- `bun run codegen` — regenerate the API client (orval) + i18n runtime (paraglide). Runs on
  `bun install` (`prepare`) and before `dev`/`build` (`pre*` for orval; the Vite plugin
  compiles paraglide on dev/build).
- After a management-API change, regenerate the spec on the Rust side first:
  `cargo run -p punktfunk-host -- openapi > docs/api/openapi.json`, then `bun run api:gen`.

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

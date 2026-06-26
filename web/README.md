# punktfunk web — management console

The browser UI for the punktfunk host's **management REST API** (`crates/punktfunk-host/src/mgmt.rs`,
OpenAPI at `api/openapi.json`). It shows live status, host capabilities, paired
clients, the pairing-PIN flow, and session controls.

Stack: **TanStack Start** (full SSR) on **Bun** via **Nitro v2** (`bun` preset) · **React
Query** through **orval** codegen from the OpenAPI spec · **[`@unom/ui`](https://git.unom.io/unom/ui)**
— the shared punktfunk/unom design system the marketing site + docs are built on (Tailwind v4,
animated components on the violet brand over dark chrome) ·
**Paraglide** i18n (en/de). Package manager + runtime: **Bun**.

The `@unom` registry mapping lives in [`.npmrc`](.npmrc); the auth token comes from
`~/.npmrc` (or a CI secret).

## Develop

```sh
# from web/  — Bun is the toolchain (https://bun.sh)
bun install               # runs `prepare` → codegen (orval + paraglide)
bun run dev               # http://localhost:3000

# The dev server proxies /api → https://127.0.0.1:47990 (the host's mgmt API; it serves HTTPS
# with the host's self-signed identity cert — the dev proxy uses `secure: false`).
# Point it elsewhere: PUNKTFUNK_MGMT_URL=https://<host>:47990 bun run dev
```

Start a host with the management API up:

```sh
# from the repo root — `serve` brings up the native punktfunk/1 plane + the mgmt API (the console
# only needs the mgmt API; add --gamestream too if you also want the Moonlight surface):
WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE \
  cargo run -rp punktfunk-host -- serve
# loopback :47990, no token (a token is mandatory for non-loopback binds).
```

If the host runs with `--mgmt-token`, set it under **Settings → API token** (stored in
`localStorage`, sent as `Authorization: Bearer …` by the orval fetcher).

## Build & run (Nitro + Bun)

```sh
bun run build             # → .output/  (Nitro server, `bun` preset, + .output/public assets)
PORT=3000 HOST=0.0.0.0 \
  PUNKTFUNK_UI_PASSWORD=… PUNKTFUNK_MGMT_TOKEN=… \
  PUNKTFUNK_MGMT_URL=https://127.0.0.1:47990 NODE_TLS_REJECT_UNAUTHORIZED=0 \
  bun run start           # = bun run .output/server/index.mjs
# (the mgmt API is HTTPS w/ the host's self-signed cert on loopback → the proxy's fetch needs
#  NODE_TLS_REJECT_UNAUTHORIZED=0; it makes no other outbound TLS calls. See .env.example.)
bun run lint              # tsc --noEmit
```

The built **Nitro Bun server** SSR-renders the app and is the only thing exposed on the LAN.
Run it on the same box as the host; it serves the console on `:3000` (or `$PORT`).

## Auth (backend-for-frontend)

Single-user, login-gated. Config via env (see `.env.example`):

- The console requires a **login** (`PUNKTFUNK_UI_PASSWORD`). On success the server sets a
  **sealed session cookie** (h3 `useSession`, AES-GCM). `server/middleware/auth.ts` gates
  *every* request — pages redirect to `/login`, `/api` returns 401 — and **fails closed**
  (503) if `PUNKTFUNK_UI_PASSWORD` is unset, so a misconfigured LAN server admits no one.
- The **management API stays loopback-only + token** — never LAN-exposed. The web server
  holds `PUNKTFUNK_MGMT_TOKEN` server-side and injects it when proxying `/api/**` →
  `PUNKTFUNK_MGMT_URL` (`server/routes/api/[...].ts`). **The token never reaches the
  browser**; the browser only ever holds the session cookie.

So: `browser ──password──▶ web server (session cookie) ──mgmt token, server-side──▶ mgmt API`.
Run the host with a matching token: `cargo run -rp punktfunk-host -- serve` +
`PUNKTFUNK_MGMT_TOKEN=…` (or `--mgmt-token …`). `vite dev` has no gate (localhost-only) and
proxies straight to the loopback mgmt API.

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
  `cargo run -p punktfunk-host -- openapi > api/openapi.json`, then `bun run api:gen`.

## Layout

```
src/
  routes/            file-based routes (index=dashboard, host, clients, pairing, settings)
  components/
    app-shell.tsx    sidebar nav (brand lens + wordmark) + language switcher
    brand-mark/wordmark/logo.tsx   punktfunk lens mark + wordmark (shared with the site/docs)
    ui/              @unom/ui-backed primitives (button, input, label, card; badge/table/skeleton)
    query-state.tsx  loading/error wrapper (incl. 401 → "set a token")
  api/
    fetcher.ts       orval mutator: base URL, bearer token, JSON, throwing ApiError
    gen/             GENERATED react-query hooks + models (orval)
  lib/i18n.ts        reactive Paraglide locale hook
  paraglide/         GENERATED i18n runtime (paraglide)
messages/{en,de}.json   translation sources
```

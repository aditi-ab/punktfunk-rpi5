import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import { nitroV2Plugin } from '@tanstack/nitro-v2-vite-plugin'
import viteReact from '@vitejs/plugin-react'
import viteTsConfigPaths from 'vite-tsconfig-paths'
import tailwindcss from '@tailwindcss/vite'
import { paraglideVitePlugin } from '@inlang/paraglide-js'

// Absolute path to our Nitro server source (middleware + routes). Passed as a scanDir
// because the TanStack Nitro plugin doesn't auto-scan a server/ dir.
const serverDir = fileURLToPath(new URL('./server', import.meta.url))

// The management API the console drives. The browser always talks same-origin (/api/...):
// in `vite dev` the dev server proxies it (below); in the built Bun/Nitro server a Nitro
// route-rule proxies it (below). Override the upstream with PUNKTFUNK_MGMT_URL.
const MGMT_URL = process.env.PUNKTFUNK_MGMT_URL ?? 'https://127.0.0.1:47990'

export default defineConfig({
  server: {
    proxy: {
      // `secure: false`: the host serves its own self-signed identity cert on loopback.
      '/api': { target: MGMT_URL, changeOrigin: true, secure: false },
    },
  },
  plugins: [
    viteTsConfigPaths({ projects: ['./tsconfig.json'] }),
    tailwindcss(),
    paraglideVitePlugin({
      project: './project.inlang',
      outdir: './src/paraglide',
      strategy: ['localStorage', 'preferredLanguage', 'baseLocale'],
    }),
    // Full SSR on the TanStack Start runtime (the management console's data queries run
    // client-side after hydration — React Query doesn't fetch during SSR — so the server
    // renders a data-free shell that hydrates in the browser).
    tanstackStart(),
    // Nitro v2 is the deployment target: the `bun` preset bundles a Bun-runnable server to
    // .output/ (`bun run .output/server/index.mjs`). Auth + the /api proxy live in the
    // scanned `server/` dir (middleware/auth.ts gates every request; routes/api/[...].ts
    // proxies to the management host injecting the bearer token server-side) — NOT a static
    // routeRule, so the proxy runs behind the login gate and reads env at runtime.
    nitroV2Plugin({
      // node-server (not bun): a STANDALONE node HTTP server (`node .output/server/index.mjs`
      // listens — the plain `node` preset only exports a handler). Lets the bundled punktfunk-web
      // .deb depend on apt-native `nodejs (>= 20)` instead of vendoring bun. CI still BUILDS with
      // bun; only the runtime target changes. (dev `vite dev` is unaffected.)
      preset: 'node-server',
      compatibilityDate: '2026-06-10',
      // Scan server/{middleware,routes} for the auth gate + the /api proxy.
      scanDirs: [serverDir],
    }),
    // Must come AFTER tanstackStart — provides the React JSX transform + Refresh runtime
    // that Start's dev mode requires (omitting it leaves the client JS unable to load).
    viteReact(),
  ],
})

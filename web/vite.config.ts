import { defineConfig } from 'vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import { nitroV2Plugin } from '@tanstack/nitro-v2-vite-plugin'
import viteReact from '@vitejs/plugin-react'
import viteTsConfigPaths from 'vite-tsconfig-paths'
import tailwindcss from '@tailwindcss/vite'
import { paraglideVitePlugin } from '@inlang/paraglide-js'

// The management API the console drives. The browser always talks same-origin (/api/...):
// in `vite dev` the dev server proxies it (below); in the built Bun/Nitro server a Nitro
// route-rule proxies it (below). Override the upstream with PUNKTFUNK_MGMT_URL.
const MGMT_URL = process.env.PUNKTFUNK_MGMT_URL ?? 'http://127.0.0.1:47990'

export default defineConfig({
  server: {
    proxy: {
      '/api': { target: MGMT_URL, changeOrigin: true },
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
    // .output/ (`bun run .output/server/index.mjs`). The route-rule keeps the browser
    // same-origin by proxying /api/** to the management host, so the bearer token and
    // cookies ride along with no CORS.
    nitroV2Plugin({
      preset: 'bun',
      compatibilityDate: '2026-06-10',
      routeRules: {
        '/api/**': { proxy: `${MGMT_URL}/api/**` },
      },
    }),
    // Must come AFTER tanstackStart — provides the React JSX transform + Refresh runtime
    // that Start's dev mode requires (omitting it leaves the client JS unable to load).
    viteReact(),
  ],
})

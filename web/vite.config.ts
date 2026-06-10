import { defineConfig } from 'vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import viteReact from '@vitejs/plugin-react'
import viteTsConfigPaths from 'vite-tsconfig-paths'
import tailwindcss from '@tailwindcss/vite'
import { paraglideVitePlugin } from '@inlang/paraglide-js'

// The management API the console drives. In dev we proxy same-origin so the browser
// never needs CORS and the bearer token (when set) rides along untouched. Override the
// target with PUNKTFUNK_MGMT_URL when the host isn't on the default loopback port.
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
    tanstackStart({
      // A management console for a loopback host — render it as a client SPA (no SSR data
      // fetching against a token-gated local API), still on the TanStack Start runtime.
      spa: { enabled: true },
    }),
    // Must come AFTER tanstackStart — provides the React JSX transform + Refresh runtime
    // that Start's dev mode requires (omitting it leaves the client JS unable to load).
    viteReact(),
  ],
})

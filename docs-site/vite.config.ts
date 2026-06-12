import { defineConfig } from 'vite'
import tsConfigPaths from 'vite-tsconfig-paths'
import tailwindcss from '@tailwindcss/vite'
import mdx from 'fumadocs-mdx/vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import { nitroV2Plugin } from '@tanstack/nitro-v2-vite-plugin'
import viteReact from '@vitejs/plugin-react'

export default defineConfig({
  server: { port: 3001 },
  plugins: [
    // Fumadocs MDX must run first: it registers the resolver/loader that transforms
    // .md/.mdx and emits the `.source` typegen (the `collections/*` virtual modules the
    // docs route imports), before route generation and the React transform.
    mdx(),
    tsConfigPaths({ projects: ['./tsconfig.json'] }),
    tailwindcss(),
    // Full SSR on the TanStack Start runtime; the docs render server-side then hydrate.
    tanstackStart(),
    // Nitro v2 is the deployment target: the `bun` preset bundles a Bun-runnable server
    // to .output/ (`bun run .output/server/index.mjs`), matching the rest of the project.
    nitroV2Plugin({
      preset: 'bun',
      compatibilityDate: '2026-06-12',
    }),
    // Must come AFTER tanstackStart — provides the React JSX transform + Refresh runtime
    // that Start's dev mode requires.
    viteReact(),
  ],
})

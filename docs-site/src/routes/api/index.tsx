import { createFileRoute, Link } from '@tanstack/react-router'
import { ApiReferenceReact } from '@scalar/api-reference-react'
// @scalar/api-reference-react@0.9.47's entry does NOT import its own stylesheet
// (and doesn't inject it at runtime), so we must ship it ourselves or the
// reference renders unstyled. Load it as a route-scoped <link> (same pattern as
// the root app.css), so it's present for SSR + the client-side Vue mount.
import scalarCss from '@scalar/api-reference-react/style.css?url'
import BrandMark from '@/components/BrandMark'
import Wordmark from '@/components/Wordmark'

export const Route = createFileRoute('/api/')({
  component: ApiReference,
  head: () => ({
    meta: [
      { title: 'punktfunk — Management API reference' },
      {
        name: 'description',
        content:
          'Interactive reference for the punktfunk host management REST API (OpenAPI).',
      },
    ],
    links: [{ rel: 'stylesheet', href: scalarCss }],
  }),
})

// The full punktfunk theme rolled out onto Scalar — the same dark-violet
// product chrome as the management console (bg #141019 / cards #1c1530, the
// violet lens brand, Geist). Scalar is locked to dark mode below; the palette
// maps every Scalar token (surfaces, text, sidebar, links, buttons, method
// colours). Scalar ignores unknown custom-property names, so this is forward-safe.
const SCALAR_CSS = `
.light-mode,
.dark-mode {
  --scalar-font: 'Geist Variable', ui-sans-serif, system-ui, sans-serif;
  --scalar-font-code: ui-monospace, 'SFMono-Regular', Menlo, Consolas, monospace;
  --scalar-radius: 0.5rem;
  --scalar-radius-lg: 0.75rem;
  --scalar-radius-xl: 0.875rem;
}
.dark-mode {
  /* Surfaces — the violet-tinted app-icon chrome. */
  --scalar-background-1: #141019;
  --scalar-background-2: #1c1530;
  --scalar-background-3: #221a36;
  --scalar-background-accent: #6c5bf32e;
  --scalar-border-color: #2a2148;

  /* Text. */
  --scalar-color-1: #f4f2fb;
  --scalar-color-2: #b7b1c9;
  --scalar-color-3: #8a85a0;
  --scalar-color-accent: #a79ff8;

  /* Links. */
  --scalar-link-color: #a79ff8;
  --scalar-link-color-hover: #c8c0fb;

  /* Primary action button (brand violet). */
  --scalar-button-1: #6c5bf3;
  --scalar-button-1-color: #ffffff;
  --scalar-button-1-hover: #5d4ee0;

  /* Sidebar. */
  --scalar-sidebar-background-1: #17121f;
  --scalar-sidebar-color-1: #e9e6f4;
  --scalar-sidebar-color-2: #9a94ad;
  --scalar-sidebar-color-active: #c8c0fb;
  --scalar-sidebar-item-hover-background: #6c5bf31f;
  --scalar-sidebar-item-hover-color: #f4f2fb;
  --scalar-sidebar-item-active-background: #6c5bf333;
  --scalar-sidebar-border-color: #241c3d;
  --scalar-sidebar-search-background: #1c1530;
  --scalar-sidebar-search-border-color: #2a2148;
  --scalar-sidebar-search-color: #9a94ad;
  --scalar-sidebar-indent-border: #2a2148;
  --scalar-sidebar-indent-border-active: #6c5bf3;
  --scalar-sidebar-indent-border-hover: #463a78;

  /* Header (if shown). */
  --scalar-header-background-1: #141019;
  --scalar-header-color-1: #f4f2fb;
  --scalar-header-border-color: #2a2148;

  /* Scrollbar. */
  --scalar-scrollbar-color: #2a2148;
  --scalar-scrollbar-color-active: #463a78;

  /* HTTP method / status colours — kept distinct, tuned to read on dark. */
  --scalar-color-green: #4ade80;
  --scalar-color-red: #f87171;
  --scalar-color-yellow: #fbbf24;
  --scalar-color-blue: #60a5fa;
  --scalar-color-orange: #fb923c;
  --scalar-color-purple: #a79ff8;
}
`

function ApiReference() {
  return (
    <div className="flex min-h-screen flex-col">
      {/* Slim branded bar so the reference stays inside the punktfunk identity
          and links back into the docs. */}
      <header className="flex h-14 shrink-0 items-center justify-between border-b border-fd-border px-4 md:px-6">
        <Link
          to="/docs/$"
          params={{ _splat: '' }}
          aria-label="punktfunk documentation"
          className="flex items-center gap-2 no-underline"
        >
          <BrandMark className="size-6" />
          <Wordmark className="h-4" />
          <span className="ml-2 hidden text-sm text-fd-muted-foreground sm:inline">
            Management API
          </span>
        </Link>
        <nav className="flex items-center gap-4 text-sm">
          <Link
            to="/docs/$"
            params={{ _splat: '' }}
            className="text-fd-muted-foreground transition-colors hover:text-fd-foreground"
          >
            ← Docs
          </Link>
          <a
            href="/openapi.json"
            className="text-fd-muted-foreground transition-colors hover:text-fd-foreground"
          >
            openapi.json
          </a>
        </nav>
      </header>

      {/* Scalar mounts a Vue app client-side in a useEffect (SSR-safe: the
          server renders an empty container, the browser hydrates the reference). */}
      <div className="min-h-0 flex-1">
        <ApiReferenceReact
          configuration={{
            url: '/openapi.json',
            darkMode: true,
            // Lock to the punktfunk dark-violet theme — no light-mode escape hatch.
            hideDarkModeToggle: true,
            metaData: { title: 'punktfunk Management API' },
            hideDownloadButton: false,
            customCss: SCALAR_CSS,
          }}
        />
      </div>
    </div>
  )
}

import { createFileRoute, Link } from '@tanstack/react-router'
import { ApiReferenceReact } from '@scalar/api-reference-react'
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
  }),
})

// Brand the Scalar reference to the punktfunk violet + Geist, in both light and
// dark. Scalar ignores unknown custom-property names, so this is forward-safe.
const SCALAR_CSS = `
:root {
  --scalar-color-accent: #6c5bf3;
  --scalar-font: 'Geist Variable', ui-sans-serif, system-ui, sans-serif;
}
.dark-mode {
  --scalar-color-accent: #a79ff8;
  --scalar-background-1: #141019;
  --scalar-background-2: #1c1530;
  --scalar-background-3: #221a36;
  --scalar-border-color: #2a2148;
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
            metaData: { title: 'punktfunk Management API' },
            hideDownloadButton: false,
            customCss: SCALAR_CSS,
          }}
        />
      </div>
    </div>
  )
}

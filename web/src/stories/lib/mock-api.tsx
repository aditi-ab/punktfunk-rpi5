import { type ReactNode, useEffect, useRef, useState } from 'react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'

/** Map of API pathname (e.g. `/api/v1/host`) → JSON body to return for a GET. */
export type MockRoutes = Record<string, unknown>

/**
 * Renders a data-backed page WITHOUT a running host by stubbing `window.fetch`
 * for the lifetime of the story: matched pathnames return their mock JSON (200),
 * everything else returns `{}` (200) so mutations + polling never error. The
 * real orval/React-Query hooks run unchanged, so loading/success transitions and
 * `refetchInterval` behave exactly as in the app. Each story gets a fresh,
 * isolated QueryClient (retries off).
 */
export function MockApi({ routes, children }: { routes: MockRoutes; children: ReactNode }) {
  // Read the latest routes inside the stub without re-installing it.
  const routesRef = useRef(routes)
  routesRef.current = routes
  const [stubbed, setStubbed] = useState(false)

  useEffect(() => {
    const real = window.fetch
    const stub = (input: RequestInfo | URL): Promise<Response> => {
      const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
      const path = new URL(url, window.location.origin).pathname
      const data = path in routesRef.current ? routesRef.current[path] : {}
      return Promise.resolve(
        new Response(JSON.stringify(data ?? null), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      )
    }
    window.fetch = stub as typeof window.fetch
    setStubbed(true)
    return () => {
      window.fetch = real
    }
  }, [])

  const [queryClient] = useState(
    () => new QueryClient({ defaultOptions: { queries: { retry: false } } }),
  )

  // Hold the first render until the stub is installed, so the page's initial
  // query resolves against the mock rather than racing a real (failing) request.
  if (!stubbed) return null
  return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
}

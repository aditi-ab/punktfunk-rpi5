import { createRouter as createTanStackRouter, Link } from '@tanstack/react-router'
import { reloadOnStaleChunk } from '@unom/ui/preload-reload'
import { routeTree } from './routeTree.gen'

// Hydration can build a second router and discard the first. `reloadOnStaleChunk`
// registers once, so its callback has to read whichever router is live at event
// time rather than closing over the one that happened to exist at startup.
let liveRouter: { latestLocation: { href: string } } | undefined

export function getRouter() {
  const router = createTanStackRouter({
    routeTree,
    defaultPreload: 'intent',
    scrollRestoration: true,
    defaultNotFoundComponent: NotFound,
  })

  liveRouter = router

  // A deploy replaces every hashed chunk, so a tab opened before it asks for files the
  // new server has never heard of and the next navigation dies — with `defaultPreload:
  // 'intent'`, a hover is enough to trip it. Handing back where the router was heading
  // means the click that tripped it still lands on the page the reader asked for; during
  // a hover-preload that is the current URL, which degrades to a plain reload.
  reloadOnStaleChunk(10_000, () => liveRouter?.latestLocation.href)

  return router
}

function NotFound() {
  return (
    <main className="flex flex-1 flex-col items-center justify-center gap-4 px-4 py-24 text-center">
      <h1 className="text-3xl font-bold">404</h1>
      <p className="text-fd-muted-foreground">This page could not be found.</p>
      <Link to="/" className="text-fd-primary underline underline-offset-4">
        Back home
      </Link>
    </main>
  )
}

declare module '@tanstack/react-router' {
  interface Register {
    router: ReturnType<typeof getRouter>
  }
}

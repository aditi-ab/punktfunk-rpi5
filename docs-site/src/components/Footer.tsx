import { getRouteApi } from '@tanstack/react-router'
import type { NavigationLink, NavigationSection } from '@/lib/cms'

const rootApi = getRouteApi('__root__')

// The docs share the marketing site's footer (same CMS global). Root-relative
// links target the website, so resolve them against its origin — the docs don't
// host /legal/* etc. themselves. Mirrors the website Footer, themed for docs.
const SITE_URL = 'https://punktfunk.unom.io'
const resolve = (to?: string | null) =>
  to ? (to.startsWith('/') ? `${SITE_URL}${to}` : to) : '#'

export default function Footer() {
  const { footer } = rootApi.useLoaderData()
  const sections: NavigationSection[] = footer?.sections ?? []
  const tagline = footer?.tagline?.trim()

  if (!sections.length && !tagline) return null

  return (
    <footer className="border-t border-fd-border bg-fd-card">
      <div className="mx-auto flex w-full max-w-6xl flex-row flex-wrap gap-12 px-4 py-12 sm:px-6">
        {sections.map((group, gi) => (
          <div key={group.id ?? gi}>
            {group.title && (
              <h3 className="mb-2 text-sm font-semibold text-fd-foreground">
                {group.title}
              </h3>
            )}
            <div className="flex flex-col gap-1">
              {(group.entries ?? []).map((item: NavigationLink, i) => (
                <a
                  key={item.id ?? `${item.to}-${i}`}
                  href={resolve(item.to)}
                  className="text-sm text-fd-muted-foreground transition-colors hover:text-fd-foreground"
                >
                  {item.label}
                </a>
              ))}
            </div>
          </div>
        ))}
        {tagline && (
          <p className="ml-auto self-end text-sm text-fd-muted-foreground">
            {tagline}
          </p>
        )}
      </div>
    </footer>
  )
}

// Thin reactive layer over Paraglide. Paraglide's `m.*` message functions and
// `setLocale`/`getLocale` are framework-agnostic; this hook re-renders React when the
// locale changes (Paraglide's localStorage strategy persists the choice across reloads).
import { useSyncExternalStore } from 'react'
import { getLocale, setLocale, locales } from '@/paraglide/runtime'

/** The available locales as a union (`'en' | 'de'`), derived from Paraglide's `locales`. */
export type Locale = (typeof locales)[number]

const listeners = new Set<() => void>()

/** Switch locale and notify subscribers (Paraglide also persists it per its strategy). */
export function changeLocale(locale: Locale) {
  // `reload: false` keeps the SPA mounted; we re-render via the store below.
  setLocale(locale, { reload: false })
  for (const l of listeners) l()
}

function subscribe(cb: () => void) {
  listeners.add(cb)
  return () => listeners.delete(cb)
}

/** Current locale, reactive — components using `m.*` should read this so they re-render. */
export function useLocale(): Locale {
  return useSyncExternalStore(subscribe, getLocale, () => 'en' as Locale)
}

export { locales }

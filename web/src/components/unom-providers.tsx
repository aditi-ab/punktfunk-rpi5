import type { ReactNode } from 'react'
import { MaterialProvider } from '@unom/ui/material'

// Turn on @unom/ui's specular "material" gloss for the whole console — the same
// design system the punktfunk marketing site is built on. SSR-safe (material
// gates its effects on a mounted flag). No SoundProvider: like the marketing
// site, the console stays silent (@unom/ui's useSound no-ops without a provider).
const MATERIAL = {
  button: { enabled: true },
  card: { enabled: true },
  dialog: { enabled: true },
  tabs: { enabled: true },
  select: { enabled: true },
  input: { enabled: true },
  checkbox: { enabled: true },
  toast: { enabled: true },
} as const

export function UnomProviders({ children }: { children: ReactNode }) {
  return <MaterialProvider theme={MATERIAL}>{children}</MaterialProvider>
}

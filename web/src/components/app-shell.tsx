import type { ReactNode } from 'react'
import { Link } from '@tanstack/react-router'
import { Activity, Server, Users, KeyRound, Settings, Radio } from 'lucide-react'
import { m } from '@/paraglide/messages'
import { useLocale, changeLocale, locales, type Locale } from '@/lib/i18n'
import { cn } from '@/lib/utils'

const NAV = [
  { to: '/', icon: Activity, label: () => m.nav_dashboard() },
  { to: '/host', icon: Server, label: () => m.nav_host() },
  { to: '/clients', icon: Users, label: () => m.nav_clients() },
  { to: '/pairing', icon: KeyRound, label: () => m.nav_pairing() },
  { to: '/settings', icon: Settings, label: () => m.nav_settings() },
] as const

export function AppShell({ children }: { children: ReactNode }) {
  // Read the locale so the whole shell re-renders on a language switch.
  useLocale()
  return (
    <div className="flex min-h-screen">
      <aside className="hidden w-60 shrink-0 flex-col border-r bg-card/40 p-4 sm:flex">
        <div className="mb-6 flex items-center gap-2 px-2">
          <Radio className="size-5 text-[var(--success)]" />
          <div>
            <div className="font-semibold leading-tight">{m.app_name()}</div>
            <div className="text-xs text-muted-foreground">{m.app_tagline()}</div>
          </div>
        </div>
        <nav className="flex flex-col gap-1">
          {NAV.map(({ to, icon: Icon, label }) => (
            <Link
              key={to}
              to={to}
              activeOptions={{ exact: to === '/' }}
              className="flex items-center gap-3 rounded-md px-3 py-2 text-sm text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
              activeProps={{ className: 'bg-accent text-foreground font-medium' }}
            >
              <Icon className="size-4" />
              {label()}
            </Link>
          ))}
        </nav>
        <div className="mt-auto pt-4">
          <LanguageSwitcher />
        </div>
      </aside>
      <main className="flex-1 overflow-x-hidden">
        <div className="mx-auto max-w-5xl p-6 sm:p-10">{children}</div>
      </main>
    </div>
  )
}

function LanguageSwitcher() {
  const current = useLocale()
  return (
    <div className="flex gap-1" role="group" aria-label="Language">
      {locales.map((l: Locale) => (
        <button
          key={l}
          onClick={() => changeLocale(l)}
          className={cn(
            'rounded px-2 py-1 text-xs uppercase transition-colors',
            l === current
              ? 'bg-secondary text-secondary-foreground font-medium'
              : 'text-muted-foreground hover:text-foreground',
          )}
        >
          {l}
        </button>
      ))}
    </div>
  )
}

import { createFileRoute } from '@tanstack/react-router'
import { LogOut } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { m } from '@/paraglide/messages'
import { useLocale, changeLocale, locales, type Locale } from '@/lib/i18n'

export const Route = createFileRoute('/settings')({ component: SettingsPage })

function SettingsPage() {
  const current = useLocale()

  const onLogout = async () => {
    await fetch('/_auth/logout', { method: 'POST' })
    window.location.href = '/login'
  }

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold">{m.settings_title()}</h1>

      <Card className="max-w-lg">
        <CardHeader>
          <CardTitle>{m.settings_language()}</CardTitle>
        </CardHeader>
        <CardContent className="flex gap-2">
          {locales.map((l: Locale) => (
            <Button
              key={l}
              variant={l === current ? 'default' : 'outline'}
              size="sm"
              className="uppercase"
              onClick={() => changeLocale(l)}
            >
              {l}
            </Button>
          ))}
        </CardContent>
      </Card>

      <Card className="max-w-lg">
        <CardHeader>
          <CardTitle>{m.nav_settings()}</CardTitle>
        </CardHeader>
        <CardContent>
          <Button variant="outline" onClick={onLogout}>
            <LogOut className="size-4" />
            {m.action_logout()}
          </Button>
        </CardContent>
      </Card>
    </div>
  )
}

import { useState } from 'react'
import { createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Check } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { getApiToken, setApiToken } from '@/api/fetcher'
import { m } from '@/paraglide/messages'
import { useLocale, changeLocale, locales, type Locale } from '@/lib/i18n'
import { cn } from '@/lib/utils'

export const Route = createFileRoute('/settings')({ component: SettingsPage })

function SettingsPage() {
  const current = useLocale()
  const qc = useQueryClient()
  const [token, setToken] = useState(getApiToken())
  const [saved, setSaved] = useState(false)

  const onSave = (e: React.FormEvent) => {
    e.preventDefault()
    setApiToken(token.trim())
    // Re-fetch everything with the new credential.
    qc.invalidateQueries()
    setSaved(true)
    setTimeout(() => setSaved(false), 2_000)
  }

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold">{m.settings_title()}</h1>

      <Card className="max-w-lg">
        <CardHeader>
          <CardTitle>{m.settings_token_label()}</CardTitle>
        </CardHeader>
        <CardContent>
          <form onSubmit={onSave} className="space-y-4">
            <div className="space-y-2">
              <Label htmlFor="token">{m.settings_token_label()}</Label>
              <Input
                id="token"
                type="password"
                autoComplete="off"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="••••••••"
              />
              <p className="text-xs text-muted-foreground">{m.settings_token_help()}</p>
            </div>
            <Button type="submit">
              {saved ? <Check className="size-4" /> : null}
              {saved ? m.settings_saved() : m.settings_save()}
            </Button>
          </form>
        </CardContent>
      </Card>

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
              className={cn('uppercase')}
              onClick={() => changeLocale(l)}
            >
              {l}
            </Button>
          ))}
        </CardContent>
      </Card>
    </div>
  )
}

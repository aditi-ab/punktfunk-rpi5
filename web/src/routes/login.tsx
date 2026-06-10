import { useState } from 'react'
import { createFileRoute, useRouter } from '@tanstack/react-router'
import { Radio } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { m } from '@/paraglide/messages'
import { useLocale } from '@/lib/i18n'

export const Route = createFileRoute('/login')({
  validateSearch: (s: Record<string, unknown>): { next?: string } => ({
    next: typeof s.next === 'string' ? s.next : undefined,
  }),
  component: LoginPage,
})

function LoginPage() {
  useLocale()
  const router = useRouter()
  const { next } = Route.useSearch()
  const [password, setPassword] = useState('')
  const [error, setError] = useState(false)
  const [busy, setBusy] = useState(false)

  const onSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    setBusy(true)
    setError(false)
    try {
      const res = await fetch('/_auth/login', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password }),
      })
      if (!res.ok) {
        setError(true)
        setBusy(false)
        return
      }
      // Full reload to the target so SSR re-runs WITH the new session cookie.
      window.location.href = next && next.startsWith('/') ? next : '/'
    } catch {
      setError(true)
      setBusy(false)
    }
    void router
  }

  return (
    <div className="flex min-h-screen items-center justify-center p-6">
      <Card className="w-full max-w-sm">
        <CardHeader className="items-center text-center">
          <div className="mb-2 flex items-center gap-2">
            <Radio className="size-5 text-[var(--success)]" />
            <span className="font-semibold">{m.app_name()}</span>
          </div>
          <CardTitle>{m.login_title()}</CardTitle>
          <p className="text-sm text-muted-foreground">{m.login_subtitle()}</p>
        </CardHeader>
        <CardContent>
          <form onSubmit={onSubmit} className="space-y-4">
            <div className="space-y-2">
              <Label htmlFor="pw">{m.login_password()}</Label>
              <Input
                id="pw"
                type="password"
                autoFocus
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </div>
            {error && <p className="text-sm text-destructive">{m.login_error()}</p>}
            <Button type="submit" className="w-full" disabled={busy || !password}>
              {busy ? m.login_signing_in() : m.login_submit()}
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  )
}

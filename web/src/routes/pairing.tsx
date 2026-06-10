import { useState } from 'react'
import { createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { KeyRound, CheckCircle2 } from 'lucide-react'
import {
  useGetPairingStatus,
  useSubmitPairingPin,
  getGetPairingStatusQueryKey,
} from '@/api/gen/pairing/pairing'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { QueryState } from '@/components/query-state'
import { m } from '@/paraglide/messages'
import { useLocale } from '@/lib/i18n'

export const Route = createFileRoute('/pairing')({ component: PairingPage })

function PairingPage() {
  useLocale()
  const qc = useQueryClient()
  const [pin, setPin] = useState('')
  // Poll: the host flips pin_pending when a Moonlight client begins pairing.
  const pairing = useGetPairingStatus({ query: { refetchInterval: 2_000 } })
  const submit = useSubmitPairingPin()
  const pending = pairing.data?.pin_pending ?? false

  const onSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    submit.mutate(
      { data: { pin } },
      {
        onSuccess: () => {
          setPin('')
          qc.invalidateQueries({ queryKey: getGetPairingStatusQueryKey() })
        },
      },
    )
  }

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold">{m.pairing_title()}</h1>
      <QueryState isLoading={pairing.isLoading} error={pairing.error} refetch={pairing.refetch}>
        <Card className="max-w-md">
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <KeyRound className="size-4" />
              {m.pairing_title()}
            </CardTitle>
          </CardHeader>
          <CardContent>
            {!pending ? (
              <p className="text-sm text-muted-foreground">{m.pairing_idle()}</p>
            ) : (
              <form onSubmit={onSubmit} className="space-y-4">
                <p className="text-sm">{m.pairing_waiting()}</p>
                <div className="space-y-2">
                  <Label htmlFor="pin">{m.pairing_pin_label()}</Label>
                  <Input
                    id="pin"
                    inputMode="numeric"
                    autoComplete="off"
                    maxLength={8}
                    value={pin}
                    onChange={(e) => setPin(e.target.value.replace(/\D/g, ''))}
                    placeholder="0000"
                    className="font-mono text-lg tracking-widest"
                  />
                </div>
                <Button type="submit" disabled={pin.length < 4 || submit.isPending}>
                  {m.pairing_submit()}
                </Button>
                {submit.isSuccess && (
                  <p className="flex items-center gap-1.5 text-sm text-[var(--success)]">
                    <CheckCircle2 className="size-4" />
                    {m.pairing_success()}
                  </p>
                )}
                {submit.isError && (
                  <p className="text-sm text-destructive">{m.pairing_failed()}</p>
                )}
              </form>
            )}
          </CardContent>
        </Card>
      </QueryState>
    </div>
  )
}

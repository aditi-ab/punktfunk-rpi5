import { useState } from 'react'
import { createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { KeyRound, CheckCircle2, Smartphone, Timer, Trash2 } from 'lucide-react'
import {
  useGetNativePairing,
  useArmNativePairing,
  useDisarmNativePairing,
  useListNativeClients,
  useUnpairNativeClient,
  getGetNativePairingQueryKey,
  getListNativeClientsQueryKey,
} from '@/api/gen/native/native'
import {
  useGetPairingStatus,
  useSubmitPairingPin,
  getGetPairingStatusQueryKey,
} from '@/api/gen/pairing/pairing'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { QueryState } from '@/components/query-state'
import { m } from '@/paraglide/messages'
import { useLocale } from '@/lib/i18n'

export const Route = createFileRoute('/pairing')({ component: PairingPage })

/** Seconds → `m:ss`. */
function fmtTime(secs: number): string {
  const s = Math.max(0, Math.floor(secs))
  return `${Math.floor(s / 60)}:${(s % 60).toString().padStart(2, '0')}`
}

function PairingPage() {
  useLocale()
  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold">{m.pairing_title()}</h1>
      <NativePairingCard />
      <NativeDevices />
      <MoonlightPairingCard />
    </div>
  )
}

/** Native (punktfunk/1) pairing: arm a window → DISPLAY the PIN the user enters on their device. */
function NativePairingCard() {
  const qc = useQueryClient()
  // Poll fast while armed (live countdown), slow otherwise.
  const status = useGetNativePairing({
    query: { refetchInterval: (q) => (q.state.data?.armed ? 1_000 : 4_000) },
  })
  const arm = useArmNativePairing()
  const disarm = useDisarmNativePairing()
  const d = status.data
  const refresh = () => qc.invalidateQueries({ queryKey: getGetNativePairingQueryKey() })

  return (
    <QueryState isLoading={status.isLoading} error={status.error} refetch={status.refetch}>
      <Card className="max-w-md">
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Smartphone className="size-4" />
            {m.pairing_native_title()}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          {!d?.enabled ? (
            <p className="text-sm text-muted-foreground">{m.pairing_native_disabled()}</p>
          ) : d.armed && d.pin ? (
            <div className="space-y-3">
              <p className="text-sm">{m.pairing_native_enter()}</p>
              <div className="rounded-lg border bg-muted/40 py-5 text-center font-mono text-4xl font-semibold tracking-[0.3em]">
                {d.pin}
              </div>
              {d.expires_in_secs != null && (
                <p className="flex items-center justify-center gap-1.5 text-sm text-muted-foreground">
                  <Timer className="size-4" />
                  {m.pairing_native_expires()} {fmtTime(d.expires_in_secs)}
                </p>
              )}
              <Button
                variant="outline"
                className="w-full"
                disabled={disarm.isPending}
                onClick={() => disarm.mutate(undefined, { onSuccess: refresh })}
              >
                {m.pairing_native_cancel()}
              </Button>
            </div>
          ) : (
            <>
              <p className="text-sm text-muted-foreground">{m.pairing_native_desc()}</p>
              <Button
                disabled={arm.isPending}
                onClick={() => arm.mutate({ data: { ttl_secs: 120 } }, { onSuccess: refresh })}
              >
                <KeyRound className="size-4" />
                {m.pairing_native_arm()}
              </Button>
            </>
          )}
        </CardContent>
      </Card>
    </QueryState>
  )
}

/** The paired native (punktfunk/1) devices, with unpair. */
function NativeDevices() {
  const qc = useQueryClient()
  const clients = useListNativeClients()
  const unpair = useUnpairNativeClient()
  const rows = clients.data ?? []

  const onUnpair = (fingerprint: string) => {
    if (!confirm(m.pairing_native_unpair_confirm())) return
    unpair.mutate(
      { fingerprint },
      { onSuccess: () => qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() }) },
    )
  }

  return (
    <div className="space-y-2">
      <h2 className="text-lg font-medium">{m.pairing_native_devices()}</h2>
      <QueryState isLoading={clients.isLoading} error={clients.error} refetch={clients.refetch}>
        {rows.length === 0 ? (
          <Card>
            <CardContent className="p-6 text-center text-sm text-muted-foreground">
              {m.pairing_native_empty()}
            </CardContent>
          </Card>
        ) : (
          <Card>
            <CardContent className="p-0">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{m.clients_name()}</TableHead>
                    <TableHead>{m.clients_fingerprint()}</TableHead>
                    <TableHead className="w-12" />
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {rows.map((c) => (
                    <TableRow key={c.fingerprint}>
                      <TableCell className="font-medium">{c.name || '—'}</TableCell>
                      <TableCell className="font-mono text-xs text-muted-foreground">
                        {c.fingerprint.slice(0, 16)}…
                      </TableCell>
                      <TableCell>
                        <Button
                          variant="ghost"
                          size="icon"
                          aria-label={m.action_unpair()}
                          disabled={unpair.isPending}
                          onClick={() => onUnpair(c.fingerprint)}
                        >
                          <Trash2 className="size-4 text-destructive" />
                        </Button>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </CardContent>
          </Card>
        )}
      </QueryState>
    </div>
  )
}

/** GameStream/Moonlight pairing: the client shows a PIN, the operator submits it here. */
function MoonlightPairingCard() {
  const qc = useQueryClient()
  const [pin, setPin] = useState('')
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
    <QueryState isLoading={pairing.isLoading} error={pairing.error} refetch={pairing.refetch}>
      <Card className="max-w-md">
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <KeyRound className="size-4" />
            {m.pairing_moonlight_title()}
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
              {submit.isError && <p className="text-sm text-destructive">{m.pairing_failed()}</p>}
            </form>
          )}
        </CardContent>
      </Card>
    </QueryState>
  )
}

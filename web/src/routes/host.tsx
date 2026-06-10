import { createFileRoute } from '@tanstack/react-router'
import { useGetHostInfo, useListCompositors } from '@/api/gen/host/host'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { QueryState } from '@/components/query-state'
import { m } from '@/paraglide/messages'
import { useLocale } from '@/lib/i18n'

export const Route = createFileRoute('/host')({ component: HostPage })

function HostPage() {
  useLocale()
  const host = useGetHostInfo()
  const compositors = useListCompositors()
  const h = host.data

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold">{m.nav_host()}</h1>
      <QueryState isLoading={host.isLoading} error={host.error} refetch={host.refetch}>
        {h && (
          <div className="grid gap-4 lg:grid-cols-2">
            <Card>
              <CardHeader>
                <CardTitle>{m.host_identity()}</CardTitle>
              </CardHeader>
              <CardContent>
                <dl className="grid grid-cols-1 gap-3">
                  <Row label={m.host_hostname()} value={h.hostname} />
                  <Row label={m.host_local_ip()} value={h.local_ip} mono />
                  <Row label={m.host_version()} value={`${h.app_version} (${h.version})`} />
                  <Row label={m.host_abi()} value={String(h.abi_version)} />
                  <Row label={m.host_uniqueid()} value={h.uniqueid} mono />
                </dl>
              </CardContent>
            </Card>
            <div className="space-y-4">
              <Card>
                <CardHeader>
                  <CardTitle>{m.host_codecs()}</CardTitle>
                </CardHeader>
                <CardContent className="flex flex-wrap gap-2">
                  {h.codecs.map((c) => (
                    <Badge key={c} variant="secondary">
                      {c.toUpperCase()}
                    </Badge>
                  ))}
                </CardContent>
              </Card>
              <Card>
                <CardHeader>
                  <CardTitle>{m.host_ports()}</CardTitle>
                </CardHeader>
                <CardContent>
                  <dl className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm tabular-nums">
                    {Object.entries(h.ports).map(([k, v]) => (
                      <div key={k} className="flex justify-between">
                        <dt className="text-muted-foreground uppercase">{k}</dt>
                        <dd className="font-medium">{v as number}</dd>
                      </div>
                    ))}
                  </dl>
                </CardContent>
              </Card>
            </div>
          </div>
        )}
      </QueryState>

      <Card>
        <CardHeader>
          <CardTitle>{m.host_compositors()}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <p className="text-sm text-muted-foreground">{m.host_compositors_help()}</p>
          <QueryState
            isLoading={compositors.isLoading}
            error={compositors.error}
            refetch={compositors.refetch}
          >
            <ul className="divide-y rounded-md border">
              {compositors.data?.map((c) => (
                <li key={c.id} className="flex items-center justify-between gap-4 px-4 py-3">
                  <div className="min-w-0">
                    <div className="flex items-center gap-2">
                      <span className="font-medium">{c.label}</span>
                      {c.default && <Badge variant="secondary">{m.compositor_default()}</Badge>}
                    </div>
                    <code className="text-xs text-muted-foreground">{c.id}</code>
                  </div>
                  <Badge variant={c.available ? 'default' : 'outline'}>
                    {c.available ? m.compositor_available() : m.compositor_unavailable()}
                  </Badge>
                </li>
              ))}
            </ul>
          </QueryState>
        </CardContent>
      </Card>
    </div>
  )
}

function Row({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-baseline justify-between gap-4">
      <dt className="text-sm text-muted-foreground">{label}</dt>
      <dd className={mono ? 'truncate font-mono text-xs' : 'font-medium'} title={value}>
        {value}
      </dd>
    </div>
  )
}

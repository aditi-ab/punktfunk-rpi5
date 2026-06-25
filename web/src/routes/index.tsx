import { createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Video, Volume2, MonitorPlay, ZapOff, RefreshCw } from 'lucide-react'
import {
  useGetStatus,
  getGetStatusQueryKey,
} from '@/api/gen/host/host'
import { useStopSession, useRequestIdr } from '@/api/gen/session/session'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { QueryState } from '@/components/query-state'
import { m } from '@/paraglide/messages'
import { useLocale } from '@/lib/i18n'

export const Route = createFileRoute('/')({ component: Dashboard })

// Exported for Storybook (see src/stories) — harmless alongside `Route`.
export function Dashboard() {
  useLocale()
  const qc = useQueryClient()
  // Poll live status every 2s so the console tracks an active session.
  const status = useGetStatus({ query: { refetchInterval: 2_000 } })
  const stop = useStopSession()
  const idr = useRequestIdr()

  const invalidate = () => qc.invalidateQueries({ queryKey: getGetStatusQueryKey() })
  const s = status.data

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold">{m.status_title()}</h1>
      <QueryState isLoading={status.isLoading} error={status.error} refetch={status.refetch}>
        {s && (
          <>
            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
              <StatCard
                icon={<Video className="size-4" />}
                label={m.status_video()}
                on={s.video_streaming}
              />
              <StatCard
                icon={<Volume2 className="size-4" />}
                label={m.status_audio()}
                on={s.audio_streaming}
              />
              <Card>
                <CardContent className="flex items-center justify-between p-4">
                  <span className="text-sm text-muted-foreground">{m.status_paired_count()}</span>
                  <span className="text-2xl font-semibold tabular-nums">{s.paired_clients}</span>
                </CardContent>
              </Card>
              <Card>
                <CardContent className="flex items-center justify-between p-4">
                  <span className="text-sm text-muted-foreground">{m.status_pin_pending()}</span>
                  <Badge variant={s.pin_pending ? 'default' : 'outline'}>
                    {s.pin_pending ? '●' : '—'}
                  </Badge>
                </CardContent>
              </Card>
            </div>

            <Card>
              <CardHeader className="flex flex-col items-start gap-3 space-y-0 sm:flex-row sm:items-center sm:justify-between">
                <CardTitle className="flex items-center gap-2">
                  <MonitorPlay className="size-4" />
                  {m.status_session()}
                </CardTitle>
                <div className="flex flex-wrap gap-2">
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={!s.video_streaming || idr.isPending}
                    onClick={() => idr.mutate(undefined)}
                  >
                    <RefreshCw className="size-3.5" />
                    {m.action_request_idr()}
                  </Button>
                  <Button
                    variant="destructive"
                    size="sm"
                    disabled={!s.session || stop.isPending}
                    onClick={() => stop.mutate(undefined, { onSuccess: invalidate })}
                  >
                    <ZapOff className="size-3.5" />
                    {m.action_stop_session()}
                  </Button>
                </div>
              </CardHeader>
              <CardContent>
                {s.stream ? (
                  <dl className="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4">
                    <Field label={m.stream_codec()} value={s.stream.codec.toUpperCase()} />
                    <Field
                      label={m.stream_resolution()}
                      value={`${s.stream.width}×${s.stream.height}`}
                    />
                    <Field label={m.stream_fps()} value={`${s.stream.fps} fps`} />
                    <Field
                      label={m.stream_bitrate()}
                      value={`${(s.stream.bitrate_kbps / 1000).toFixed(1)} Mbps`}
                    />
                  </dl>
                ) : (
                  <p className="text-sm text-muted-foreground">{m.status_no_session()}</p>
                )}
              </CardContent>
            </Card>
          </>
        )}
      </QueryState>
    </div>
  )
}

function StatCard({ icon, label, on }: { icon: React.ReactNode; label: string; on: boolean }) {
  return (
    <Card>
      <CardContent className="flex items-center justify-between p-4">
        <span className="flex items-center gap-2 text-sm text-muted-foreground">
          {icon}
          {label}
        </span>
        <Badge variant={on ? 'success' : 'outline'}>
          {on ? m.status_streaming() : m.status_idle()}
        </Badge>
      </CardContent>
    </Card>
  )
}

function Field({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-0.5 font-medium tabular-nums">{value}</dd>
    </div>
  )
}

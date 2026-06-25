import { createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { Trash2 } from 'lucide-react'
import {
  useListPairedClients,
  useUnpairClient,
  getListPairedClientsQueryKey,
} from '@/api/gen/clients/clients'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { QueryState } from '@/components/query-state'
import { m } from '@/paraglide/messages'
import { useLocale } from '@/lib/i18n'

export const Route = createFileRoute('/clients')({ component: ClientsPage })

// Exported for Storybook (see src/stories) — harmless alongside `Route`.
export function ClientsPage() {
  useLocale()
  const qc = useQueryClient()
  const clients = useListPairedClients()
  const unpair = useUnpairClient()
  const rows = clients.data ?? []

  const onUnpair = (fingerprint: string) => {
    if (!confirm(m.clients_unpair_confirm())) return
    unpair.mutate(
      { fingerprint },
      { onSuccess: () => qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() }) },
    )
  }

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold">{m.clients_title()}</h1>
      <QueryState isLoading={clients.isLoading} error={clients.error} refetch={clients.refetch}>
        {rows.length === 0 ? (
          <Card>
            <CardContent className="p-8 text-center text-sm text-muted-foreground">
              {m.clients_empty()}
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
                      <TableCell className="font-medium">{c.subject || '—'}</TableCell>
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

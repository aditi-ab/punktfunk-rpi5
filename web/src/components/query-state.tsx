import type { ReactNode } from 'react'
import { ApiError } from '@/api/fetcher'
import { Spinner } from '@/components/ui/spinner'
import { Button } from '@/components/ui/button'
import { m } from '@/paraglide/messages'

interface QueryStateProps {
  isLoading: boolean
  error: unknown
  refetch?: () => void
  children: ReactNode
}

/** Uniform loading/error wrapper for a query-backed view. */
export function QueryState({ isLoading, error, refetch, children }: QueryStateProps) {
  if (isLoading) {
    return (
      <div
        role="status"
        className="flex min-h-40 flex-col items-center justify-center gap-3 text-sm text-muted-foreground"
      >
        <Spinner className="size-8" />
        {m.common_loading()}
      </div>
    )
  }
  if (error) {
    const unauthorized = error instanceof ApiError && error.status === 401
    return (
      <div className="rounded-lg border border-destructive/40 bg-destructive/5 p-4 text-sm">
        <p className="font-medium text-destructive">
          {unauthorized ? m.common_unauthorized() : m.common_error()}
        </p>
        {refetch && !unauthorized && (
          <Button variant="outline" size="sm" className="mt-3" onClick={() => refetch()}>
            {m.common_retry()}
          </Button>
        )}
      </div>
    )
  }
  return <>{children}</>
}

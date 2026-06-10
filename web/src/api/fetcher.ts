// The fetch mutator orval-generated hooks call: `apiFetch<T>(url, RequestInit)`. orval is
// configured (includeHttpResponseReturnType: false) so `T` is the response BODY; on an HTTP
// error we THROW an `ApiError` so React Query's `isError` works (the query client is
// configured not to retry 4xx — see src/router.tsx).
//
// Centralizes the bearer token (from Settings → localStorage). In dev, requests use a
// relative `/api/...` path that Vite proxies to the management host (same-origin, no CORS,
// the token rides along); a production build served by the host hits the same path.

const TOKEN_KEY = 'punktfunk.apiToken'

export function getApiToken(): string {
  if (typeof localStorage === 'undefined') return ''
  return localStorage.getItem(TOKEN_KEY) ?? ''
}

export function setApiToken(token: string): void {
  if (typeof localStorage === 'undefined') return
  if (token) localStorage.setItem(TOKEN_KEY, token)
  else localStorage.removeItem(TOKEN_KEY)
}

/** A failed API call. `status` is the HTTP code; `data` is the parsed `ApiError` body if any. */
export class ApiError extends Error {
  status: number
  data: unknown
  constructor(status: number, data: unknown, message?: string) {
    super(message ?? `API error ${status}`)
    this.name = 'ApiError'
    this.status = status
    this.data = data
  }
}

export async function apiFetch<T>(url: string, options?: RequestInit): Promise<T> {
  const token = getApiToken()
  const headers = new Headers(options?.headers)
  headers.set('Accept', 'application/json')
  if (token) headers.set('Authorization', `Bearer ${token}`)

  const res = await fetch(url, { ...options, headers })

  const text = await res.text()
  const body = text ? safeJson(text) : undefined
  if (!res.ok) throw new ApiError(res.status, body, res.statusText)
  return body as T
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text)
  } catch {
    return text
  }
}

export default apiFetch

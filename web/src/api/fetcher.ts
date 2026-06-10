// The fetch mutator orval-generated hooks call: `apiFetch<T>(url, RequestInit)`. orval is
// configured (includeHttpResponseReturnType: false) so `T` is the response BODY; on an HTTP
// error we THROW an `ApiError` so React Query's `isError` works (the query client skips
// retries on 4xx — see src/router.tsx).
//
// Auth: requests are same-origin to `/api/...`; the browser sends only the session cookie
// (the server-side proxy injects the management bearer token — the token never lives in the
// browser). A 401 means the session is gone → bounce to /login.

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
  const headers = new Headers(options?.headers)
  headers.set('Accept', 'application/json')

  const res = await fetch(url, { ...options, headers, credentials: 'same-origin' })

  const text = await res.text()
  const body = text ? safeJson(text) : undefined
  if (res.status === 401) redirectToLogin()
  if (!res.ok) throw new ApiError(res.status, body, res.statusText)
  return body as T
}

/** On lost session, send the user to the login screen, remembering where they were. */
function redirectToLogin(): void {
  if (typeof window === 'undefined') return
  if (window.location.pathname === '/login') return
  const next = encodeURIComponent(window.location.pathname)
  window.location.href = `/login?next=${next}`
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text)
  } catch {
    return text
  }
}

export default apiFetch

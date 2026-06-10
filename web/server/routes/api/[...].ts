// /api/** → the management API. By the time we get here the gate (middleware/auth.ts) has
// confirmed an authenticated session. We inject the management bearer token server-side
// (the browser never sees it) and drop the browser's own cookies/auth from the upstream
// request, then proxy. The management API itself binds loopback only — this proxy is the
// ONLY path to it from the LAN, and it's authenticated.
import { defineEventHandler, getRequestURL, proxyRequest } from 'h3'
import { mgmtToken, mgmtUrl } from '../../util/auth'

export default defineEventHandler((event) => {
  const { pathname, search } = getRequestURL(event)
  const target = `${mgmtUrl()}${pathname}${search}`
  const token = mgmtToken()
  return proxyRequest(event, target, {
    headers: {
      // Overwrite, not append: the host-held token replaces anything the browser sent.
      authorization: token ? `Bearer ${token}` : '',
      // Don't forward the session cookie to the management API.
      cookie: '',
    },
  })
})

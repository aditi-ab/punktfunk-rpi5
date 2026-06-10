// Shared auth helpers for the Nitro server (the deployed Bun server). Single-user,
// shared-password gate: the user logs in with PUNKTFUNK_UI_PASSWORD, which sets a SEALED
// (h3 useSession — AES-GCM) cookie; every request is gated by server/middleware/auth.ts.
//
// The management token never reaches the browser: server/routes/api/[...].ts injects it
// server-side when proxying to the loopback management API.
import { createHash, timingSafeEqual as nodeTimingSafeEqual } from 'node:crypto'
import type { SessionConfig } from 'h3'

export const SESSION_NAME = 'pf_session'

/** The login password. Empty string ⇒ auth is MISCONFIGURED (the gate fails closed). */
export function uiPassword(): string {
  return process.env.PUNKTFUNK_UI_PASSWORD ?? ''
}

/** The management API the proxy forwards to (loopback by default — never LAN-exposed). */
export function mgmtUrl(): string {
  return process.env.PUNKTFUNK_MGMT_URL ?? 'http://127.0.0.1:47990'
}

/** Bearer token for the management API, injected server-side. */
export function mgmtToken(): string {
  return process.env.PUNKTFUNK_MGMT_TOKEN ?? ''
}

/**
 * The cookie-sealing key for h3 `useSession` (must be ≥ 32 chars). Use PUNKTFUNK_UI_SECRET
 * if set; otherwise derive a stable 64-hex key from the password so single-var config works
 * (changing the password then invalidates existing sessions, which is fine).
 */
export function sessionConfig(): SessionConfig {
  const secret = process.env.PUNKTFUNK_UI_SECRET
  const password = secret && secret.length >= 32
    ? secret
    : createHash('sha256').update(`punktfunk-session-v1:${uiPassword()}`).digest('hex')
  return { name: SESSION_NAME, password }
}

/** Constant-time string comparison (avoids leaking the password via timing). */
export function timingSafeEqual(a: string, b: string): boolean {
  const ab = Buffer.from(a)
  const bb = Buffer.from(b)
  if (ab.length !== bb.length) return false
  return nodeTimingSafeEqual(ab, bb)
}

/** Paths reachable WITHOUT a session: the login page, the auth endpoints, and static
 * assets (the login page needs its own CSS/JS). Everything else is gated. */
export function isPublicPath(pathname: string): boolean {
  if (pathname === '/login') return true
  if (pathname.startsWith('/_auth/')) return true
  if (pathname.startsWith('/assets/')) return true
  if (pathname === '/favicon.ico' || pathname === '/robots.txt') return true
  // Vite/TanStack client chunks and source maps requested by the login page.
  if (/\.(js|css|map|ico|svg|png|woff2?|json)$/.test(pathname)) return true
  return false
}

export interface SessionData {
  authenticated?: boolean
}

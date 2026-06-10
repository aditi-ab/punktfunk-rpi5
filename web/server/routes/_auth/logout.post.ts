// POST /_auth/logout — clear the session cookie.
import { defineEventHandler, useSession } from 'h3'
import { sessionConfig, type SessionData } from '../../util/auth'

export default defineEventHandler(async (event) => {
  const session = await useSession<SessionData>(event, sessionConfig())
  await session.clear()
  return { ok: true }
})

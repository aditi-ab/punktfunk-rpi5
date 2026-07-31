// POST /_auth/logout — clear the session cookie AND revoke every session issued so far.
//
// Clearing alone only deletes the browser's copy: the cookie is stateless, so a captured value
// stayed valid for its whole 7-day TTL and "log out" logged nothing out. Bumping the epoch means
// the gate rejects every cookie sealed before now. Single-user console, so "log out" and "sign out
// everywhere" are the same action — which is the safer of the two to make the default.
import { defineEventHandler, useSession } from "h3";
import {
	revokeAllSessions,
	type SessionData,
	sessionConfig,
} from "../../util/auth";

export default defineEventHandler(async (event) => {
	const session = await useSession<SessionData>(event, sessionConfig());
	await session.clear();
	revokeAllSessions();
	return { ok: true };
});

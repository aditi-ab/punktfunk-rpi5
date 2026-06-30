// POST /_auth/logout — clear the session cookie.
import { defineEventHandler, useSession } from "h3";
import { type SessionData, sessionConfig } from "../../util/auth";

export default defineEventHandler(async (event) => {
	const session = await useSession<SessionData>(event, sessionConfig());
	await session.clear();
	return { ok: true };
});

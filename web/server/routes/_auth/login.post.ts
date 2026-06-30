// POST /_auth/login {password} — verify the shared password (constant-time), then seal an
// authenticated session cookie. Public (allowlisted in the gate) so an unauthenticated user
// can actually log in.
import { createError, defineEventHandler, readBody, useSession } from "h3";
import {
	type SessionData,
	sessionConfig,
	timingSafeEqual,
	uiPassword,
} from "../../util/auth";

export default defineEventHandler(async (event) => {
	const expected = uiPassword();
	if (!expected) {
		throw createError({
			statusCode: 503,
			statusMessage: "auth not configured",
		});
	}
	const body = await readBody<{ password?: string }>(event);
	const password = String(body?.password ?? "");
	if (!timingSafeEqual(password, expected)) {
		throw createError({ statusCode: 401, statusMessage: "invalid password" });
	}
	const session = await useSession<SessionData>(event, sessionConfig());
	await session.update({ authenticated: true });
	return { ok: true };
});

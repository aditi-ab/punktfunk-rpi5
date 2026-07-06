// POST /_auth/login {password} — verify the shared password (constant-time), then seal an
// authenticated session cookie. Public (allowlisted in the gate) so an unauthenticated user
// can actually log in. Brute force is bounded by an in-memory per-IP throttle (loginThrottle):
// the constant-time compare stops a timing leak, the throttle stops guessing at volume.
import {
	createError,
	defineEventHandler,
	getRequestIP,
	readBody,
	setResponseHeader,
	useSession,
} from "h3";
import {
	type SessionData,
	sessionConfig,
	timingSafeEqual,
	uiPassword,
} from "../../util/auth";
import {
	recordLoginFailure,
	recordLoginSuccess,
	throttleRetryAfterMs,
} from "../../util/loginThrottle";

export default defineEventHandler(async (event) => {
	const expected = uiPassword();
	if (!expected) {
		throw createError({
			statusCode: 503,
			statusMessage: "auth not configured",
		});
	}
	// The socket peer address — deliberately NOT trusting X-Forwarded-For (spoofable unless we sit
	// behind a known proxy, which the packaged console does not). Falls back to a single shared bucket
	// if the address is somehow unavailable, so the throttle still applies.
	const ip = getRequestIP(event) ?? "unknown";

	// Throttle BEFORE touching the password so a locked-out client can't keep the guess loop spinning.
	const wait = throttleRetryAfterMs(ip);
	if (wait > 0) {
		setResponseHeader(event, "Retry-After", Math.ceil(wait / 1000));
		throw createError({
			statusCode: 429,
			statusMessage: "too many attempts — try again shortly",
		});
	}

	const body = await readBody<{ password?: string }>(event);
	const password = String(body?.password ?? "");
	if (!timingSafeEqual(password, expected)) {
		recordLoginFailure(ip);
		throw createError({ statusCode: 401, statusMessage: "invalid password" });
	}
	recordLoginSuccess(ip);
	const session = await useSession<SessionData>(event, sessionConfig());
	await session.update({ authenticated: true });
	return { ok: true };
});

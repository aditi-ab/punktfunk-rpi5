// POST /api/v1/update/apply — the ONE proxied route with an extra gate: the console password
// must be re-entered per apply (design host-update-from-web-console.md §4.3). A 7-day session
// cookie alone must not be able to update-and-restart the host; the password is verified HERE
// (only the BFF knows it), stripped, and never forwarded. Wrong attempts share the login
// throttle's per-IP budget, so apply can't be used as a password oracle.
//
// This specific file wins over the `[...]` catch-all (h3 route specificity) — verified in the
// U1 gate; everything else about proxying (bearer injection, loopback TLS scoping, 401→502)
// mirrors ../../[...].ts.
import {
	createError,
	defineEventHandler,
	getRequestIP,
	readBody,
	setResponseHeader,
	setResponseStatus,
} from "h3";
import {
	isLoopbackUrl,
	mgmtToken,
	mgmtUrl,
	timingSafeEqual,
	uiPassword,
} from "../../../../util/auth";
import {
	recordLoginFailure,
	recordLoginSuccess,
	throttleRetryAfterMs,
} from "../../../../util/loginThrottle";

export default defineEventHandler(async (event) => {
	const expected = uiPassword();
	if (!expected) {
		throw createError({ statusCode: 503, statusMessage: "auth not configured" });
	}
	const ip = getRequestIP(event) ?? "unknown";
	const wait = throttleRetryAfterMs(ip);
	if (wait > 0) {
		setResponseHeader(event, "Retry-After", Math.ceil(wait / 1000));
		throw createError({
			statusCode: 429,
			statusMessage: "too many attempts — try again shortly",
		});
	}

	const body = await readBody<{ password?: string; force?: boolean }>(event);
	const password = String(body?.password ?? "");
	if (!timingSafeEqual(password, expected)) {
		recordLoginFailure(ip);
		throw createError({
			statusCode: 401,
			statusMessage: "password confirmation failed",
		});
	}
	recordLoginSuccess(ip);

	const token = mgmtToken();
	if (!token) {
		setResponseStatus(event, 503);
		return { error: "management token not configured" };
	}
	const base = mgmtUrl();
	const init: RequestInit = {
		method: "POST",
		headers: {
			authorization: `Bearer ${token}`,
			"content-type": "application/json",
		},
		// The password stops here — the host only ever sees the force flag.
		body: JSON.stringify({ force: body?.force === true }),
	};
	if (isLoopbackUrl(base)) {
		// Bun.fetch extension (see ../../[...].ts for why this is scoped per-request).
		(init as unknown as { tls: { rejectUnauthorized: boolean } }).tls = {
			rejectUnauthorized: false,
		};
	}
	const upstream = await fetch(`${base}/api/v1/update/apply`, init);
	if (upstream.status === 401) {
		throw createError({
			statusCode: 502,
			statusMessage:
				"management API rejected the host token (check PUNKTFUNK_MGMT_TOKEN)",
		});
	}
	setResponseStatus(event, upstream.status);
	setResponseHeader(event, "content-type", "application/json");
	return upstream.text();
});

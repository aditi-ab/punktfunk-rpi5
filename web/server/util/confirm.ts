// Password re-confirmation for the routes where an authenticated session is NOT enough.
//
// The console's session cookie lives for 7 days, so on its own it must not be able to run new code
// on the host. Three routes clear that bar and each re-verifies the console password HERE (only the
// BFF knows it), strips it, and never forwards it:
//
//   - POST /api/v1/update/apply            — update-and-restart the host
//   - POST /api/v1/store/install           — but only for a RAW SPEC (`accept_unverified`), which
//                                            runs an unreviewed package
//   - PUT  /api/v1/store/sources/{name}    — adds a catalog SOURCE, i.e. a new trust root
//
// A catalog install from an already-trusted source is deliberately NOT gated: the operator made
// that trust decision when they added the source, and re-prompting on every install would train
// them to type the password without reading. The gate belongs at the trust boundary, not past it.
//
// Wrong attempts share the login throttle's per-peer budget, so none of these can be used as a
// password oracle, and a lockout covers all of them at once.
import { createError, type H3Event, setResponseHeader } from "h3";
import { peerAddress, timingSafeEqual, uiPassword } from "./auth";
import {
	recordLoginFailure,
	recordLoginSuccess,
	throttleRetryAfterMs,
} from "./loginThrottle";

/**
 * Verify the re-entered console password, or throw the right HTTP error (503 unconfigured,
 * 429 throttled, 401 wrong). Returns nothing on success — the caller proceeds.
 */
export function confirmPassword(event: H3Event, password: unknown): void {
	const expected = uiPassword();
	if (!expected) {
		throw createError({
			statusCode: 503,
			statusMessage: "auth not configured",
		});
	}
	const ip = peerAddress(event);
	const wait = throttleRetryAfterMs(ip);
	if (wait > 0) {
		setResponseHeader(event, "Retry-After", Math.ceil(wait / 1000));
		throw createError({
			statusCode: 429,
			statusMessage: "too many attempts — try again shortly",
		});
	}
	if (!timingSafeEqual(String(password ?? ""), expected)) {
		recordLoginFailure(ip);
		throw createError({
			statusCode: 401,
			statusMessage: "password confirmation failed",
		});
	}
	recordLoginSuccess(ip);
}

// One-shot forward to the management API, for the handful of routes that need their own handler
// (a password gate, a rewritten body) instead of the generic `/api/**` passthrough in
// routes/api/[...].ts. Everything about how we talk upstream is identical to the passthrough:
// server-side bearer injection, loopback-scoped TLS relaxation, and 401 → 502 so a host-token
// misconfiguration can't bounce a logged-in user into a redirect loop.
import {
	createError,
	type H3Event,
	setResponseHeader,
	setResponseStatus,
} from "h3";
import { loopbackTls, mgmtToken, mgmtUrl } from "./auth";

/**
 * Every field of a host request model, each present and possibly `undefined`. A password route
 * rebuilds its upstream body as `{…} satisfies AllFields<Model>`, so `tsc` fails when the host
 * grows a field the rebuild would strip. `JSON.stringify` drops the `undefined` ones.
 */
export type AllFields<T> = { [K in keyof Required<T>]: T[K] | undefined };

/** Forward a JSON body to `path` on the management API and relay the upstream response verbatim.
 * Omit `body` for a bodiless method (GET) — a read whose RESPONSE we rewrite. */
export async function forwardJson(
	event: H3Event,
	path: string,
	method: string,
	body?: unknown,
): Promise<string> {
	const token = mgmtToken();
	if (!token) {
		setResponseStatus(event, 503);
		setResponseHeader(event, "content-type", "application/json");
		return JSON.stringify({ error: "management token not configured" });
	}
	const base = mgmtUrl();
	const init: RequestInit = {
		method,
		headers: {
			authorization: `Bearer ${token}`,
			...(body === undefined ? {} : { "content-type": "application/json" }),
		},
		body: body === undefined ? undefined : JSON.stringify(body),
	};
	// Bun.fetch extension — pinned per request, never process-wide (see routes/api/[...].ts).
	Object.assign(init, loopbackTls(base));
	// A dead/unstarted host makes `fetch` reject. The generic passthrough answers 502 for that, so
	// these routes must too — an unreachable upstream is not a console bug, and letting the
	// rejection escape would surface it as a bare 500 "Server Error".
	let upstream: Response;
	try {
		upstream = await fetch(`${base}${path}`, init);
	} catch (cause) {
		throw createError({
			statusCode: 502,
			statusMessage: "management API unreachable",
			cause,
		});
	}
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
}

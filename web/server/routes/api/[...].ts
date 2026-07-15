// /api/** → the management API. By the time we get here the gate (middleware/auth.ts) has
// confirmed an authenticated session. We inject the management bearer token server-side
// (the browser never sees it) and drop the browser's own cookies/auth from the upstream
// request, then proxy. The management API itself binds loopback only — this proxy is the
// ONLY path to it from the LAN, and it's authenticated.
import {
	createError,
	defineEventHandler,
	getRequestURL,
	proxyRequest,
	setResponseStatus,
} from "h3";
import { isLoopbackUrl, mgmtToken, mgmtUrl } from "../../util/auth";

export default defineEventHandler((event) => {
	const { pathname, search } = getRequestURL(event);
	const base = mgmtUrl();
	const target = `${base}${pathname}${search}`;
	const token = mgmtToken();
	// The mgmt API now requires a token always. Without one configured, forwarding an empty bearer
	// would just bounce as 401 — fail fast and legibly instead (the packaged service sources the
	// host's ~/.config/punktfunk/mgmt-token, so this only fires on a misconfigured/early-start deploy).
	if (!token) {
		setResponseStatus(event, 503);
		return {
			error:
				"management token not configured (PUNKTFUNK_MGMT_TOKEN / ~/.config/punktfunk/mgmt-token)",
		};
	}
	// TLS scoping (replaces the old process-wide NODE_TLS_REJECT_UNAUTHORIZED=0): the host presents a
	// SELF-SIGNED, no-SAN identity cert on loopback, which normal verification rejects. We relax
	// verification ONLY for this one loopback hop, via Bun's per-request `tls` option — so any OTHER
	// outbound TLS the process ever makes still verifies normally (the global env unverified
	// everything). If the operator points PUNKTFUNK_MGMT_URL at a NON-loopback host, we do NOT relax:
	// a remote mgmt API must present a valid chain, which is stricter than the old blanket accept.
	const fetchOptions = isLoopbackUrl(base)
		? // `tls` is a Bun.fetch extension (the console runs on bun — Bun.serve/`bun .output/...`), not
			// in the standard RequestInit type, so cast through unknown.
			({ tls: { rejectUnauthorized: false } } as unknown as RequestInit)
		: undefined;

	return proxyRequest(event, target, {
		fetchOptions,
		headers: {
			// Overwrite, not append: the host-held token replaces anything the browser sent.
			authorization: `Bearer ${token}`,
			// Don't forward the session cookie to the management API.
			cookie: "",
		},
		onResponse: (_event, response) => {
			// This handler only runs AFTER the gate (middleware/auth.ts) confirmed a valid session, so
			// a 401 HERE is the management API rejecting OUR host token — a server/deploy misconfig, not
			// an expired user session. Forwarding it would make the browser bounce a logged-in user to
			// /login, where re-auth succeeds but the next call 401s again → a redirect loop. Surface it
			// as a 502 (upstream failure) so the console shows an error instead of looping.
			if (response.status === 401) {
				throw createError({
					statusCode: 502,
					statusMessage:
						"management API rejected the host token (check PUNKTFUNK_MGMT_TOKEN)",
				});
			}
		},
	});
});

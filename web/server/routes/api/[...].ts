// /api/** → the management API. By the time we get here the gate (middleware/auth.ts) has
// confirmed an authenticated session. We inject the management bearer token server-side
// (the browser never sees it) and drop the browser's own cookies/auth from the upstream
// request, then proxy. The management API itself binds loopback only — this proxy is the
// ONLY path to it from the LAN, and it's authenticated.
import {
	defineEventHandler,
	getRequestURL,
	proxyRequest,
	setResponseStatus,
} from "h3";
import { loopbackTls, mgmtUrl, normalizePath } from "../../util/auth";
import { assertHostTokenAccepted, mgmtBearer } from "../../util/forward";

export default defineEventHandler((event) => {
	const { pathname, search } = getRequestURL(event);
	// A plugin UI's proxy credential (its per-boot secret) is fetched server-side by the
	// /plugin-ui proxy and must NEVER reach a browser — deny it on the generic passthrough so a
	// session-authed page can't read it (plugin-ui-surface §5, D6). The secret-free list at
	// /api/v1/plugins is fine; only the {id}/ui-credential leaf is blocked.
	//
	// Matched against the NORMALIZED path as well as the raw one: `/api//v1/...`, `/api/./v1/...`
	// and percent-encoded variants all reach the same upstream route, and a denylist that only
	// knows the canonical spelling is one router-quirk away from leaking the secret.
	const denied = /^\/api\/v1\/plugins\/[^/]+\/ui-credential\/?$/i;
	if (denied.test(pathname) || denied.test(normalizePath(pathname))) {
		setResponseStatus(event, 403);
		return {
			error: "plugin UI credentials are not accessible from the browser",
		};
	}
	// The bearer and the 401 rule are `mgmtFetch`'s; the relay, and its 502 for a dead host, is
	// `proxyRequest`'s.
	const base = mgmtUrl();
	return proxyRequest(event, `${base}${pathname}${search}`, {
		// `tls` is a Bun.fetch extension, not in the standard RequestInit type.
		fetchOptions: loopbackTls(base) as unknown as RequestInit | undefined,
		headers: {
			// Overwrite, not append: the host-held token replaces anything the browser sent.
			authorization: mgmtBearer(),
			// Don't forward the session cookie to the management API.
			cookie: "",
		},
		onResponse: (_event, response) => assertHostTokenAccepted(response),
	});
});

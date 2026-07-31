// /plugin-ui/<id>/** → a plugin's loopback UI server (plugin-ui-surface §5). By the time we get
// here the gate (middleware/auth.ts) has confirmed a session — a plugin UI is reachable only by the
// logged-in operator, on the console's own origin, with no separate password. We look up the
// plugin's `{port, secret}` server-side, inject the secret as a bearer, strip the browser's cookie,
// and stream the response through (SSE included). The plugin only ever gets dialed on 127.0.0.1.
//
// This route runs in the built Bun/Nitro server. In `vite dev` a small middleware in vite.config.ts
// handles `/plugin-ui` instead (it intercepts before this route, like the /api dev proxy).
import {
	defineEventHandler,
	getProxyRequestHeaders,
	getRequestURL,
	readRawBody,
	sendWebResponse,
	setResponseStatus,
} from "h3";
import {
	bustCredential,
	fetchUiCredential,
	PLUGIN_ID_RE,
} from "../../util/pluginProxy";

export default defineEventHandler(async (event) => {
	const { pathname, search } = getRequestURL(event);
	// /plugin-ui/<id>/<rest…>
	const m = pathname.match(/^\/plugin-ui\/([^/]+)(\/.*)?$/);
	const id = m?.[1];
	if (!id || !PLUGIN_ID_RE.test(id)) {
		setResponseStatus(event, 404);
		return { error: "not a valid plugin-ui path" };
	}
	const rest = m?.[2] ?? "/";
	const prefix = `/plugin-ui/${id}`;

	// Forwardable request headers (h3 strips hop-by-hop + host); we set our own auth and drop the
	// session cookie so plugin code never sees it.
	const headers = getProxyRequestHeaders(event) as Record<string, string>;
	delete headers.cookie;
	delete headers.authorization;
	headers["x-forwarded-prefix"] = prefix;
	const method = event.method;
	// Only read a body for the methods that can carry one. `readRawBody` asserts a payload method,
	// so calling it for OPTIONS (a plugin UI's CORS preflight, or any client probing Allow) threw
	// 405 out of the CONSOLE before the plugin was ever dialed.
	const body = BODY_METHODS.has(method)
		? ((await readRawBody(event, false)) as Uint8Array | undefined)
		: undefined;

	// One proxied attempt; `null` means the plugin is unreachable (unregistered, or its port died).
	const attempt = async (bustCache: boolean): Promise<Response | null> => {
		const cred = await fetchUiCredential(id, { bustCache });
		if (!cred) return null;
		const target = `http://127.0.0.1:${cred.port}${rest}${search}`;
		try {
			return await fetch(target, {
				method,
				headers: { ...headers, authorization: `Bearer ${cred.secret}` },
				body: body as BodyInit | undefined,
				redirect: "manual",
			});
		} catch {
			// The port is dead (plugin crashed/restarted on a new port): drop the stale credential so
			// the next request re-resolves it.
			bustCredential(id);
			return null;
		}
	};

	let resp = await attempt(false);
	// Stale secret after a plugin restart (S7): the plugin rejects our cached secret — re-fetch once.
	if (resp?.status === 401) {
		const retry = await attempt(true);
		if (retry) resp = retry;
	}
	if (!resp) {
		setResponseStatus(event, 502);
		return { error: `plugin "${id}" is not running` };
	}
	return sendWebResponse(event, sanitize(resp));
});

/** Methods that may carry a request body. Anything else (GET, HEAD, OPTIONS, TRACE) must not be
 * handed to `readRawBody`. */
const BODY_METHODS = new Set(["POST", "PUT", "PATCH", "DELETE"]);

/**
 * Fix up a plugin's response before it goes out on the console's origin.
 *
 * - `content-encoding` / `content-length` / `transfer-encoding`: `fetch` already decoded the body,
 *   but the plugin's original headers survive on the Response. Re-emitting `content-encoding: gzip`
 *   over plaintext makes the browser fail to decode the page, and a stale `content-length` truncates
 *   it. The framing belongs to OUR response, so drop the plugin's and let it be recomputed.
 * - `set-cookie`: a plugin runs on the console's own origin, so any cookie it sets is scoped to the
 *   console — it could collide with (or shadow) `pf_session`. A plugin UI has no business setting
 *   cookies on this origin; it authenticates with the injected per-boot bearer.
 */
function sanitize(resp: Response): Response {
	const headers = new Headers(resp.headers);
	headers.delete("content-encoding");
	headers.delete("content-length");
	headers.delete("transfer-encoding");
	headers.delete("set-cookie");
	// 204/304 must not carry a body — passing one through throws in the Response constructor.
	const bodyless = resp.status === 204 || resp.status === 304;
	return new Response(bodyless ? null : resp.body, {
		status: resp.status,
		statusText: resp.statusText,
		headers,
	});
}

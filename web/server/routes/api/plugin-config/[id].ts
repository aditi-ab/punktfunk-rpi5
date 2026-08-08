// GET/PUT /api/plugin-config/<id> — a plugin's `__config`, readable from the CONSOLE origin.
//
// The Library section's "Game sources" settings drawer renders a form from a library plugin's
// `__config` (the kit's generic settings surface, so a scanner needs no SPA of its own). It fetched
// `/plugin-ui/<id>/__config` same-origin — and that stopped working the moment plugin UIs moved to
// their own origin (2026-08-05 review H-3): `middleware/auth.ts` answers 404 for `/plugin-ui/**` on
// the console origin, unconditionally and by design. The drawer is the only NON-IFRAME consumer of
// that path, so nothing else noticed, and settings silently failed to open for every library plugin.
//
// The fix is deliberately not "point the drawer at the plugin origin". That needs CORS plus
// cross-site cookies, and it would put a plugin-controlled response inside a credentialed
// cross-origin fetch — reopening the hole the split exists to close. What the drawer needs is DATA,
// not an embedded UI: this reads the JSON server-side over loopback and returns it same-origin, so
// no plugin HTML or JS is ever served from the console origin.
//
// Auth: `/api/**` is always session-gated (`isPublicPath`), so reaching here means a logged-in
// operator, and it answers 401 as JSON rather than redirecting — which is what a `fetch` needs. The
// plugin's per-boot secret stays server-side, exactly as in the `/plugin-ui` proxy.
import {
	defineEventHandler,
	getRouterParam,
	readRawBody,
	setResponseStatus,
} from "h3";
import {
	bustCredential,
	fetchUiCredential,
	PLUGIN_ID_RE,
} from "../../../util/pluginProxy";

/** `GET` reads schema + current value; `PUT` validates and saves. Nothing else is forwarded. */
const ALLOWED = new Set(["GET", "PUT"]);

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id");
	if (!id || !PLUGIN_ID_RE.test(id)) {
		setResponseStatus(event, 404);
		return { error: "not a valid plugin id" };
	}
	const method = event.method;
	if (!ALLOWED.has(method)) {
		setResponseStatus(event, 405);
		return { error: "method not allowed" };
	}
	// Read the body BEFORE the retry below: `readRawBody` drains the stream, so a second attempt
	// would forward an empty PUT and quietly save `{}` over the operator's config.
	const body =
		method === "PUT"
			? ((await readRawBody(event, false)) as Uint8Array | undefined)
			: undefined;

	const attempt = async (bustCache: boolean): Promise<Response | null> => {
		const cred = await fetchUiCredential(id, { bustCache });
		if (!cred) return null;
		try {
			return await fetch(`http://127.0.0.1:${cred.port}/__config`, {
				method,
				headers: {
					authorization: `Bearer ${cred.secret}`,
					...(method === "PUT" ? { "content-type": "application/json" } : {}),
				},
				body: body as BodyInit | undefined,
			});
		} catch {
			return null;
		}
	};

	// A plugin's secret rotates when its process restarts, which happens well inside the credential
	// cache's TTL — so a 401 here means "stale credential", not "denied". Same one-shot retry the
	// `/plugin-ui` proxy does, for the same reason.
	let res = await attempt(false);
	if (res?.status === 401) {
		bustCredential(id);
		res = await attempt(true);
	}
	if (!res) {
		setResponseStatus(event, 502);
		return { error: `plugin ${id} is not reachable` };
	}

	setResponseStatus(event, res.status);
	// Pass the plugin's own body through untouched: a 400 from `__config` carries the decode issue
	// the drawer shows the operator, and rewriting it would throw away the only useful part.
	const text = await res.text();
	try {
		return JSON.parse(text) as unknown;
	} catch {
		return { error: text || `plugin ${id} answered ${res.status}` };
	}
});

// The single server-side gate. Runs for EVERY request to the deployed Bun/Nitro server
// (pages, the /api proxy, everything) before routing. Unauthenticated requests are
// redirected to /login (page navigations) or rejected 401 (/api). Fails CLOSED if
// PUNKTFUNK_UI_PASSWORD is unset, so a misconfigured LAN-exposed server admits no one.
import {
	defineEventHandler,
	getRequestHeader,
	getRequestURL,
	type H3Event,
	sendRedirect,
	setResponseHeader,
	setResponseStatus,
	useSession,
} from "h3";
import {
	isPublicPath,
	type SessionData,
	sessionConfig,
	sessionEpoch,
	uiPassword,
} from "../util/auth";
import {
	consoleOriginPort,
	consoleOriginScheme,
	frameAncestorSource,
	isPluginUiPath,
	listenerOf,
} from "../util/pluginOrigin";

export default defineEventHandler(async (event) => {
	const { pathname } = getRequestURL(event);
	const listener = listenerOf(event);
	const isPluginPath = isPluginUiPath(pathname);

	// ── the origin split (2026-08-05 review H-3) ────────────────────────────────────────────────
	//
	// Plugin UIs live on their own origin (see nitro-entry/bun-https.mjs). Enforcing that is two
	// refusals, and BOTH are load-bearing:
	//
	//  - the console origin must not serve `/plugin-ui/**`, or the old same-origin path still works
	//    and nothing has changed;
	//  - the plugin origin must not serve anything ELSE — above all not `/api/**`. Plugin JS is
	//    same-origin with the plugin listener, so if that listener proxied `/api/**` the BFF would
	//    attach the operator's admin bearer to the plugin's own fetch and hand back exactly the
	//    escalation we just moved.
	//
	// Unconditional, not conditional on the plugin listener having bound: if it did not, plugin UIs
	// are disabled and refusing here is the correct answer, not a reason to fall back. (`vite dev`
	// serves one origin, but its own middleware answers `/plugin-ui` before Nitro is reached, so
	// this never fires there.)
	if (listener === "console" && isPluginPath) {
		setResponseStatus(event, 404);
		return { error: "plugin UIs are served from their own origin" };
	}
	if (listener === "plugin" && !isPluginPath) {
		setResponseStatus(event, 404);
		return { error: "this origin serves plugin UIs only" };
	}

	// Baseline response headers for everything this server emits. Deliberately modest: a plugin's
	// own UI is third-party code we don't control, so a script-src policy tight enough to be worth
	// having would break the pages it serves. What is safe to assert unconditionally still closes
	// the cheap holes:
	//   nosniff        — a plugin serving text/plain that "looks like" HTML can't be sniffed into it
	//   frame-ancestors— who may frame this; see below, it differs per origin
	//   object-src     — no Flash/applet embedding anywhere
	//   base-uri       — a stray <base> can't repoint every relative URL on the page
	//   Referrer-Policy— never leak a console path (which can carry ids) to an external homepage link
	setResponseHeader(event, "X-Content-Type-Options", "nosniff");
	setResponseHeader(event, "Referrer-Policy", "no-referrer");
	// `frame-ancestors 'self'` is right for the console and WRONG for the plugin origin: 'self'
	// there means the plugin origin, and the console — now a different origin — is precisely who
	// needs to frame it. So the plugin origin names the console explicitly, and nobody else.
	setResponseHeader(
		event,
		"Content-Security-Policy",
		`frame-ancestors ${listener === "plugin" ? consoleFrameAncestor(event) : "'self'"}; object-src 'none'; base-uri 'self'`,
	);

	// Same-origin check for every MUTATING request (defense in depth beyond SameSite=Lax,
	// added with the update-apply route where CSRF ≈ code execution — design
	// host-update-from-web-console.md §4.3). `Sec-Fetch-Site` is browser-set and unforgeable
	// from a page; absent (curl, very old browsers) ⇒ allowed — the console's threat here is
	// a BROWSER being ridden cross-site, and every riding browser sends the header.
	// `same-site` is rejected too: with an IP-address origin, another port on the same box
	// counts as same-site, and nothing on another port has business mutating the console.
	// Applies to public paths as well (login CSRF), before any session logic.
	const method = event.method?.toUpperCase?.() ?? "GET";
	if (method !== "GET" && method !== "HEAD" && method !== "OPTIONS") {
		const site = getRequestHeader(event, "sec-fetch-site")?.toLowerCase();
		if (site && site !== "same-origin" && site !== "none") {
			setResponseStatus(event, 403);
			return { error: "cross-site request refused" };
		}
	}

	if (isPublicPath(pathname)) return;

	// Misconfigured: refuse everything rather than serve open on the LAN.
	if (!uiPassword()) {
		setResponseStatus(event, 503);
		return { error: "auth not configured: set PUNKTFUNK_UI_PASSWORD" };
	}

	const session = await useSession<SessionData>(event, sessionConfig());
	// The epoch check is what makes logout mean something: a cookie sealed before the last
	// revocation unseals fine but no longer matches, so it is refused like any other bad session.
	if (session.data.authenticated && session.data.epoch === sessionEpoch())
		return; // authenticated — let it through

	if (pathname.startsWith("/api")) {
		setResponseStatus(event, 401);
		return { error: "unauthorized" };
	}
	// The plugin origin has no /login to bounce to — it serves plugin UIs and nothing else, so a
	// redirect there would land on this middleware's own 404. Answer plainly instead; the console
	// probes plugin liveness server-side and renders the session-expired state itself.
	if (listener === "plugin") {
		setResponseStatus(event, 401);
		return { error: "unauthorized" };
	}
	// Page navigation → bounce to the login screen, remembering where they were headed.
	return sendRedirect(
		event,
		`/login?next=${encodeURIComponent(pathname)}`,
		302,
	);
});

/**
 * The console origin, as a `frame-ancestors` source, derived from the request the PLUGIN origin is
 * answering: the hostname is whatever name the operator actually browsed to (an IP, an mDNS name, a
 * hostname — so the policy matches their address bar), plus the console's port and scheme.
 *
 * ⚠ The scheme must NOT come from the request. `getRequestURL` reports `http:` on an HTTPS listener
 * here — Nitro hands the app a synthetic request with no TLS socket — so this named
 * `http://host:47992` as the only permitted ancestor of a console the operator was reading over
 * HTTPS, and every plugin UI came up as an empty panel with `ERR_BLOCKED_BY_RESPONSE`. It is taken
 * from the listener's own TLS state instead (`consoleOriginScheme`), with `x-forwarded-proto`
 * winning when something in front terminated TLS for us — that is the one case where the browser's
 * scheme differs from this process's.
 *
 * Falls back to `'none'` rather than `'self'` or `*` when the console port is unknown: an unframable
 * plugin page is a visible, harmless failure, and the alternatives are a policy that either does
 * nothing or lets any page on the LAN frame a logged-in plugin UI.
 */
function consoleFrameAncestor(event: H3Event): string {
	const port = consoleOriginPort();
	if (!port) return "'none'";
	const url = getRequestURL(event);
	return frameAncestorSource({
		forwardedProto: getRequestHeader(event, "x-forwarded-proto"),
		listenerScheme: consoleOriginScheme(),
		requestScheme: url.protocol,
		hostname: url.hostname,
		port,
	});
}

// The single server-side gate. Runs for EVERY request to the deployed Bun/Nitro server
// (pages, the /api proxy, everything) before routing. Unauthenticated requests are
// redirected to /login (page navigations) or rejected 401 (/api). Fails CLOSED if
// PUNKTFUNK_UI_PASSWORD is unset, so a misconfigured LAN-exposed server admits no one.
import {
	defineEventHandler,
	getRequestHeader,
	getRequestURL,
	sendRedirect,
	setResponseHeader,
	setResponseStatus,
	useSession,
} from "h3";
import {
	isPublicPath,
	type SessionData,
	sessionConfig,
	uiPassword,
} from "../util/auth";

export default defineEventHandler(async (event) => {
	const { pathname } = getRequestURL(event);

	// Baseline response headers for everything this server emits. Deliberately modest: a plugin's
	// own UI is proxied onto THIS origin (/plugin-ui/**), so a script-src policy tight enough to be
	// worth having would break third-party plugin pages we don't control. What is safe to assert
	// unconditionally still closes the cheap holes:
	//   nosniff        — a plugin serving text/plain that "looks like" HTML can't be sniffed into it
	//   frame-ancestors— only our own pages may frame the console (the plugin iframes are same-origin)
	//   object-src     — no Flash/applet embedding anywhere
	//   base-uri       — a stray <base> can't repoint every relative URL on the page
	//   Referrer-Policy— never leak a console path (which can carry ids) to an external homepage link
	setResponseHeader(event, "X-Content-Type-Options", "nosniff");
	setResponseHeader(event, "Referrer-Policy", "no-referrer");
	setResponseHeader(
		event,
		"Content-Security-Policy",
		"frame-ancestors 'self'; object-src 'none'; base-uri 'self'",
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
	if (session.data.authenticated) return; // authenticated — let it through

	if (pathname.startsWith("/api")) {
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

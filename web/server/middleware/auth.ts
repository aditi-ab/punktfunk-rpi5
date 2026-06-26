// The single server-side gate. Runs for EVERY request to the deployed Bun/Nitro server
// (pages, the /api proxy, everything) before routing. Unauthenticated requests are
// redirected to /login (page navigations) or rejected 401 (/api). Fails CLOSED if
// PUNKTFUNK_UI_PASSWORD is unset, so a misconfigured LAN-exposed server admits no one.
import {
	defineEventHandler,
	getRequestURL,
	sendRedirect,
	setResponseStatus,
	useSession,
} from "h3";
import {
	isPublicPath,
	sessionConfig,
	uiPassword,
	type SessionData,
} from "../util/auth";

export default defineEventHandler(async (event) => {
	const { pathname } = getRequestURL(event);
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

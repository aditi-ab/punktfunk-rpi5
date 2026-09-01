// Rolling-upgrade endpoint for management-token HMAC tickets minted by older host builds.
// Current launchers use the ordinary login page so no bearer URL enters process arguments.
import {
	createError,
	defineEventHandler,
	getQuery,
	sendRedirect,
	useSession,
} from "h3";
import {
	mgmtToken,
	type SessionData,
	sessionConfig,
	sessionEpoch,
} from "../../util/auth";
import { verifyHandoff } from "../../util/handoff";

/** Tickets already redeemed, so a captured one cannot be replayed inside its TTL. Process-lifetime
 * on purpose: a console restart invalidates everything outstanding, which fails closed. Entries
 * older than the TTL are swept on each call, so it cannot grow without bound. */
const redeemed = new Map<string, number>();

export default defineEventHandler(async (event) => {
	const key = mgmtToken();
	if (!key) {
		// Without the token the console can verify nothing — and it also cannot reach the host at
		// all, so there is nothing behind this door worth opening.
		throw createError({
			statusCode: 503,
			statusMessage: "handoff not configured",
		});
	}

	const verdict = await verifyHandoff(
		String(getQuery(event).t ?? ""),
		key,
		redeemed,
	);
	if (!verdict.ok) {
		// One status for every rejection. Telling a caller *which* way their ticket was wrong is
		// free information for someone probing, and the operator's own ticket never fails.
		throw createError({
			statusCode: 401,
			statusMessage: "invalid handoff ticket",
		});
	}

	const session = await useSession<SessionData>(event, sessionConfig());
	await session.update({ authenticated: true, epoch: sessionEpoch() });
	// Land on the console proper rather than returning JSON: a browser the desktop just launched is
	// behind this request, and the person driving it wants the page.
	return sendRedirect(event, "/", 302);
});

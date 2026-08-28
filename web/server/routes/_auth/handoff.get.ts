// GET /_auth/handoff?t=<ts>.<nonce>.<mac> — log in the person who can already prove they are the
// operator of this box, without asking them for the password again.
//
// **This is not "skip the login".** The console binds all interfaces (0.0.0.0:47992) so it can be
// reached from a phone on the LAN, and its admin surface is pairing, unpair and session control.
// Trusting the *network* would hand that to anyone on the LAN. What this trusts instead is the
// **mgmt token**: a 0600 file in the host's 0700 config directory, readable only by the uid the
// host runs as. `punktfunk-host ctl console-url` mints a ticket with it; this route verifies it
// with the copy the console already holds. Somebody who can read that file can already drive the
// whole admin API — it is the credential this console's own proxy presents — so letting them skip
// a password they could simply read widens nothing.
//
// A visitor without a ticket still meets the login page. Nothing about the exposed surface moves.
//
// The decision itself lives in `util/handoff` so it can be tested without an h3 event; this file
// owns only the cookie and the redirect.
import { createError, defineEventHandler, getQuery, sendRedirect, useSession } from "h3";
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
		throw createError({ statusCode: 503, statusMessage: "handoff not configured" });
	}

	const verdict = await verifyHandoff(
		String(getQuery(event).t ?? ""),
		key,
		redeemed,
	);
	if (!verdict.ok) {
		// One status for every rejection. Telling a caller *which* way their ticket was wrong is
		// free information for someone probing, and the operator's own ticket never fails.
		throw createError({ statusCode: 401, statusMessage: "invalid handoff ticket" });
	}

	const session = await useSession<SessionData>(event, sessionConfig());
	await session.update({ authenticated: true, epoch: sessionEpoch() });
	// Land on the console proper rather than returning JSON: a browser the desktop just launched is
	// behind this request, and the person driving it wants the page.
	return sendRedirect(event, "/", 302);
});

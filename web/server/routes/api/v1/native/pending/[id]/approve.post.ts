// POST /api/v1/native/pending/{id}/approve — pairs a knocking device's certificate fingerprint,
// with no PIN ceremony at all, defaulting to full and permanent access for a first pairing. It is
// the shortest path from a session cookie to keyboard and mouse on the host desktop, so it sits
// behind the console password like every other code-execution route (util/confirm.ts).
//
// Wins over the `/api/**` catch-all by h3 route specificity. Deny is NOT gated — it only ever
// narrows what the host trusts.
import { createError, defineEventHandler, getRouterParam, readBody } from "h3";
import { confirmPassword } from "../../../../../../util/confirm";
import { forwardJson } from "../../../../../../util/forward";

interface ApproveBody {
	name?: string | null;
	grants?: number;
	expires_in_secs?: number;
	password?: string;
}

export default defineEventHandler(async (event) => {
	// The id goes into the upstream path, so it has to be exactly what the contract says it is —
	// a non-negative integer — rather than whatever the router matched.
	const id = Number(getRouterParam(event, "id"));
	if (!Number.isInteger(id) || id < 0) {
		throw createError({ statusCode: 404, statusMessage: "no such pending id" });
	}
	const body = await readBody<ApproveBody>(event);
	confirmPassword(event, body?.password);
	// Rebuild from known fields so the password cannot leak upstream. Absent stays absent: the
	// dialog omits `grants`/`expires_in_secs` to keep a re-knocking device's stored access.
	const { name, grants, expires_in_secs } = body ?? {};
	const upstream: Omit<ApproveBody, "password"> = {};
	if (typeof name === "string") upstream.name = name;
	if (grants !== undefined) upstream.grants = grants;
	if (expires_in_secs !== undefined) upstream.expires_in_secs = expires_in_secs;
	return forwardJson(
		event,
		`/api/v1/native/pending/${id}/approve`,
		"POST",
		upstream,
	);
});

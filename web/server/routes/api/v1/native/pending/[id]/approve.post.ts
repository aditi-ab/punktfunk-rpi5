// POST /api/v1/native/pending/{id}/approve — pairs a knocking device's certificate fingerprint,
// with no PIN ceremony at all, defaulting to full and permanent access for a first pairing. It is
// the shortest path from a session cookie to keyboard and mouse on the host desktop, so it sits
// behind the console password like every other code-execution route (util/confirm.ts).
//
// Wins over the `/api/**` catch-all by h3 route specificity. Deny is NOT gated — it only ever
// narrows what the host trusts.
import { createError, defineEventHandler, getRouterParam, readBody } from "h3";
import type { ApprovePending } from "../../../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../../util/forward";

export default defineEventHandler(async (event) => {
	// The id goes into the upstream path, so it has to be exactly what the contract says it is —
	// a non-negative integer — rather than whatever the router matched.
	const id = Number(getRouterParam(event, "id"));
	if (!Number.isInteger(id) || id < 0) {
		throw createError({ statusCode: 404, statusMessage: "no such pending id" });
	}
	const body = await readBody<ApprovePending & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	// Rebuild from known fields so the password cannot leak upstream. Absent stays absent: the
	// dialog omits `grants`/`expires_in_secs` to keep a re-knocking device's stored access.
	const upstream = {
		name: typeof body?.name === "string" ? body.name : undefined,
		grants: body?.grants,
		expires_in_secs: body?.expires_in_secs,
		until_disconnect: body?.until_disconnect,
	} satisfies AllFields<ApprovePending>;
	return forwardJson(
		event,
		`/api/v1/native/pending/${id}/approve`,
		"POST",
		upstream,
	);
});

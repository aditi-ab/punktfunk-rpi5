// POST /api/v1/store/install — wins over the `/api/**` catch-all (h3 route specificity), so the
// raw-spec branch can never reach the host without a password.
//
// Two shapes arrive here:
//   { source, id }                    — a curated catalog entry. Forwarded as-is: the operator
//                                       already made the trust decision when they added the source.
//   { spec, accept_unverified: true } — an unreviewed package, no catalog, no pinning. This is
//                                       arbitrary code execution on the host, so it is gated on the
//                                       console password exactly like update/apply (util/confirm.ts).
import { defineEventHandler, readBody } from "h3";
import { confirmPassword } from "../../../../util/confirm";
import { forwardJson } from "../../../../util/forward";

interface InstallBody {
	source?: string;
	id?: string;
	spec?: string;
	accept_unverified?: boolean;
	password?: string;
}

export default defineEventHandler(async (event) => {
	const body = await readBody<InstallBody>(event);
	const rawSpec = body?.accept_unverified === true;
	if (rawSpec) confirmPassword(event, body?.password);
	// The password stops here — rebuild the upstream body from known fields so it cannot leak
	// through, and so an unexpected extra field can't ride along to the host.
	const upstream = rawSpec
		? { spec: String(body?.spec ?? ""), accept_unverified: true }
		: { source: String(body?.source ?? ""), id: String(body?.id ?? "") };
	return forwardJson(event, "/api/v1/store/install", "POST", upstream);
});

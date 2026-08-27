// POST /api/v1/native/pair/arm — arming a window mints the PIN that pairs a device, and a paired
// device injects keyboard and mouse on the host desktop. That is code execution by any other name,
// so it joins update/apply and the hooks write behind the console password (util/confirm.ts): a
// 7-day session cookie must not be enough to admit a new device.
//
// Wins over the `/api/**` catch-all by h3 route specificity. The PIN comes back in THIS response
// and nowhere else — the polled status has it stripped (../pair.get.ts), so reading it needs the
// password too.
import { defineEventHandler, readBody } from "h3";
import { confirmPassword } from "../../../../../util/confirm";
import { forwardJson } from "../../../../../util/forward";

interface ArmBody {
	ttl_secs?: number;
	fingerprint?: string;
	grants?: number;
	expires_in_secs?: number;
	password?: string;
}

export default defineEventHandler(async (event) => {
	const body = await readBody<ArmBody>(event);
	confirmPassword(event, body?.password);
	// Rebuild from the contract's own fields so the password cannot leak upstream, and so an
	// unexpected extra field can't ride along to the host. Absent stays absent: the console omits
	// `grants`/`expires_in_secs` to mean "keep what a re-pairing device already has".
	const { ttl_secs, fingerprint, grants, expires_in_secs } = body ?? {};
	const upstream: Omit<ArmBody, "password"> = {};
	if (ttl_secs !== undefined) upstream.ttl_secs = ttl_secs;
	if (fingerprint) upstream.fingerprint = fingerprint;
	if (grants !== undefined) upstream.grants = grants;
	if (expires_in_secs !== undefined) upstream.expires_in_secs = expires_in_secs;
	return forwardJson(event, "/api/v1/native/pair/arm", "POST", upstream);
});

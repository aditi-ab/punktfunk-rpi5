// POST /api/v1/native/pair/arm — arming a window mints the PIN that pairs a device, and a paired
// device injects keyboard and mouse on the host desktop. That is code execution by any other name,
// so it joins update/apply and the hooks write behind the console password (util/confirm.ts): a
// 7-day session cookie must not be enough to admit a new device.
//
// Wins over the `/api/**` catch-all by h3 route specificity. The PIN comes back in THIS response
// and nowhere else — the polled status has it stripped (../pair.get.ts), so reading it needs the
// password too.
import { defineEventHandler, readBody } from "h3";
import type { ArmNativePairing } from "../../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../util/forward";

export default defineEventHandler(async (event) => {
	const body = await readBody<ArmNativePairing & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	// Rebuild from the contract's own fields so the password cannot leak upstream, and so an
	// unexpected extra field can't ride along to the host. Absent stays absent: the console omits
	// `grants`/`expires_in_secs` to mean "keep what a re-pairing device already has".
	const upstream = {
		ttl_secs: body?.ttl_secs,
		fingerprint: body?.fingerprint || undefined,
		grants: body?.grants,
		expires_in_secs: body?.expires_in_secs,
		until_disconnect: body?.until_disconnect,
	} satisfies AllFields<ArmNativePairing>;
	return forwardJson(event, "/api/v1/native/pair/arm", "POST", upstream);
});

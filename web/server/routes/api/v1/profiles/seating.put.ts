// PUT /api/v1/profiles/seating — turning seats on installs a display driver, turns Remote Desktop on
// (from the network too, if asked) and can install a Windows role. That is root work like the
// door's, so it asks for the console password (util/confirm.ts). Wins over the catch-all and the
// `[id]` route by h3 route specificity.
import { defineEventHandler, readBody } from "h3";
import type { SeatingChange } from "../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const body = await readBody<SeatingChange & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	const upstream = {
		enabled: body?.enabled,
		allow_rdp_from_network: body?.allow_rdp_from_network,
		prepare: body?.prepare,
	} satisfies AllFields<SeatingChange>;
	return forwardJson(event, "/api/v1/profiles/seating", "PUT", upstream);
});

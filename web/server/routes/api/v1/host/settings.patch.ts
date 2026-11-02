// PATCH /api/v1/host/settings — the plane settings (src/lib/password-gates.ts) open GameStream or the
// browser plane, or weaken them, so a patch that touches one asks for the console password
// (util/confirm.ts). The other settings do not. Wins over the catch-all by h3 route specificity.
import { defineEventHandler } from "h3";
import type { HostSettingsPatch } from "../../../../../src/api/gen/model";
import { touchesPlane } from "../../../../../src/lib/password-gates";
import { confirmWhen, type When } from "../../../../util/confirm";
import { forwardJson } from "../../../../util/forward";

export const when: When = ({ body }) => touchesPlane(body);

export default defineEventHandler(async (event) => {
	const { password: _password, ...patch } =
		await confirmWhen<HostSettingsPatch>(event, when);
	return forwardJson(event, "/api/v1/host/settings", "PATCH", patch);
});

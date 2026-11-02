// POST /api/v1/plugin-access/{plugin}/decide — `allow` hands a plugin a folder, read-write with
// `write`, and can install an emulator, so it asks for the console password (util/confirm.ts).
// `deny` and `forget` only narrow and do not. Wins over the catch-all by h3 route specificity.
import { defineEventHandler, getRouterParam } from "h3";
import type { DecideRequest } from "../../../../../../src/api/gen/model";
import { confirmWhen, type When } from "../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../util/forward";

export const when: When = ({ body }) =>
	body.decision !== "deny" && body.decision !== "forget";

export default defineEventHandler(async (event) => {
	const plugin = getRouterParam(event, "plugin") ?? "";
	const body = await confirmWhen<DecideRequest>(event, when);
	const upstream = {
		path: body.path,
		decision: body.decision,
		write: body.write,
		form: body.form,
	} satisfies AllFields<DecideRequest>;
	return forwardJson(
		event,
		`/api/v1/plugin-access/${encodeURIComponent(plugin)}/decide`,
		"POST",
		upstream,
	);
});

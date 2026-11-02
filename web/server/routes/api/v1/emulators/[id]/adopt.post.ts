// POST /api/v1/emulators/{id}/adopt — the host launches the adopted program for every game of that
// emulator, so naming one is a command by another name: it asks for the console password
// (util/confirm.ts). Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import type { AdoptRequest } from "../../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../util/forward";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<AdoptRequest & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	const upstream = {
		exe: body?.exe,
		forget: body?.forget,
	} satisfies AllFields<AdoptRequest>;
	return forwardJson(
		event,
		`/api/v1/emulators/${encodeURIComponent(id)}/adopt`,
		"POST",
		upstream,
	);
});

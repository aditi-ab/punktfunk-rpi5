// POST /api/v1/emulators/{id}/saves/import — on the operator's lane it reads any absolute file into
// an emulator's save folder, the other half of export's copy. It asks for the console password
// (util/confirm.ts). Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import type { ImportUnitsRequest } from "../../../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../../util/forward";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<ImportUnitsRequest & { password?: string }>(
		event,
	);
	await confirmPassword(event, body?.password);
	const upstream = {
		game: body?.game,
		others: body?.others,
		platform: body?.platform,
		units: body?.units,
	} satisfies AllFields<ImportUnitsRequest>;
	return forwardJson(
		event,
		`/api/v1/emulators/${encodeURIComponent(id)}/saves/import`,
		"POST",
		upstream,
	);
});

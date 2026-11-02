// POST /api/v1/emulators/{id}/saves/export — on the operator's lane `dir` is any absolute folder,
// made when missing, so with import it copies files from anywhere to anywhere as the host user. It
// asks for the console password (util/confirm.ts). Wins over the catch-all by route specificity.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import type { ExportUnitsRequest } from "../../../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../../util/forward";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<ExportUnitsRequest & { password?: string }>(
		event,
	);
	await confirmPassword(event, body?.password);
	const upstream = {
		dir: body?.dir,
		game: body?.game,
		platform: body?.platform,
		units: body?.units,
	} satisfies AllFields<ExportUnitsRequest>;
	return forwardJson(
		event,
		`/api/v1/emulators/${encodeURIComponent(id)}/saves/export`,
		"POST",
		upstream,
	);
});

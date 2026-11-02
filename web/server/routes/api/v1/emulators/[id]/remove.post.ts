// POST /api/v1/emulators/{id}/remove — with `purge` it deletes the emulator's config and saves, so
// that call asks for the console password (util/confirm.ts); a plain remove keeps them and does
// not. Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, getRouterParam } from "h3";
import type { RemoveEmulatorRequest } from "../../../../../../src/api/gen/model";
import { confirmWhen, type When } from "../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../util/forward";

export const when: When = ({ body }) => body.purge === true;

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await confirmWhen<RemoveEmulatorRequest>(event, when);
	const upstream = {
		purge: body.purge === true,
	} satisfies AllFields<RemoveEmulatorRequest>;
	return forwardJson(
		event,
		`/api/v1/emulators/${encodeURIComponent(id)}/remove`,
		"POST",
		upstream,
	);
});

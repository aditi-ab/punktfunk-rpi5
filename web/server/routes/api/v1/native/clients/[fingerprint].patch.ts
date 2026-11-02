// PATCH /api/v1/native/clients/{fingerprint} — a change that gives a device a grant it lacks, or more
// time, turns a guest into a full controller with a session alone, so it asks for the console
// password (util/confirm.ts). A narrowing change does not. Wins over the catch-all by h3 route
// specificity.
import { defineEventHandler, getRouterParam } from "h3";
import type {
	NativeClient,
	UpdateNativeAccess,
} from "../../../../../../src/api/gen/model";
import { widensAccess } from "../../../../../../src/lib/password-gates";
import { confirmWhen, type When } from "../../../../../util/confirm";
import {
	type AllFields,
	forwardJson,
	mgmtFetch,
} from "../../../../../util/forward";

/** Compared with the device as the host stores it now; a device the host can't name widens. */
export const when: When = async ({ body, params }) => {
	const res = await mgmtFetch("/api/v1/native/clients");
	const list: unknown = res.ok ? await res.json() : [];
	const fp = (params.fingerprint ?? "").toLowerCase();
	const stored = Array.isArray(list)
		? (list as NativeClient[]).find((c) => c.fingerprint.toLowerCase() === fp)
		: undefined;
	return widensAccess(stored, body, Math.floor(Date.now() / 1000));
};

export default defineEventHandler(async (event) => {
	const fingerprint = getRouterParam(event, "fingerprint") ?? "";
	const body = await confirmWhen<UpdateNativeAccess>(event, when);
	const upstream = {
		grants: body.grants,
		expires_in_secs: body.expires_in_secs,
		clear_expiry: body.clear_expiry,
		until_disconnect: body.until_disconnect,
	} satisfies AllFields<UpdateNativeAccess>;
	return forwardJson(
		event,
		`/api/v1/native/clients/${encodeURIComponent(fingerprint)}`,
		"PATCH",
		upstream,
	);
});

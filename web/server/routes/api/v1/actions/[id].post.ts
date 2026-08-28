// POST /api/v1/actions/{id} — invoking a host action (sleep/reboot/shutdown,
// design/host-actions.md) is password-gated like update/apply: a 7-day session cookie alone
// must not be able to power the machine off. The password is verified here (only the BFF knows
// it), stripped, and never forwarded — the upstream request body is EMPTY by design (the id is
// the whole request; no field reaches the host's privileged path).
//
// This specific file wins over the `[...]` catch-all (h3 route specificity), which is what
// keeps the catch-all from proxying the invoke UNgated.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import { confirmPassword } from "../../../../util/confirm";
import { forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<{ password?: string }>(event);
	confirmPassword(event, body?.password);
	return forwardJson(
		event,
		`/api/v1/actions/${encodeURIComponent(id)}`,
		"POST",
	);
});

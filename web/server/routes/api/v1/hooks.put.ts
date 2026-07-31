// PUT /api/v1/hooks — writing a hook means writing a SHELL COMMAND the host will execute on its own
// events, as the host user. That is code execution by any other name, so it joins update/apply and
// raw-spec installs behind the console password (util/confirm.ts): a 7-day session cookie must not
// be enough to leave a persistent command behind on the machine.
//
// Wins over the `/api/**` catch-all by h3 route specificity. GET is not gated — reading the current
// automation is ordinary console business.
import { defineEventHandler, readBody } from "h3";
import { confirmPassword } from "../../../util/confirm";
import { forwardJson } from "../../../util/forward";

interface HooksBody {
	hooks?: unknown[];
	password?: string;
}

export default defineEventHandler(async (event) => {
	const body = await readBody<HooksBody>(event);
	confirmPassword(event, body?.password);
	// Rebuild from the one field the host takes, so the password cannot leak upstream.
	return forwardJson(event, "/api/v1/hooks", "PUT", {
		hooks: Array.isArray(body?.hooks) ? body.hooks : [],
	});
});

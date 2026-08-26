// POST /api/v1/pair/pin — the GameStream lane's half of the same decision: delivering the PIN a
// Moonlight client is showing completes its pairing handshake, and the paired client then has
// keyboard and mouse on the host desktop. Anyone holding a session cookie can point their own
// Moonlight at the host and read the PIN off their own screen, so this is gated on the console
// password exactly like the native approve route (util/confirm.ts).
//
// Wins over the `/api/**` catch-all by h3 route specificity. Only reachable when the host runs the
// compat planes (`--gamestream`, off by default).
import { defineEventHandler, readBody } from "h3";
import { confirmPassword } from "../../../../util/confirm";
import { forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const body = await readBody<{ pin?: string; password?: string }>(event);
	confirmPassword(event, body?.password);
	// Rebuild from the one field the host takes, so the password cannot leak upstream.
	return forwardJson(event, "/api/v1/pair/pin", "POST", {
		pin: String(body?.pin ?? ""),
	});
});

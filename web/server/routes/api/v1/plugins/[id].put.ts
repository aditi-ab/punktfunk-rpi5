// PUT /api/v1/plugins/{id} — a plugin registers itself, with its own token, straight to the host.
// The console never does, so it answers here and never relays the call with the admin bearer.
// Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, setResponseStatus } from "h3";

export default defineEventHandler((event) => {
	setResponseStatus(event, 403);
	return {
		error:
			"The console can't register a plugin. Install it from the Plugins page instead.",
	};
});

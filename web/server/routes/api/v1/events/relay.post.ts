// POST /api/v1/events/relay — the box's door hands its own events to the owner's host, with that
// host's token. The console never does, so it answers here and never relays the call with the
// admin bearer. Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, setResponseStatus } from "h3";

export default defineEventHandler((event) => {
	setResponseStatus(event, 403);
	return {
		error:
			"Only the box's door relays events. The console has none to put on the bus.",
	};
});

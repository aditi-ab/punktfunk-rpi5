// GET/PUT /api/plugin-game/<id>?entry=<library id> — a plugin's section on one library entry's
// page (`/__game`), read server-side over loopback like `plugin-config`, so no plugin markup or
// secret reaches the console origin. A save grants the folders the operator typed into it and lets
// go of the ones taken out.
import {
	defineEventHandler,
	getQuery,
	getRouterParam,
	setResponseStatus,
} from "h3";
import { PLUGIN_ID_RE, validEntryId } from "../../../util/pluginProxy";
import { pluginSurface } from "../../../util/pluginSurface";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id");
	const { entry } = getQuery(event);
	if (!id || !PLUGIN_ID_RE.test(id) || !validEntryId(entry)) {
		setResponseStatus(event, 400);
		return { error: "not a valid plugin or library id" };
	}
	// A 404 is the plugin having nothing for this entry: the page shows no tab.
	return pluginSurface(
		event,
		id,
		`/__game?entry=${encodeURIComponent(entry)}`,
		{
			methods: ["GET", "PUT"],
			grantForm: `game:${entry}`,
			notFound: {
				error: "plugin has no section for this entry",
				noSection: true,
			},
		},
	);
});

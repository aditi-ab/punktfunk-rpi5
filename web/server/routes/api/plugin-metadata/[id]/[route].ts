// /api/plugin-metadata/<id>/<route> — an Art & Metadata source's `/__metadata/<route>` (status,
// match, search, images), read server-side over loopback like `plugin-game`, so no plugin markup or
// secret reaches the console origin. A pick is stored by the host, not through here.
import {
	defineEventHandler,
	getQuery,
	getRouterParam,
	setResponseStatus,
} from "h3";
import { PLUGIN_ID_RE } from "../../../../util/pluginProxy";
import { pluginSurface } from "../../../../util/pluginSurface";

/** The routes the kit serves, and their methods. Nothing else is forwarded. */
const ROUTES: Record<string, ReadonlyArray<"GET" | "PUT" | "POST">> = {
	status: ["GET"],
	match: ["GET", "PUT"],
	search: ["POST"],
	images: ["GET"],
};

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id");
	const route = getRouterParam(event, "route");
	const methods = route && Object.hasOwn(ROUTES, route) ? ROUTES[route] : null;
	if (!id || !PLUGIN_ID_RE.test(id) || !methods) {
		setResponseStatus(event, 400);
		return { error: "not a valid plugin id or route" };
	}
	const query = new URLSearchParams();
	const { entry, kind } = getQuery(event);
	if (typeof entry === "string") query.set("entry", entry);
	if (typeof kind === "string") query.set("kind", kind);
	const qs = query.toString();
	return pluginSurface(event, id, `/__metadata/${route}${qs ? `?${qs}` : ""}`, {
		methods,
	});
});

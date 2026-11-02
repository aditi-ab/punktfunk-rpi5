import { expect, test } from "bun:test";
import type { PluginAccessSnapshot } from "@/api/gen/model/pluginAccessSnapshot";
import { nextAsk, putOff } from "./PluginAccess";

const row = (plugin: string, ...paths: string[]) =>
	({
		plugin,
		grants: [],
		denied: [],
		pending: paths.map((path) => ({ path, write: false })),
	}) as unknown as PluginAccessSnapshot;

test("the prompt asks for a request until Later puts it off, then the next", () => {
	const rows = [row("steam", "/mnt/a", "/mnt/b"), row("gog", "/games")];
	expect(nextAsk(rows, [])?.plugin).toBe("steam");
	const later = putOff(rows, [], rows[0]);
	expect(later).toEqual(["steam\n/mnt/a", "steam\n/mnt/b"]);
	expect(nextAsk(rows, later)?.plugin).toBe("gog");
	// A new folder from the same plugin asks again.
	expect(nextAsk([row("steam", "/mnt/a", "/mnt/c")], later)?.plugin).toBe(
		"steam",
	);
});

test("Later forgets requests that are no longer pending", () => {
	const rows = [row("gog", "/games")];
	expect(putOff(rows, ["steam\n/mnt/a"], rows[0])).toEqual(["gog\n/games"]);
});

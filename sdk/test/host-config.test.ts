// The runner reads two files the host writes. `host-config/` holds a copy of each, written by the
// host's own tests (`the_grants_file_the_runner_reads_is_checked_in` in plugins/access.rs and
// `the_token_map_the_runner_reads_is_checked_in` in mgmt_token.rs), so a field the host renames
// fails here instead of granting nothing on a real box.
import { expect, test } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";
import { pluginToken } from "../src/runner-linux.js";
import { grantedRoots } from "../src/sandbox.js";

const HOST_CONFIG = path.join(import.meta.dir, "host-config");

test("the host writes the two files where the runner looks", () => {
	const files = fs.readdirSync(HOST_CONFIG, { recursive: true }) as string[];
	expect(files.sort()).toEqual([
		"plugin-run",
		path.join("plugin-run", "plugin-grants.json"),
		path.join("plugin-run", "plugin-tokens.json"),
	]);
});

test("every grant the host writes is one the sandbox binds", () => {
	const roots = grantedRoots(HOST_CONFIG, "demo");
	expect(roots.map(({ path, write, links }) => ({ path, write, links }))).toEqual([
		{
			path: "/mnt/roms",
			write: false,
			links: [{ path: "/home/u/Games", target: "/mnt/roms/games" }],
		},
		{ path: "/mnt/saves", write: true, links: undefined },
		{ path: "/mnt/old", write: false, links: undefined },
	]);
	expect(grantedRoots(HOST_CONFIG, "quiet")).toEqual([]);
});

test("a plugin finds its own token in the host's map", () => {
	expect(pluginToken(HOST_CONFIG, "demo")).toBe("sample-for-demo");
	expect(pluginToken(HOST_CONFIG, "rom-manager")).toBe("sample-for-rom-manager");
	expect(pluginToken(HOST_CONFIG, "absent")).toBeUndefined();
});
